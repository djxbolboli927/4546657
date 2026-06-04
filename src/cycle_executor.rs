//! Bridges arb_cycle profitable hits to Metis /swap-instructions + Jito submission.
//!
//! Flow per 2-hop CycleHit (WSOL → X → WSOL):
//!   1. Local filter: profit_gross >= min_profit_lamports  (before any Metis call)
//!   2. Metis /quote × 2 — strict mode: onlyDirectRoutes=true + DEX filter
//!      Metis is used ONLY for the routePlan (instruction bytes); its outAmount
//!      is logged for comparison but does NOT gate execution.
//!   3. merge_quotes with min_acceptable_out = amount_in + min_profit_lamports
//!   4. /swap-instructions
//!   5. build_arb_transaction → send to Jito (REST first, gRPC fallback)
//!
//! 3-hop support requires merge_quotes_3; arb_cycle filters those out before
//! forwarding so skip_hops should stay near zero.

use std::sync::{
    atomic::{AtomicI64, AtomicU64, Ordering::Relaxed},
    Arc, Mutex,
};
use std::time::Duration;

use solana_client::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use tokio::sync::mpsc;

use crate::alt_cache::AltCache;
use crate::arb_cycle::CycleHit;
use crate::blockhash_cache::BlockhashCache;
use crate::jito::JitoClient;
use crate::jito_grpc::JitoGrpcClient;
use crate::metis::MetisClient;
use crate::rate_limiter::RateLimiter;
use crate::tokens::WSOL_MINT;
use crate::transaction;

// ── Execution context ─────────────────────────────────────────────────────────

pub struct ExecutorCtx {
    pub metis: Arc<MetisClient>,
    pub jito: Arc<JitoClient>,
    pub jito_grpc: Option<Arc<JitoGrpcClient>>,
    pub jito_limiter: Arc<Mutex<RateLimiter>>,
    pub jito_grpc_limiter: Option<Arc<Mutex<RateLimiter>>>,
    pub trading_keypair: Arc<Keypair>,
    pub rpc_client: Arc<RpcClient>,
    pub blockhash_cache: Arc<BlockhashCache>,
    pub alt_cache: AltCache,
    pub cu_limits: Vec<u32>,
    pub user_pubkey: String,
    /// Minimum local profit (lamports) required before Metis call and on-chain floor.
    pub min_profit_lamports: u64,
    /// Jito tip lamports added to every transaction.
    pub tip_lamports: u64,
    pub base_fee_lamports: u64,
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ExecutorMetrics {
    pub cycles_received: AtomicU64,
    /// Hits skipped because hops != 2 (should be ~0 after arb_cycle filter).
    pub skipped_hops: AtomicU64,
    /// Dropped before Metis: local profit_gross < min_profit_lamports.
    pub local_filter_drop: AtomicU64,
    pub metis_calls: AtomicU64,
    /// Both /quote calls succeeded (routePlan obtained).
    pub metis_route_ok: AtomicU64,
    pub metis_error: AtomicU64,
    pub instructions_built: AtomicU64,
    pub swap_ix_error: AtomicU64,
    pub build_error: AtomicU64,
    pub jito_rest_sent: AtomicU64,
    pub jito_grpc_sent: AtomicU64,
    pub jito_error: AtomicU64,
    pub jito_rate_limited: AtomicU64,
    /// Sum of local profit_gross for all Jito-sent transactions (lamports).
    pub total_local_profit: AtomicI64,
}

// ── DEX label mapping ─────────────────────────────────────────────────────────

/// Map our internal DexKind name to the Metis/Jupiter API label used in the
/// `dexes` query parameter and in routePlan `swapInfo.label`.
fn dex_to_metis_label(name: &str) -> &'static str {
    match name {
        "RaydiumAmmV4"    => "Raydium",
        "RaydiumCpmm"     => "Raydium CPMM",
        "RaydiumClmm"     => "Raydium CLMM",
        "OrcaWhirlpoolV1" => "Whirlpool",
        "MeteoraDammV2"   => "Meteora DAMM v2",
        "MeteoraDlmm"     => "Meteora DLMM",
        "PumpSwap"        => "Pump.fun AMM",
        other             => {
            // Unknown — pass as-is so Metis can tell us it's unrecognised
            // rather than silently routing through a different DEX.
            // 'static safety: leak once per unknown DEX name (practically never).
            Box::leak(other.to_string().into_boxed_str())
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn spawn_cycle_executor(
    mut rx: mpsc::Receiver<CycleHit>,
    ctx: Arc<ExecutorCtx>,
) -> Arc<ExecutorMetrics> {
    let metrics = Arc::new(ExecutorMetrics::default());

    // Periodic reporter — every 30 s.
    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut last_built = 0u64;
            let mut last_rest  = 0u64;
            let mut last_grpc  = 0u64;
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let received  = m.cycles_received.load(Relaxed);
                let skip      = m.skipped_hops.load(Relaxed);
                let local_drp = m.local_filter_drop.load(Relaxed);
                let m_calls   = m.metis_calls.load(Relaxed);
                let m_ok      = m.metis_route_ok.load(Relaxed);
                let m_err     = m.metis_error.load(Relaxed);
                let built     = m.instructions_built.load(Relaxed);
                let ix_err    = m.swap_ix_error.load(Relaxed);
                let bld_err   = m.build_error.load(Relaxed);
                let rest_sent = m.jito_rest_sent.load(Relaxed);
                let grpc_sent = m.jito_grpc_sent.load(Relaxed);
                let j_err     = m.jito_error.load(Relaxed);
                let rate_lim  = m.jito_rate_limited.load(Relaxed);
                let tot_prof  = m.total_local_profit.load(Relaxed);
                let total_sent = rest_sent + grpc_sent;
                let d_built   = built - last_built;
                let d_rest    = rest_sent - last_rest;
                let d_grpc    = grpc_sent - last_grpc;
                let avg_prof  = if total_sent > 0 { tot_prof / total_sent as i64 } else { 0 };
                eprintln!(
                    "[executor] recv={received} skip_hops={skip} local_drop={local_drp} | \
metis_calls={m_calls} route_ok={m_ok} metis_err={m_err} | \
built={built}(+{d_built}) ix_err={ix_err} build_err={bld_err} | \
jito_rest={rest_sent}(+{d_rest}) jito_grpc={grpc_sent}(+{d_grpc}) \
jito_err={j_err} rate_lim={rate_lim} | \
avg_local_profit={avg_prof}L total_local={tot_prof}L"
                );
                last_built = built;
                last_rest  = rest_sent;
                last_grpc  = grpc_sent;
            }
        });
    }

    let m = metrics.clone();
    tokio::spawn(async move {
        while let Some(hit) = rx.recv().await {
            let ctx = ctx.clone();
            let m   = m.clone();
            tokio::spawn(async move { process_hit(hit, ctx, m).await });
        }
    });

    metrics
}

// ── Per-hit execution ─────────────────────────────────────────────────────────

async fn process_hit(hit: CycleHit, ctx: Arc<ExecutorCtx>, m: Arc<ExecutorMetrics>) {
    m.cycles_received.fetch_add(1, Relaxed);

    // Only 2-hop supported (arb_cycle already filters, but guard here too).
    if hit.hops() != 2 {
        m.skipped_hops.fetch_add(1, Relaxed);
        return;
    }

    // ── Local profit filter (before any Metis call) ───────────────────────────
    if hit.profit_gross < ctx.min_profit_lamports {
        m.local_filter_drop.fetch_add(1, Relaxed);
        return;
    }

    let x_mint    = hit.intermediate_mints[0].to_string();
    let amount_in = hit.amount_in;

    // ── Stage 1: Strict /quote × 2 ───────────────────────────────────────────
    // Use DEX-filtered direct routes so Metis stays close to the same pool our
    // local calculation used.  Metis outAmount is logged but NOT used to gate
    // execution — local math is the profitability authority.
    let dex1 = dex_to_metis_label(hit.dex_names[0]);
    let dex2 = dex_to_metis_label(hit.dex_names[1]);

    m.metis_calls.fetch_add(1, Relaxed);

    let q1 = match ctx.metis.get_quote_strict(WSOL_MINT, &x_mint, amount_in, &[dex1]).await {
        Ok(q) => q,
        Err(e) => {
            eprintln!(
                "[executor][q1_err] dex={dex1} WSOL→{x_mint} amount={amount_in} err={e}"
            );
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    let mid: u64 = q1.out_amount.parse().unwrap_or(0);
    if mid == 0 {
        eprintln!("[executor][q1_zero] dex={dex1} WSOL→{x_mint} amount={amount_in} q1.out=0");
        m.metis_error.fetch_add(1, Relaxed);
        return;
    }

    let q2 = match ctx.metis.get_quote_strict(&x_mint, WSOL_MINT, mid, &[dex2]).await {
        Ok(q) => q,
        Err(e) => {
            eprintln!(
                "[executor][q2_err] dex={dex2} {x_mint}→WSOL mid={mid} err={e}"
            );
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    let metis_out: u64 = q2.out_amount.parse().unwrap_or(0);
    let min_acceptable_out = amount_in.saturating_add(ctx.min_profit_lamports);

    // Log Metis round-trip result vs our local estimate vs required floor.
    eprintln!(
        "[executor][quote] dex={dex1}→{dex2} x={x_mint} in={amount_in} \
local_gross={}L metis_out={metis_out}L min_floor={min_acceptable_out}L pools={}→{}",
        hit.profit_gross,
        hit.pools[0],
        hit.pools[1],
    );

    m.metis_route_ok.fetch_add(1, Relaxed);

    // ── Stage 2: Merge + /swap-instructions ──────────────────────────────────
    let merged = match MetisClient::merge_quotes(&q1, &q2, min_acceptable_out) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("[executor][merge_err] in={amount_in} err={e}");
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    let swap_ixs = match ctx.metis.get_swap_instructions(&ctx.user_pubkey, &merged).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[executor][ix_err] dex={dex1}→{dex2} x={x_mint} in={amount_in} \
metis_out={metis_out} min_floor={min_acceptable_out} err={e:?}"
            );
            m.swap_ix_error.fetch_add(1, Relaxed);
            return;
        }
    };

    m.instructions_built.fetch_add(1, Relaxed);
    m.total_local_profit.fetch_add(hit.profit_gross as i64, Relaxed);

    // ── Stage 3: Build transaction ────────────────────────────────────────────
    let cu_idx   = hit.hops().saturating_sub(2);
    let cu_limit = ctx.cu_limits.get(cu_idx).copied()
        .unwrap_or_else(|| ctx.cu_limits.last().copied().unwrap_or(220_000));
    let blockhash = ctx.blockhash_cache.get();

    let tx = match transaction::build_arb_transaction(
        &swap_ixs,
        &ctx.trading_keypair,
        ctx.tip_lamports,
        cu_limit,
        blockhash,
        &ctx.alt_cache,
        &ctx.rpc_client,
    ) {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!(
                "[executor][build_err] dex={dex1}→{dex2} in={amount_in} cu={cu_limit} err={e}"
            );
            m.build_error.fetch_add(1, Relaxed);
            return;
        }
    };

    // ── Stage 4: Submit to Jito (REST → gRPC fallback) ────────────────────────
    let use_rest = ctx.jito_limiter.lock()
        .map(|mut l| l.try_acquire())
        .unwrap_or(false);

    if use_rest {
        match ctx.jito.send_bundle(&tx).await {
            Ok(bundle_id) => {
                eprintln!(
                    "[executor][rest_sent] bundle={bundle_id} dex={dex1}→{dex2} \
in={amount_in} local_gross={}L tip={}L",
                    hit.profit_gross, ctx.tip_lamports,
                );
                m.jito_rest_sent.fetch_add(1, Relaxed);
            }
            Err(e) => {
                eprintln!("[executor][rest_err] in={amount_in} err={e}");
                m.jito_error.fetch_add(1, Relaxed);
            }
        }
        return;
    }

    // REST rate-limit full — try gRPC.
    if let (Some(grpc), Some(gl)) = (&ctx.jito_grpc, &ctx.jito_grpc_limiter) {
        let use_grpc = gl.lock().map(|mut l| l.try_acquire()).unwrap_or(false);
        if use_grpc {
            match grpc.send_bundle(&tx).await {
                Ok(bundle_id) => {
                    eprintln!(
                        "[executor][grpc_sent] bundle={bundle_id} dex={dex1}→{dex2} \
in={amount_in} local_gross={}L tip={}L",
                        hit.profit_gross, ctx.tip_lamports,
                    );
                    m.jito_grpc_sent.fetch_add(1, Relaxed);
                }
                Err(e) => {
                    eprintln!("[executor][grpc_err] in={amount_in} err={e}");
                    m.jito_error.fetch_add(1, Relaxed);
                }
            }
            return;
        }
    }

    // Both limiters full — drop.
    m.jito_rate_limited.fetch_add(1, Relaxed);
}
