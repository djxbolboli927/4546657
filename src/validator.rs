/// Price validator: compares on-chain spot prices (from PoolStateStore vault
/// balances) with Jupiter Price API V3 USD prices.
///
/// Runs on a configurable interval. When `[validation] enabled = true` in
/// config.toml, the main bot loop is skipped and this runs standalone so no
/// Metis calls or Jito bundles are sent.
///
/// Comparison formula:
///   local_spot = (reserve_b / reserve_a) × 10^(dec_a − dec_b)
///   jupiter     = usdPrice_A / usdPrice_B
///   diff_bps    = |local_spot − jupiter| / jupiter × 10_000
///
/// Jupiter Price API V3 returns `decimals` per mint so no separate RPC call
/// for mint decimals is needed.
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

/// Read (mint, amount) from a classic SPL Token account.
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

#[derive(Deserialize, Debug, Clone)]
struct JupPriceEntry {
    #[serde(rename = "usdPrice")]
    usd_price: f64,
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

    let mut out = HashMap::new();
    if let Some(obj) = raw.as_object() {
        for (k, v) in obj {
            if let (Ok(pk), Ok(entry)) = (
                k.parse::<Pubkey>(),
                serde_json::from_value::<JupPriceEntry>(v.clone()),
            ) {
                if entry.usd_price > 0.0 {
                    out.insert(pk, entry);
                }
            }
        }
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
) {
    // ── 1. Collect live vault data ───────────────────────────────────────────
    let mut live: Vec<LivePool> = Vec::new();
    let mut mint_set: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let mut mint_list: Vec<Pubkey> = Vec::new();

    for pair in pairs {
        let a_ref = match store.accounts.get(&pair.vault_a) {
            Some(r) => r,
            None => continue,
        };
        let b_ref = match store.accounts.get(&pair.vault_b) {
            Some(r) => r,
            None => continue,
        };

        let Some((mint_a, reserve_a)) = read_spl_token_account(&a_ref.data) else {
            continue;
        };
        let Some((mint_b, reserve_b)) = read_spl_token_account(&b_ref.data) else {
            continue;
        };

        if reserve_a == 0 || reserve_b == 0 {
            continue;
        }

        let slot = a_ref.slot.max(b_ref.slot);

        if mint_set.insert(mint_a) {
            mint_list.push(mint_a);
        }
        if mint_set.insert(mint_b) {
            mint_list.push(mint_b);
        }

        live.push(LivePool {
            pool: pair.pool,
            mint_a,
            mint_b,
            reserve_a,
            reserve_b,
            slot,
        });
    }

    if live.is_empty() {
        eprintln!(
            "[validator] waiting for state — subscribed_pools={} live=0",
            pairs.len()
        );
        return;
    }

    // ── 2. Fetch Jupiter prices (≤50 per request) ───────────────────────────
    let mut jup: HashMap<Pubkey, JupPriceEntry> = HashMap::new();
    for chunk in mint_list.chunks(50) {
        let batch = fetch_jupiter_prices(client, jup_cfg, chunk).await;
        jup.extend(batch);
    }

    // ── 3. Compare and log ───────────────────────────────────────────────────
    let mut diffs: Vec<(f64, String)> = Vec::new();
    let mut no_jup = 0usize;

    for d in &live {
        let (Some(pa), Some(pb)) = (jup.get(&d.mint_a), jup.get(&d.mint_b)) else {
            no_jup += 1;
            continue;
        };

        // Local spot price of A in B (real, decimal-adjusted):
        //   = (reserve_b / reserve_a) × 10^(dec_a − dec_b)
        let dec_adj = 10f64.powi(pa.decimals as i32 - pb.decimals as i32);
        let local = (d.reserve_b as f64 / d.reserve_a as f64) * dec_adj;

        // Jupiter mid-price of A in B
        let jup_price = pa.usd_price / pb.usd_price;

        let diff_bps = ((local - jup_price) / jup_price).abs() * 10_000.0;

        diffs.push((
            diff_bps,
            format!(
                "pool={p} ra={ra} rb={rb} local={lo:.6} jup={jp:.6} diff={d:.1}bps slot={sl}",
                p = d.pool,
                ra = d.reserve_a,
                rb = d.reserve_b,
                lo = local,
                jp = jup_price,
                d = diff_bps,
                sl = d.slot,
            ),
        ));
    }

    // Sort worst-first so the user sees the largest discrepancies first
    diffs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let avg_bps = if !diffs.is_empty() {
        diffs.iter().map(|(d, _)| d).sum::<f64>() / diffs.len() as f64
    } else {
        0.0
    };
    let max_bps = diffs.first().map(|(d, _)| *d).unwrap_or(0.0);

    eprintln!(
        "[validator] pools_live={live} compared={ok} no_jup={no_jup} avg={avg:.1}bps max={max:.1}bps",
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

/// Spawn the periodic price-validation background task.
///
/// Waits two full intervals before the first run to give the Yellowstone
/// stream time to populate the PoolStateStore.
pub fn spawn_validator(
    pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    val_cfg: ValidationConfig,
    jup_cfg: JupiterPriceConfig,
) {
    eprintln!(
        "[validator] started — {pairs} vault pairs, interval={}s, max_log={}",
        val_cfg.interval_secs,
        val_cfg.max_pools_log,
        pairs = pairs.len(),
    );

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");

        let wait = Duration::from_secs(val_cfg.interval_secs);
        tokio::time::sleep(wait * 2).await; // let Yellowstone fill the store

        loop {
            run_cycle(&pairs, &store, &client, &jup_cfg, val_cfg.max_pools_log).await;
            tokio::time::sleep(wait).await;
        }
    });
}
