use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use tokio::sync::mpsc::Receiver;

use crate::{
    arb_cycle::CycleHit,
    config::ArbTestConfig,
    metis::{MetisClient, RouteHopInfo},
    tokens::WSOL_MINT,
};

// ── Metrics ────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ValidatorMetrics {
    pub hits_received: AtomicU64,
    pub hits_validated: AtomicU64,
    /// Hops where route_len==1, label matched, ammKey matched, mints matched.
    pub strict_match_count: AtomicU64,
    /// Hops where at least one strict criterion failed.
    pub route_mismatch_count: AtomicU64,
    /// /quote call errors (network, timeout, HTTP error).
    pub quote_errors: AtomicU64,
    /// Sum of (metis_out_strict - local_out) across all validated hops.
    pub total_delta_lamports: AtomicU64,
}

// ── DEX label mapping ──────────────────────────────────────────────────────────

fn dex_to_metis_label(name: &str) -> &'static str {
    match name {
        "RaydiumAmmV4"    => "Raydium",
        "RaydiumCpmm"     => "Raydium CPMM",
        "RaydiumClmm"     => "Raydium CLMM",
        "OrcaWhirlpoolV1" => "Whirlpool",
        "MeteoraDammV2"   => "Meteora DAMM v2",
        "MeteoraDlmm"     => "Meteora DLMM",
        "PumpSwap"        => "Pump.fun AMM",
        other             => Box::leak(other.to_string().into_boxed_str()),
    }
}

// ── Per-hop helpers ────────────────────────────────────────────────────────────

/// Mint going INTO hop `hop_idx`.
fn hop_mint_in(hit: &CycleHit, hop_idx: usize, wsol: &str) -> String {
    if hop_idx == 0 {
        wsol.to_string()
    } else {
        hit.intermediate_mints[hop_idx - 1].to_string()
    }
}

/// Mint coming OUT of hop `hop_idx`.
fn hop_mint_out(hit: &CycleHit, hop_idx: usize, wsol: &str) -> String {
    if hop_idx + 1 == hit.hops() {
        wsol.to_string()
    } else {
        hit.intermediate_mints[hop_idx].to_string()
    }
}

/// Token amount going INTO hop `hop_idx` (from local calculator).
fn hop_local_in(hit: &CycleHit, hop_idx: usize) -> u64 {
    if hop_idx == 0 {
        hit.amount_in
    } else {
        hit.intermediate_amounts.get(hop_idx - 1).copied().unwrap_or(0)
    }
}

/// Token amount coming OUT of hop `hop_idx` (from local calculator).
fn hop_local_out(hit: &CycleHit, hop_idx: usize) -> u64 {
    if hop_idx + 1 == hit.hops() {
        hit.amount_out
    } else {
        hit.intermediate_amounts.get(hop_idx).copied().unwrap_or(0)
    }
}

// ── Core validation logic ──────────────────────────────────────────────────────

struct HopResult {
    strict: bool,
    metis_out: Option<u64>,
}

async fn validate_hop(
    hit: &CycleHit,
    hop_idx: usize,
    hit_serial: u64,
    metis: &MetisClient,
    cfg: &ArbTestConfig,
) -> HopResult {
    let wsol = WSOL_MINT;
    let mint_in  = hop_mint_in(hit, hop_idx, wsol);
    let mint_out = hop_mint_out(hit, hop_idx, wsol);
    let local_in  = hop_local_in(hit, hop_idx);
    let local_out = hop_local_out(hit, hop_idx);

    let dex_name   = hit.dex_names[hop_idx];
    let pool_local = hit.pools[hop_idx].to_string();
    let expected_label = dex_to_metis_label(dex_name);

    let dexes: &[&str] = if cfg.strict_dex_filter {
        &[expected_label]
    } else {
        &[]
    };

    let quote_result = metis
        .get_quote_strict(&mint_in, &mint_out, local_in, dexes)
        .await;

    let quote = match quote_result {
        Ok(q) => q,
        Err(e) => {
            eprintln!(
                "[validate_hop] hit={hit_serial} hop={hop_idx} dex={dex_name} \
pool_local={pool_local} ERROR={e}"
            );
            return HopResult { strict: false, metis_out: None };
        }
    };

    let hop_info: RouteHopInfo = MetisClient::extract_first_hop(&quote);

    let amm_key_str  = hop_info.amm_key.as_deref().unwrap_or("?");
    let label_str    = hop_info.label.as_deref().unwrap_or("?");
    let h_mint_in    = hop_info.input_mint.as_deref().unwrap_or("?");
    let h_mint_out   = hop_info.output_mint.as_deref().unwrap_or("?");
    let metis_out    = hop_info.out_amount;
    let metis_in_amt = hop_info.in_amount.unwrap_or(0);

    let label_ok    = label_str == expected_label;
    let amm_ok      = !cfg.require_ammkey_match || amm_key_str == pool_local;
    let route_ok    = hop_info.route_len == 1;
    let mint_in_ok  = h_mint_in == mint_in;
    let mint_out_ok = h_mint_out == mint_out;

    let strict = label_ok && amm_ok && route_ok && mint_in_ok && mint_out_ok;

    let delta: i64 = match metis_out {
        Some(mo) => (mo as i64) - (local_out as i64),
        None     => 0,
    };
    let delta_bps: i64 = if local_out > 0 {
        delta * 10_000 / (local_out as i64)
    } else {
        0
    };

    eprintln!(
        "[validate_hop] hit={hit_serial} hop={hop_idx} dex={dex_name} \
pool_local={pool_local:.8} ammKey={amm_key_str:.8} label={label_str} \
route_len={} mint_in_ok={mint_in_ok} mint_out_ok={mint_out_ok} \
local_in={local_in} local_out={local_out} metis_in={metis_in_amt} \
metis_out={} delta={delta:+}L delta_bps={delta_bps:+} strict={strict}",
        hop_info.route_len,
        metis_out.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
    );

    HopResult { strict, metis_out }
}

async fn validate_local_hit(
    hit: &CycleHit,
    hit_serial: u64,
    metis: &MetisClient,
    cfg: &ArbTestConfig,
    metrics: &ValidatorMetrics,
) {
    let hops = hit.hops();

    // Gather per-hop results sequentially (each depends on local_in of previous hop).
    let mut hop_results: Vec<HopResult> = Vec::with_capacity(hops);
    for hop_idx in 0..hops {
        let r = validate_hop(hit, hop_idx, hit_serial, metis, cfg).await;
        hop_results.push(r);
    }

    // Compute metis gross using only strict hops' outAmounts.
    // We chain: metis_out of each hop feeds the next. For the cycle summary we
    // report the final hop's metis_out as the cycle output.
    let all_strict = hop_results.iter().all(|r| r.strict);
    let any_missing = hop_results.iter().any(|r| r.metis_out.is_none());
    let route_mismatch = hop_results.iter().any(|r| !r.strict);

    let metis_final_out: Option<u64> = if any_missing {
        None
    } else {
        hop_results.last().and_then(|r| r.metis_out)
    };

    let metis_gross_strict: i64 = match metis_final_out {
        Some(mo) => (mo as i64) - (hit.amount_in as i64),
        None     => i64::MIN,
    };

    let cycle_delta: i64 = match metis_final_out {
        Some(mo) => (mo as i64) - (hit.amount_out as i64),
        None     => i64::MIN,
    };

    let mints_str: Vec<String> = hit.intermediate_mints.iter()
        .map(|m| m.to_string()[..8].to_string())
        .collect();
    let via = if mints_str.is_empty() { "direct".to_string() } else { mints_str.join("→") };

    eprintln!(
        "[validate_cycle] hit={hit_serial} hops={hops} via={via} \
in={} local_out={} local_gross={:+} \
metis_out_strict={} metis_gross_strict={:+} \
cycle_delta={:+} all_strict={all_strict} route_mismatch={route_mismatch}",
        hit.amount_in,
        hit.amount_out,
        hit.profit_gross as i64,
        metis_final_out.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
        if metis_gross_strict == i64::MIN { "?".to_string() } else { format!("{metis_gross_strict:+}") },
        if cycle_delta == i64::MIN { "?".to_string() } else { format!("{cycle_delta:+}") },
    );

    metrics.hits_validated.fetch_add(1, Ordering::Relaxed);
    if all_strict {
        metrics.strict_match_count.fetch_add(1, Ordering::Relaxed);
    }
    if route_mismatch {
        metrics.route_mismatch_count.fetch_add(1, Ordering::Relaxed);
    }
    let errors = hop_results.iter().filter(|r| r.metis_out.is_none()).count();
    metrics.quote_errors.fetch_add(errors as u64, Ordering::Relaxed);
}

// ── Spawn ──────────────────────────────────────────────────────────────────────

pub fn spawn_arb_validator(
    mut rx: Receiver<CycleHit>,
    metis: Arc<MetisClient>,
    cfg: ArbTestConfig,
) -> Arc<ValidatorMetrics> {
    let metrics = Arc::new(ValidatorMetrics::default());
    let metrics_ret = metrics.clone();

    tokio::spawn(async move {
        let mut hit_serial: u64 = 0;
        eprintln!("[arb_validator] started — validate_local=true");

        while let Some(hit) = rx.recv().await {
            hit_serial += 1;
            let received = metrics.hits_received.fetch_add(1, Ordering::Relaxed) + 1;

            let hops = hit.hops();
            // Optionally skip 3-hop validation.
            if hops == 3 && !cfg.enable_3hop_validation {
                eprintln!(
                    "[arb_validator] hit={hit_serial} skip 3-hop (enable_3hop_validation=false)"
                );
                continue;
            }

            if received % 50 == 0 {
                let validated   = metrics.hits_validated.load(Ordering::Relaxed);
                let strict      = metrics.strict_match_count.load(Ordering::Relaxed);
                let mismatches  = metrics.route_mismatch_count.load(Ordering::Relaxed);
                let q_errors    = metrics.quote_errors.load(Ordering::Relaxed);
                eprintln!(
                    "[arb_validator] stats recv={received} validated={validated} \
strict={strict} mismatch={mismatches} quote_err={q_errors}"
                );
            }

            validate_local_hit(&hit, hit_serial, &metis, &cfg, &metrics).await;
        }

        eprintln!("[arb_validator] channel closed — exiting");
    });

    metrics_ret
}
