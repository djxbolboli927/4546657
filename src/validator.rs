/// Price validator — a WEAK sanity check, not a correctness oracle.
///
/// Compares local spot prices against Jupiter's USD price ratio for the same
/// two mints. Covers all supported pool types:
///
///   • Raydium AMM v4 / CPMM — x*y=k; spot price from vault reserves.
///   • Meteora DAMM v2 — Uniswap-v3-style; spot price from sqrt_price.
///   • Orca Whirlpool — CLMM; spot price from pool account sqrt_price.
///   • Raydium CLMM — CLMM; spot price from pool account sqrt_price.
///   • Meteora DLMM — bin-based; spot price from active_id + bin_step.
///   • PumpSwap — x*y=k; spot price from vault reserves.
///
/// NOTE: spot price here is NOT the same as an exact-in quote. The validator
/// only checks that local price roughly agrees with Jupiter's USD mid-price.
/// Profit calculation lives in arb_cycle.rs, which uses quote_edge for
/// exact-in simulation. A large validator diff is a sanity alert, not a
/// signal to trade.
///
/// Token decimals come from the real on-chain mint accounts (RPC, cached), NOT
/// from Jupiter. Jupiter Price API gives a general USD mid-price, not a quote
/// for any specific pool, so non-zero diffs are expected and only large diffs
/// are interesting. The field is named `jup_price_ratio` to make that explicit.
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
use crate::dex::{
    meteora_damm_v2, meteora_dlmm, pumpswap, raydium_amm_v4, raydium_clmm, raydium_cpmm, whirlpool,
};
use crate::pool_state_store::PoolStateStore;
use crate::pool_state_stream::PoolVaultPair;

// ── DEX classification ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    RaydiumAmmV4,
    RaydiumCpmm,
    /// Meteora DAMM v2 — priced from the pool account's `sqrt_price`, not from
    /// vault reserves. Included so its prices can be checked against Jupiter.
    MeteoraDammV2,
    /// Orca Whirlpool — priced from pool account sqrt_price (Q64.64).
    OrcaWhirlpool,
    /// Raydium CLMM — priced from pool account sqrt_price (Q64.64).
    RaydiumClmm,
    /// PumpSwap — priced from vault reserves (constant-product, 30 bps fee).
    PumpSwap,
    /// Meteora DLMM — bin-based; spot price comes from the active bin
    /// (`(1 + bin_step/10000)^active_id`).
    MeteoraDlmm,
}

impl Engine {
    fn label(&self) -> &'static str {
        match self {
            Engine::RaydiumAmmV4 => "ray_amm_v4",
            Engine::RaydiumCpmm => "ray_cpmm",
            Engine::MeteoraDammV2 => "mtr_damm_v2",
            Engine::OrcaWhirlpool => "orca_wpool",
            Engine::RaydiumClmm => "ray_clmm",
            Engine::PumpSwap => "pumpswap",
            Engine::MeteoraDlmm => "mtr_dlmm",
        }
    }
}

/// Classify a pool by its owner program id. Returns None for unsupported DEX
/// programs (e.g. orderbooks, exotic AMMs not yet wired up).
fn classify(owner: &Pubkey) -> Option<Engine> {
    if *owner == raydium_amm_v4::PROGRAM_ID {
        Some(Engine::RaydiumAmmV4)
    } else if *owner == raydium_cpmm::PROGRAM_ID {
        Some(Engine::RaydiumCpmm)
    } else if *owner == meteora_damm_v2::PROGRAM_ID {
        Some(Engine::MeteoraDammV2)
    } else if *owner == whirlpool::PROGRAM_ID {
        Some(Engine::OrcaWhirlpool)
    } else if *owner == raydium_clmm::PROGRAM_ID {
        Some(Engine::RaydiumClmm)
    } else if *owner == pumpswap::PROGRAM_ID {
        Some(Engine::PumpSwap)
    } else if *owner == meteora_dlmm::PROGRAM_ID {
        Some(Engine::MeteoraDlmm)
    } else {
        None
    }
}

/// Q64.64 scale as f64 (2^64). DAMM v2 spot price of token A in token B
/// (atomic units) is `(sqrt_price / 2^64)^2`.
const Q64_F64: f64 = 18_446_744_073_709_551_616.0;

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
    /// Spot price of token A in token B, atomic units (B per A). For CP pools
    /// this is `reserve_b / reserve_a`; for DAMM v2 it is `(sqrt_price/2^64)^2`.
    spot_atomic: f64,
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
    // ── 1. Collect live pools — CP pools from vaults, DAMM v2 from pool account ─
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

        // ── CLMM pools (DAMM v2, Whirlpool) — price from pool account ────────
        if engine == Engine::OrcaWhirlpool {
            let Some(acc) = store.accounts.get(&pair.pool) else { continue };
            let Some(pool) = whirlpool::parse_pool(&acc.data) else { continue };
            // spot price of token A in token B, in atomic units:
            //   price = (sqrt_price / 2^64)^2
            let sp = pool.sqrt_price as f64 / Q64_F64;
            let spot_atomic = sp * sp;
            let slot = acc.slot;
            let (reserve_a, reserve_b) = {
                let ra = store.accounts.get(&pair.vault_a)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                let rb = store.accounts.get(&pair.vault_b)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                (ra, rb)
            };
            if mint_set.insert(pool.token_mint_a) { mint_list.push(pool.token_mint_a); }
            if mint_set.insert(pool.token_mint_b) { mint_list.push(pool.token_mint_b); }
            live.push(LivePool {
                pool: pair.pool,
                engine,
                mint_a: pool.token_mint_a,
                mint_b: pool.token_mint_b,
                reserve_a,
                reserve_b,
                spot_atomic,
                slot,
            });
            continue;
        }

        if engine == Engine::MeteoraDammV2 {
            let Some(acc) = store.accounts.get(&pair.pool) else { continue };
            let Some(pool) = meteora_damm_v2::parse_pool(&acc.data) else { continue };
            if !pool.is_supported() { continue; }
            // spot price of token A in token B, in atomic units:
            //   price = (sqrt_price / 2^64)^2
            let sp = pool.sqrt_price as f64 / Q64_F64;
            let spot_atomic = sp * sp;
            let slot = acc.slot;
            // Also read vaults for the reserve display in the log — optional,
            // silently skipped when not yet live.
            let (reserve_a, reserve_b) = {
                let ra = store.accounts.get(&pair.vault_a)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                let rb = store.accounts.get(&pair.vault_b)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                (ra, rb)
            };
            if mint_set.insert(pool.token_a_mint) { mint_list.push(pool.token_a_mint); }
            if mint_set.insert(pool.token_b_mint) { mint_list.push(pool.token_b_mint); }
            live.push(LivePool {
                pool: pair.pool,
                engine,
                mint_a: pool.token_a_mint,
                mint_b: pool.token_b_mint,
                reserve_a,
                reserve_b,
                spot_atomic,
                slot,
            });
            continue;
        }

        if engine == Engine::RaydiumClmm {
            let Some(acc) = store.accounts.get(&pair.pool) else { continue };
            let Some(pool) = raydium_clmm::parse_pool(&acc.data) else { continue };
            let sp = pool.sqrt_price_x64 as f64 / Q64_F64;
            let spot_atomic = sp * sp;
            let slot = acc.slot;
            let (reserve_a, reserve_b) = {
                let ra = store.accounts.get(&pool.token_vault_0)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                let rb = store.accounts.get(&pool.token_vault_1)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                (ra, rb)
            };
            if mint_set.insert(pool.token_mint_0) { mint_list.push(pool.token_mint_0); }
            if mint_set.insert(pool.token_mint_1) { mint_list.push(pool.token_mint_1); }
            live.push(LivePool {
                pool: pair.pool,
                engine,
                mint_a: pool.token_mint_0,
                mint_b: pool.token_mint_1,
                reserve_a,
                reserve_b,
                spot_atomic,
                slot,
            });
            continue;
        }

        if engine == Engine::PumpSwap {
            let Some(pool_acc) = store.accounts.get(&pair.pool) else { continue };
            let Some(pool) = pumpswap::parse_pool(&pool_acc.data) else { continue };
            let slot = pool_acc.slot;
            let Some(base_data) = store.accounts.get(&pool.base_vault) else { continue };
            let Some(quote_data) = store.accounts.get(&pool.quote_vault) else { continue };
            let Some((_, reserve_a)) = read_spl_token_account(&base_data.data) else { continue };
            let Some((_, reserve_b)) = read_spl_token_account(&quote_data.data) else { continue };
            if reserve_a == 0 || reserve_b == 0 { continue; }
            let spot_atomic = reserve_b as f64 / reserve_a as f64;
            if mint_set.insert(pool.base_mint) { mint_list.push(pool.base_mint); }
            if mint_set.insert(pool.quote_mint) { mint_list.push(pool.quote_mint); }
            live.push(LivePool {
                pool: pair.pool,
                engine,
                mint_a: pool.base_mint,
                mint_b: pool.quote_mint,
                reserve_a,
                reserve_b,
                spot_atomic,
                slot,
            });
            continue;
        }

        if engine == Engine::MeteoraDlmm {
            let Some(acc) = store.accounts.get(&pair.pool) else { continue };
            let Some(pool) = meteora_dlmm::parse_pool(&acc.data) else { continue };
            if !pool.is_supported() { continue; }
            // Active-bin spot price of token X in token Y (atomic), Q64.64 → f64.
            let Some(price_q64) = meteora_dlmm::get_price_from_id(pool.active_id, pool.bin_step)
            else { continue };
            let spot_atomic = price_q64 as f64 / Q64_F64;
            let slot = acc.slot;
            let (reserve_a, reserve_b) = {
                let ra = store.accounts.get(&pool.reserve_x)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                let rb = store.accounts.get(&pool.reserve_y)
                    .and_then(|r| read_spl_token_account(&r.data).map(|(_, a)| a))
                    .unwrap_or(0);
                (ra, rb)
            };
            if mint_set.insert(pool.token_x_mint) { mint_list.push(pool.token_x_mint); }
            if mint_set.insert(pool.token_y_mint) { mint_list.push(pool.token_y_mint); }
            live.push(LivePool {
                pool: pair.pool,
                engine,
                mint_a: pool.token_x_mint,
                mint_b: pool.token_y_mint,
                reserve_a,
                reserve_b,
                spot_atomic,
                slot,
            });
            continue;
        }

        // ── Raydium AMM v4 / CPMM — price from vault reserves ─────────────────
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
        let spot_atomic = reserve_b as f64 / reserve_a as f64;

        if mint_set.insert(mint_a) { mint_list.push(mint_a); }
        if mint_set.insert(mint_b) { mint_list.push(mint_b); }

        live.push(LivePool {
            pool: pair.pool,
            engine,
            mint_a,
            mint_b,
            reserve_a,
            reserve_b,
            spot_atomic,
            slot,
        });
    }

    if live.is_empty() {
        eprintln!(
            "[validator] waiting for state — subscribed_pools={} live=0 skipped_unsupported={}",
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

        // Spot price of A in B, adjusted for decimal difference → real units.
        // CP pools: spot_atomic = reserve_b / reserve_a.
        // DAMM v2:  spot_atomic = (sqrt_price / 2^64)^2.
        // Both give B-per-A in atomic units; multiply by 10^(dec_a-dec_b) → real.
        let dec_adj = 10f64.powi(dec_a as i32 - dec_b as i32);
        let local = d.spot_atomic * dec_adj;
        // Jupiter USD price ratio (NOT a pool quote — weak reference only).
        let jup_price_ratio = usd_a / usd_b;

        let diff_bps = ((local - jup_price_ratio) / jup_price_ratio).abs() * 10_000.0;

        diffs.push((
            diff_bps,
            format!(
                "[{eng}] pool={p} ra={ra} rb={rb} dec_a={da} dec_b={db} \
spot_atomic={sa:.8} local={lo:.8} jup_ratio={jp:.8} diff={d:.1}bps slot={sl}",
                eng = d.engine.label(),
                p = d.pool,
                ra = d.reserve_a,
                rb = d.reserve_b,
                da = dec_a,
                db = dec_b,
                sa = d.spot_atomic,
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
    let min = diffs.last().map(|(d, _)| *d).unwrap_or(0.0);

    // Count pool accounts not updated in the last 2 minutes (stale = no on-chain trades).
    // Growing stale_count explains divergence from Jupiter: unchanged pools have correct
    // prices but Jupiter may reflect newer trades in other sources.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let stale_2m = live.iter().filter(|d| {
        store.accounts.get(&d.pool)
            .map(|r| now_ns.saturating_sub(r.updated_at_unix_ns) > 120_000_000_000)
            .unwrap_or(false)
    }).count();

    let dmm_live = live.iter().filter(|d| d.engine == Engine::MeteoraDammV2).count();
    let wpool_live = live.iter().filter(|d| d.engine == Engine::OrcaWhirlpool).count();
    let rclmm_live = live.iter().filter(|d| d.engine == Engine::RaydiumClmm).count();
    let pump_live = live.iter().filter(|d| d.engine == Engine::PumpSwap).count();
    let dlmm_live = live.iter().filter(|d| d.engine == Engine::MeteoraDlmm).count();
    let cp_live = live.len() - dmm_live - wpool_live - rclmm_live - pump_live - dlmm_live;
    eprintln!(
        "[validator] pools_live={live} (cp={cp} damm_v2={dmm} whirlpool={wp} ray_clmm={rc} pumpswap={ps} dlmm={dl}) \
compared={ok} skipped_unsupported={skip} no_jup={nj} no_decimals={nd} \
avg={avg:.1}bps max={max:.1}bps min={min:.1}bps stale_2m={stale_2m}",
        live = live.len(),
        cp = cp_live,
        dmm = dmm_live,
        wp = wpool_live,
        rc = rclmm_live,
        ps = pump_live,
        dl = dlmm_live,
        ok = diffs.len(),
        skip = skipped_dex,
        nj = no_jup,
        nd = no_dec,
        avg = avg,
        max = max,
        min = min,
    );
    // Individual pool lines: only in VALIDATOR_DEBUG=1 mode or when max_pools_log > 0.
    if debug {
        for (_, line) in diffs.iter() {
            eprintln!("[validator]   {line}");
        }
    } else if max_log > 0 {
        for (_, line) in diffs.iter().take(max_log) {
            eprintln!("[validator]   {line}");
        }
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

    let total_pairs = pairs.len();
    eprintln!(
        "[validator] started — {total_pairs} pool pairs \
(Raydium AMM v4/CPMM/CLMM, Meteora DAMM v2/DLMM, Orca Whirlpool, PumpSwap), \
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
