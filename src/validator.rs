/// Price validator: compares on-chain spot prices (from PoolStateStore vault
/// balances) with Jupiter Price API V3 USD prices.
///
/// Comparison formula:
///   local_spot = (reserve_b / reserve_a) × 10^(dec_a − dec_b)
///   jupiter     = usdPrice_A / usdPrice_B
///   diff_bps    = |local_spot − jupiter| / jupiter × 10_000
///
/// Set VALIDATOR_DEBUG=1 to print per-pool mint+price details for diagnosing
/// formula issues.
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use crate::config::{JupiterPriceConfig, ValidationConfig};
use crate::pool_state_store::PoolStateStore;
use crate::pool_state_stream::PoolVaultPair;

// ── SPL Token account decoder ─────────────────────────────────────────────────

/// Read (mint, amount) from a standard SPL Token account.
///
/// Fixed layout:
///   [0..32]  mint    Pubkey
///   [32..64] owner   Pubkey
///   [64..72] amount  u64 LE
fn read_spl_token_account(data: &[u8]) -> Option<(Pubkey, u64)> {
    if data.len() < 72 {
        return None;
    }
    let mint = Pubkey::from(<[u8; 32]>::try_from(&data[0..32]).ok()?);
    let amount = u64::from_le_bytes(<[u8; 8]>::try_from(&data[64..72]).ok()?);
    Some((mint, amount))
}

// ── Jupiter Price API ─────────────────────────────────────────────────────────

/// Jupiter Price API V3 response entry.
/// The API returns `usdPrice` (price of 1 real token in USD) and `decimals`
/// (the token's SPL mint decimals, e.g. SOL=9, USDC=6).
#[derive(Deserialize, Debug, Clone)]
struct JupPriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
    /// SPL token decimals as returned by Jupiter.
    /// Used to convert raw reserve amounts to real token amounts.
    #[serde(default)]
    decimals: u8,
}

async fn fetch_jupiter_prices(
    client: &reqwest::Client,
    cfg: &JupiterPriceConfig,
    mints: &[Pubkey],
) -> HashMap<Pubkey, JupPriceEntry> {
    if mints.is_empty() {
        return HashMap::new();
    }

    let ids = mints
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut req = client.get(&cfg.url).query(&[("ids", &ids)]);
    if !cfg.api_key.is_empty() {
        req = req.header("x-api-key", &cfg.api_key);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Jupiter price request failed");
            return HashMap::new();
        }
    };

    let raw: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Jupiter price response parse failed");
            return HashMap::new();
        }
    };

    // Jupiter Price API V3 can return either a flat object OR a "data"-wrapped
    // object. Try the flat case first; if no pubkeys parse, try .data.
    let top = raw.as_object();
    let flat_count = top
        .map(|o| o.keys().filter(|k| k.parse::<Pubkey>().is_ok()).count())
        .unwrap_or(0);

    let source = if flat_count > 0 {
        raw.as_object().unwrap()
    } else if let Some(data) = raw.get("data").and_then(|v| v.as_object()) {
        data
    } else {
        warn!("Jupiter response has no recognisable structure: {:?}", &raw.to_string()[..200.min(raw.to_string().len())]);
        return HashMap::new();
    };

    let mut out = HashMap::new();
    for (k, v) in source {
        if let (Ok(pk), Ok(entry)) = (
            k.parse::<Pubkey>(),
            serde_json::from_value::<JupPriceEntry>(v.clone()),
        ) {
            if entry.usd_price > 0.0 {
                out.insert(pk, entry);
            }
        }
    }

    if out.is_empty() {
        // Emit raw response (first 400 chars) so the user can diagnose format.
        let raw_str = raw.to_string();
        warn!(
            preview = &raw_str[..400.min(raw_str.len())],
            "Jupiter returned 0 parseable entries — check API format"
        );
    }

    out
}

// ── Validation cycle ──────────────────────────────────────────────────────────

struct LivePool {
    pool: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    reserve_a: u64,
    reserve_b: u64,
    slot: u64,
}

async fn run_cycle(
    pairs: &[PoolVaultPair],
    store: &PoolStateStore,
    client: &reqwest::Client,
    jup_cfg: &JupiterPriceConfig,
    max_log: usize,
    debug: bool,
) {
    // ── 1. Read live vault balances ──────────────────────────────────────────
    let mut live: Vec<LivePool> = Vec::new();
    let mut mint_set: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let mut mint_list: Vec<Pubkey> = Vec::new();

    for pair in pairs {
        let a_data = {
            let r = match store.accounts.get(&pair.vault_a) { Some(r) => r, None => continue };
            r.data.clone()
        };
        let b_data = {
            let r = match store.accounts.get(&pair.vault_b) { Some(r) => r, None => continue };
            r.data.clone()
        };
        let slot = {
            let ra = store.accounts.get(&pair.vault_a).map(|r| r.slot).unwrap_or(0);
            let rb = store.accounts.get(&pair.vault_b).map(|r| r.slot).unwrap_or(0);
            ra.max(rb)
        };

        let Some((mint_a, reserve_a)) = read_spl_token_account(&a_data) else { continue };
        let Some((mint_b, reserve_b)) = read_spl_token_account(&b_data) else { continue };

        if reserve_a == 0 || reserve_b == 0 {
            continue;
        }

        if mint_set.insert(mint_a) { mint_list.push(mint_a); }
        if mint_set.insert(mint_b) { mint_list.push(mint_b); }

        live.push(LivePool { pool: pair.pool, mint_a, mint_b, reserve_a, reserve_b, slot });
    }

    if live.is_empty() {
        eprintln!(
            "[validator] waiting for state — subscribed_pools={} live=0",
            pairs.len()
        );
        return;
    }

    // ── 2. Fetch Jupiter prices in batches of ≤50 ────────────────────────────
    let mut jup: HashMap<Pubkey, JupPriceEntry> = HashMap::new();
    for chunk in mint_list.chunks(50) {
        let batch = fetch_jupiter_prices(client, jup_cfg, chunk).await;
        if debug && batch.is_empty() {
            eprintln!("[validator:debug] Jupiter returned empty batch for {} mints", chunk.len());
        }
        jup.extend(batch);
    }

    // ── 3. Compare ───────────────────────────────────────────────────────────
    let mut diffs: Vec<(f64, String)> = Vec::new();
    let mut no_jup = 0usize;

    for d in &live {
        let (Some(pa), Some(pb)) = (jup.get(&d.mint_a), jup.get(&d.mint_b)) else {
            if debug {
                eprintln!(
                    "[validator:debug] pool={} mint_a={} mint_b={} — no Jupiter price",
                    d.pool, d.mint_a, d.mint_b,
                );
            }
            no_jup += 1;
            continue;
        };

        if debug {
            eprintln!(
                "[validator:debug] pool={} \
mint_a={} usd_a={:.6} dec_a={} \
mint_b={} usd_b={:.6} dec_b={} \
ra={} rb={}",
                d.pool,
                d.mint_a, pa.usd_price, pa.decimals,
                d.mint_b, pb.usd_price, pb.decimals,
                d.reserve_a, d.reserve_b,
            );
        }

        // Price of 1 real token_A in terms of real token_B:
        //   local = (reserve_b_raw / reserve_a_raw) × 10^(dec_a − dec_b)
        //   where dec_a = pa.decimals, dec_b = pb.decimals
        //
        // Jupiter: usdPrice_A / usdPrice_B gives the same ratio.
        let dec_adj = 10f64.powi(pa.decimals as i32 - pb.decimals as i32);
        let local = (d.reserve_b as f64 / d.reserve_a as f64) * dec_adj;
        let jup_price = pa.usd_price / pb.usd_price;

        let diff_bps = ((local - jup_price) / jup_price).abs() * 10_000.0;

        diffs.push((
            diff_bps,
            format!(
                "pool={p} ra={ra} rb={rb} \
local={lo:.6} jup={jp:.6} dec_adj=1e{da:+} diff={d:.1}bps slot={sl}",
                p = d.pool,
                ra = d.reserve_a,
                rb = d.reserve_b,
                lo = local,
                jp = jup_price,
                da = pa.decimals as i32 - pb.decimals as i32,
                d = diff_bps,
                sl = d.slot,
            ),
        ));
    }

    // Sort worst-first
    diffs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let avg_bps = if !diffs.is_empty() {
        diffs.iter().map(|(d, _)| d).sum::<f64>() / diffs.len() as f64
    } else {
        0.0
    };
    let max_bps = diffs.first().map(|(d, _)| *d).unwrap_or(0.0);

    eprintln!(
        "[validator] pools_live={live} compared={ok} no_jup={no_jup} \
avg={avg:.1}bps max={max:.1}bps",
        live = live.len(),
        ok = diffs.len(),
        no_jup = no_jup,
        avg = avg_bps,
        max = max_bps,
    );
    for (_, line) in diffs.iter().take(max_log) {
        eprintln!("[validator]   {line}");
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn spawn_validator(
    pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    val_cfg: ValidationConfig,
    jup_cfg: JupiterPriceConfig,
) {
    let debug = std::env::var("VALIDATOR_DEBUG")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    eprintln!(
        "[validator] started — {n} vault pairs, interval={s}s, max_log={m}, debug={debug}",
        n = pairs.len(),
        s = val_cfg.interval_secs,
        m = val_cfg.max_pools_log,
    );

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");

        let wait = Duration::from_secs(val_cfg.interval_secs);
        tokio::time::sleep(wait * 2).await;

        loop {
            run_cycle(
                &pairs,
                &store,
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
