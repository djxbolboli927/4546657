//! Bridges arb_cycle profitable hits to Metis /swap-instructions + Jito submission.
//!
//! Flow per 2-hop CycleHit:
//!   1. Local filter: profit_net >= min_profit_lamports (done before send)
//!   2. Metis /quote × 2 (WSOL→X, X→WSOL)
//!   3. Check Metis out >= amount_in + min_profit_lamports
//!   4. merge_quotes with min_acceptable_out = amount_in + tip + base_fee
//!   5. /swap-instructions
//!   6. build_arb_transaction → send to Jito (REST, gRPC fallback)
//!
//! 3-hop cycles are skipped for now: merge_quotes only handles a single
//! intermediate token. Implement 3-hop support when it appears consistently
//! in top opportunities.

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
    /// Minimum local (and Metis) profit in lamports before calling /swap-instructions.
    pub min_profit_lamports: u64,
    /// Jito tip lamports added to every transaction (system transfer to tip account).
    pub tip_lamports: u64,
    /// Base network fee in lamports (one signature = 5000).
    pub base_fee_lamports: u64,
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ExecutorMetrics {
    /// Hits received from arb_cycle channel.
    pub cycles_received: AtomicU64,
    /// 3-hop (and higher) hits skipped — not implemented yet.
    pub skipped_hops: AtomicU64,
    /// Hits dropped because local profit_gross < min_profit_lamports.
    pub local_filter_drop: AtomicU64,
    /// Metis /quote pair calls initiated.
    pub metis_calls: AtomicU64,
    /// Both /quote calls succeeded (valid route obtained from Metis).
    pub metis_route_ok: AtomicU64,
    /// Metis /quote call failed (network, timeout, parse error).
    pub metis_error: AtomicU64,
    /// /swap-instructions successfully obtained.
    pub instructions_built: AtomicU64,
    /// /swap-instructions call failed.
    pub swap_ix_error: AtomicU64,
    /// Transaction build failed (overflow, size, etc.).
    pub build_error: AtomicU64,
    /// Bundles accepted by Jito REST.
    pub jito_rest_sent: AtomicU64,
    /// Bundles accepted by Jito gRPC.
    pub jito_grpc_sent: AtomicU64,
    /// Jito send returned an error.
    pub jito_error: AtomicU64,
    /// Both rate limiters were full — bundle dropped.
    pub jito_rate_limited: AtomicU64,
    /// Sum of local profit_gross for all jito-sent transactions (lamports).
    pub total_local_profit: AtomicI64,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawn the cycle executor.  Returns the metrics handle for inspection.
///
/// The executor drains `rx`, spawning one async task per hit to keep the
/// channel from backing up even when Metis is slow.  The channel should be
/// sized to at most a few scan cycles' worth of hits (e.g. 200).
pub fn spawn_cycle_executor(
    mut rx: mpsc::Receiver<CycleHit>,
    ctx: Arc<ExecutorCtx>,
) -> Arc<ExecutorMetrics> {
    let metrics = Arc::new(ExecutorMetrics::default());

    // Periodic reporter — prints every 30 s without interrupting hot path.
    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut last_built = 0u64;
            let mut last_rest  = 0u64;
            let mut last_grpc  = 0u64;
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.tick().await; // skip the immediate first tick
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

    // Only 2-hop: WSOL → X → WSOL, handled via merge_quotes.
    if hit.hops() != 2 {
        m.skipped_hops.fetch_add(1, Relaxed);
        return;
    }

    // ── Local profit filter (before any Metis call) ───────────────────────────
    // profit_gross = amount_out − amount_in from our own on-chain math.
    // We trust this more than Metis's estimate; Metis is only used for the
    // instruction bytes, not for the profitability decision.
    if hit.profit_gross < ctx.min_profit_lamports {
        m.local_filter_drop.fetch_add(1, Relaxed);
        return;
    }

    let x_mint    = hit.intermediate_mints[0].to_string();
    let amount_in = hit.amount_in;

    // ── Stage 1: Two /quote calls — only for the routePlan, NOT profit check ──
    // We need q1 + q2 so merge_quotes can concatenate their routePlans into a
    // single circular instruction.  Metis's outAmount is ignored intentionally:
    // its math differs from our per-DEX formulas and would incorrectly filter
    // many valid opportunities.
    m.metis_calls.fetch_add(1, Relaxed);

    let q1 = match ctx.metis.get_quote(WSOL_MINT, &x_mint, amount_in, false).await {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!(err = %e, amount_in, "executor: WSOL→X quote failed");
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    let mid: u64 = q1.out_amount.parse().unwrap_or(0);
    if mid == 0 {
        m.metis_error.fetch_add(1, Relaxed);
        return;
    }

    let q2 = match ctx.metis.get_quote(&x_mint, WSOL_MINT, mid, false).await {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!(err = %e, mid, "executor: X→WSOL quote failed");
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    // Both routes obtained — no Metis profitability gate here.
    m.metis_route_ok.fetch_add(1, Relaxed);

    // ── Stage 2: Merge + /swap-instructions ──────────────────────────────────
    // min_acceptable_out = on-chain revert floor.
    // The transaction reverts if actual out < amount_in + min_profit_lamports.
    // This is the configurable threshold from config.toml, not a fixed constant.
    let min_acceptable_out = amount_in.saturating_add(ctx.min_profit_lamports);

    let merged = match MetisClient::merge_quotes(&q1, &q2, min_acceptable_out) {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!(err = %e, "executor: merge_quotes failed");
            m.metis_error.fetch_add(1, Relaxed);
            return;
        }
    };

    let swap_ixs = match ctx.metis.get_swap_instructions(&ctx.user_pubkey, &merged).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(?e, "executor: /swap-instructions failed");
            m.swap_ix_error.fetch_add(1, Relaxed);
            return;
        }
    };

    m.instructions_built.fetch_add(1, Relaxed);
    m.total_local_profit.fetch_add(hit.profit_gross as i64, Relaxed);

    // ── Stage 3: Build transaction ────────────────────────────────────────────
    let cu_idx    = hit.hops().saturating_sub(2);
    let cu_limit  = ctx.cu_limits.get(cu_idx).copied()
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
            tracing::debug!(err = %e, "executor: build_arb_transaction failed");
            m.build_error.fetch_add(1, Relaxed);
            return;
        }
    };

    // ── Stage 4: Submit to Jito ───────────────────────────────────────────────
    // Try REST first (5/s), then gRPC fallback (5/s). Total capacity = 10/s.
    // Acquire the rate limiter lock BEFORE any await point.
    let use_rest = ctx.jito_limiter.lock()
        .map(|mut l| l.try_acquire())
        .unwrap_or(false);

    if use_rest {
        match ctx.jito.send_bundle(&tx).await {
            Ok(id) => {
                tracing::debug!(bundle_id = id, "executor: Jito REST sent");
                m.jito_rest_sent.fetch_add(1, Relaxed);
            }
            Err(e) => {
                tracing::debug!(err = %e, "executor: Jito REST error");
                m.jito_error.fetch_add(1, Relaxed);
            }
        }
        return;
    }

    // REST rate limit full — try gRPC.
    if let (Some(grpc), Some(gl)) = (&ctx.jito_grpc, &ctx.jito_grpc_limiter) {
        let use_grpc = gl.lock().map(|mut l| l.try_acquire()).unwrap_or(false);
        if use_grpc {
            match grpc.send_bundle(&tx).await {
                Ok(id) => {
                    tracing::debug!(bundle_id = id, "executor: Jito gRPC sent");
                    m.jito_grpc_sent.fetch_add(1, Relaxed);
                }
                Err(e) => {
                    tracing::debug!(err = %e, "executor: Jito gRPC error");
                    m.jito_error.fetch_add(1, Relaxed);
                }
            }
            return;
        }
    }

    // Both limiters full — drop.
    m.jito_rate_limited.fetch_add(1, Relaxed);
}
