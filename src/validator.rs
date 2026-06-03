/// Price validator — a WEAK sanity check, not a correctness oracle.
///
/// It compares the on-chain constant-product spot price of a pool (from live
/// vault balances in PoolStateStore) against Jupiter's USD price *ratio* for
/// the same two mints. Jupiter Price API gives a general USD mid-price, NOT a
/// quote for this specific pool, so a non-zero diff is expected and only large
/// diffs are interesting. The field is named `jup_price_ratio` to make that
/// explicit.
///
/// Scope (deliberately narrow per design): ONLY pools owned by Raydium AMM v4
/// or Raydium CPMM are checked — both are pure x*y=k. CLMM / DLMM / Whirlpool /
/// DAMM v2 / orderbook pools are SKIPPED here because reserve_b/reserve_a is
/// not their price (that was the source of the earlier ~900000 bps garbage).
///
/// Token decimals come from the real on-chain mint accounts (RPC, cached), NOT
/// from Jupiter.
///
/// Set VALIDATOR_DEBUG=1 for per-pool detail.
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use crate::config::{JupiterPriceConfig, ValidationConfig};
use crate::dex::{raydium_amm_v4, raydium_cpmm};
use crate::pool_state_store::PoolStateStore;
use crate::pool_state_stream::PoolVaultPair;

// ── DEX classification ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    RaydiumAmmV4,
    RaydiumCpmm,
}

impl Engine {
    fn label(&self) -> &'static str {
        match self {
            Engine::RaydiumAmmV4 => "ray_amm_v4",
            Engine::RaydiumCpmm => "ray_cpmm",
        }
    }
}

/// Classify a pool by its owner program id. Returns None for DEXes this
/// validator deliberately skips (everything that isn't pure constant product).
fn classify(owner: &Pubkey) -> Option<Engine> {
    if *owner == raydium_amm_v4::PROGRAM_ID {
        Some(Engine::RaydiumAmmV4)
    } else if *owner == raydium_cpmm::PROGRAM_ID {
        Some(Engine::RaydiumCpmm)
    } else {
        None
    }
}

// ── SPL account decoders ───────────────────────────────────────────────────────

/// (mint, amount) from a standard SPL Token account.
fn read_spl_token_account(data: &[u8]) -> Option<(Pubkey, u64)> {
    if data.len() < 72 {
        return None;
    }
    let mint = Pubkey::from(<[u8; 32]>::try_from(&data[0..32]).ok()?);
    let amount = u64::from_le_bytes(<[u8; 8]>::try_from(&data[64..72]).ok()?);
    Some((mint, amount))
}

/// Decimals from an SPL Mint account. Layout: COption mint_authority (36) +
/// supply u64 (8) → decimals u8 at offset 44.
fn read_mint_decimals(data: &[u8]) -> Option<u8> {
    data.get(44).copied()
}

// ── Jupiter Price API ───────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
struct JupPriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
}

async fn fetch_jupiter_prices(
    client: &reqwest::Client,
    cfg: &JupiterPriceConfig,
    mints: &[Pubkey],
) -> HashMap<Pubkey, f64> {
    if mints.is_empty() {
        return HashMap::new();
    }
    let ids = mints.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(",");

    let mut req = client.get(&cfg.url).query(&[("ids", &ids)]);
    if !cfg.api_key.is_empty() {
        req = req.header("x-api-key", &cfg.api_key);
    }

    let raw: serde_json::Value = match req.send().await {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Jupiter price parse failed");
                return HashMap::new();
            }
        },
        Err(e) => {
            warn!(error = %e, "Jupiter price request failed");
            return HashMap::new();
        }
    };

    // Accept both a flat {mint: {...}} object and a {"data": {mint: {...}}} wrap.
    let source = match raw.as_object() {
        Some(o) if o.keys().any(|k| k.parse::<Pubkey>().is_ok()) => o.clone(),
        _ => raw
            .get("data")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default(),
    };

    let mut out = HashMap::new();
    for (k, v) in source {
        if let (Ok(pk), Ok(entry)) =
            (k.parse::<Pubkey>(), serde_json::from_value::<JupPriceEntry>(v))
        {
            if entry.usd_price > 0.0 {
                out.insert(pk, entry.usd_price);
            }
        }
    }
    out
}

// ── Decimals cache (real mint accounts via RPC) ────────────────────────────────

/// Fetch and cache decimals for any mints not already known. Uses a blocking
/// RPC call on a blocking thread so the async runtime is never stalled.
async fn ensure_decimals(
    rpc: &Arc<RpcClient>,
    cache: &Arc<dashmap::DashMap<Pubkey, u8>>,
    mints: &[Pubkey],
) {
    let missing: Vec<Pubkey> = mints
        .iter()
        .filter(|m| !cache.contains_key(*m))
        .copied()
        .collect();
    if missing.is_empty() {
        return;
    }

    let rpc = rpc.clone();
    let cache = cache.clone();
    // getMultipleAccounts caps at 100 per call.
    let _ = tokio::task::spawn_blocking(move || {
        for chunk in missing.chunks(100) {
            if let Ok(accts) = rpc.get_multiple_accounts(chunk) {
                for (mint, maybe) in chunk.iter().zip(accts) {
                    if let Some(acc) = maybe {
                        if let Some(dec) = read_mint_decimals(&acc.data) {
                            cache.insert(*mint, dec);
                        }
                    }
                }
            }
        }
    })
    .await;
}

// ── Validation cycle ────────────────────────────────────────────────────────────

struct LivePool {
    pool: Pubkey,
    engine: Engine,
    mint_a: Pubkey,
    mint_b: Pubkey,
    reserve_a: u64,
    reserve_b: u64,
    slot: u64,
}

#[allow(clippy::too_many_arguments)]
async fn run_cycle(
    pairs: &[PoolVaultPair],
    store: &PoolStateStore,
    rpc: &Arc<RpcClient>,
    decimals: &Arc<dashmap::DashMap<Pubkey, u8>>,
    client: &reqwest::Client,
    jup_cfg: &JupiterPriceConfig,
    max_log: usize,
    debug: bool,
) {
    // ── 1. Read live vaults for Raydium AMM v4 / CPMM pools only ─────────────
    let mut live: Vec<LivePool> = Vec::new();
    let mut mint_set: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let mut mint_list: Vec<Pubkey> = Vec::new();
    let mut skipped_dex = 0usize;

    for pair in pairs {
        // Classify by mix.json owner; fall back to the live account's owner.
        let owner = if pair.owner != Pubkey::default() {
            pair.owner
        } else {
            match store.accounts.get(&pair.pool) {
                Some(r) => r.owner,
                None => continue,
            }
        };
        let engine = match classify(&owner) {
            Some(e) => e,
            None => {
                skipped_dex += 1;
                continue;
            }
        };

        let a_data = match store.accounts.get(&pair.vault_a) {
            Some(r) => r.data.clone(),
            None => continue,
        };
        let b_data = match store.accounts.get(&pair.vault_b) {
            Some(r) => r.data.clone(),
            None => continue,
        };
        let slot = store.accounts.get(&pair.vault_a).map(|r| r.slot).unwrap_or(0).max(
            store.accounts.get(&pair.vault_b).map(|r| r.slot).unwrap_or(0),
        );

        let Some((mint_a, reserve_a)) = read_spl_token_account(&a_data) else { continue };
        let Some((mint_b, reserve_b)) = read_spl_token_account(&b_data) else { continue };
        if reserve_a == 0 || reserve_b == 0 {
            continue;
        }

        if mint_set.insert(mint_a) { mint_list.push(mint_a); }
        if mint_set.insert(mint_b) { mint_list.push(mint_b); }

        live.push(LivePool {
            pool: pair.pool,
            engine,
            mint_a,
            mint_b,
            reserve_a,
            reserve_b,
            slot,
        });
    }

    if live.is_empty() {
        eprintln!(
            "[validator] waiting for state — subscribed_pools={} live=0 skipped_non_cp={}",
            pairs.len(),
            skipped_dex
        );
        return;
    }

    // ── 2. Decimals from real mints (cached) + Jupiter USD prices ────────────
    ensure_decimals(rpc, decimals, &mint_list).await;

    let mut jup: HashMap<Pubkey, f64> = HashMap::new();
    for chunk in mint_list.chunks(50) {
        jup.extend(fetch_jupiter_prices(client, jup_cfg, chunk).await);
    }

    // ── 3. Compare local spot vs jup_price_ratio ─────────────────────────────
    let mut diffs: Vec<(f64, String)> = Vec::new();
    let mut no_jup = 0usize;
    let mut no_dec = 0usize;

    for d in &live {
        let (Some(dec_a), Some(dec_b)) = (
            decimals.get(&d.mint_a).map(|r| *r),
            decimals.get(&d.mint_b).map(|r| *r),
        ) else {
            no_dec += 1;
            continue;
        };
        let (Some(usd_a), Some(usd_b)) = (jup.get(&d.mint_a), jup.get(&d.mint_b)) else {
            no_jup += 1;
            continue;
        };

        // On-chain constant-product spot price of A in B (real units):
        //   spot = (reserve_b / reserve_a) × 10^(dec_a − dec_b)
        let dec_adj = 10f64.powi(dec_a as i32 - dec_b as i32);
        let local = (d.reserve_b as f64 / d.reserve_a as f64) * dec_adj;
        // Jupiter USD price ratio (NOT a pool quote — weak reference only).
        let jup_price_ratio = usd_a / usd_b;

        let diff_bps = ((local - jup_price_ratio) / jup_price_ratio).abs() * 10_000.0;

        diffs.push((
            diff_bps,
            format!(
                "[{eng}] pool={p} ra={ra} rb={rb} dec_a={da} dec_b={db} \
local={lo:.8} jup_price_ratio={jp:.8} diff={d:.1}bps slot={sl}",
                eng = d.engine.label(),
                p = d.pool,
                ra = d.reserve_a,
                rb = d.reserve_b,
                da = dec_a,
                db = dec_b,
                lo = local,
                jp = jup_price_ratio,
                d = diff_bps,
                sl = d.slot,
            ),
        ));
    }

    diffs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let avg = if diffs.is_empty() {
        0.0
    } else {
        diffs.iter().map(|(d, _)| d).sum::<f64>() / diffs.len() as f64
    };
    let max = diffs.first().map(|(d, _)| *d).unwrap_or(0.0);

    eprintln!(
        "[validator] cp_pools_live={live} compared={ok} skipped_non_cp={skip} \
no_jup={nj} no_decimals={nd} avg={avg:.1}bps max={max:.1}bps",
        live = live.len(),
        ok = diffs.len(),
        skip = skipped_dex,
        nj = no_jup,
        nd = no_dec,
        avg = avg,
        max = max,
    );
    let n = if debug { diffs.len() } else { max_log };
    for (_, line) in diffs.iter().take(n) {
        eprintln!("[validator]   {line}");
    }
}

// ── Public API ──────────────────────────────────────────────────────────────────

pub fn spawn_validator(
    pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    rpc: Arc<RpcClient>,
    val_cfg: ValidationConfig,
    jup_cfg: JupiterPriceConfig,
) {
    let debug = std::env::var("VALIDATOR_DEBUG")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let cp_pairs = pairs.len();
    eprintln!(
        "[validator] started — {cp_pairs} vault pairs (Raydium AMM v4/CPMM only), \
interval={s}s, max_log={m}, debug={debug}",
        s = val_cfg.interval_secs,
        m = val_cfg.max_pools_log,
    );

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        let decimals: Arc<dashmap::DashMap<Pubkey, u8>> = Arc::new(dashmap::DashMap::new());

        let wait = Duration::from_secs(val_cfg.interval_secs);
        tokio::time::sleep(wait * 2).await;

        loop {
            run_cycle(
                &pairs,
                &store,
                &rpc,
                &decimals,
                &client,
                &jup_cfg,
                val_cfg.max_pools_log,
                debug,
            )
            .await;
            tokio::time::sleep(wait).await;
        }
    });
}
