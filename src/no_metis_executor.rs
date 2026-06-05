//! No-Metis executor: builds native DEX instructions from CycleHit data and
//! submits them directly to Jito without any Metis/Jupiter involvement.
//!
//! Flow per CycleHit:
//!   1. Filter: skip unsupported hop counts or oversized amounts.
//!   2. Build one native swap Instruction per hop via native_ix::build_swap().
//!      → On unsupported DEX: log [no_metis_skip] reason=missing_native_ix_builder.
//!   3. Assemble V0 transaction: compute_budget + swap_ixs + jito_tip.
//!   4. Reject if serialized size > 1232 bytes.
//!   5. Optional RPC simulation (simulate_first=true).
//!   6. Send to Jito (REST first, gRPC fallback).

use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc, Mutex,
};
use std::time::Duration;

use rand::seq::SliceRandom;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use tokio::sync::mpsc;

use crate::{
    arb_cycle::CycleHit,
    blockhash_cache::BlockhashCache,
    config::NoMetisConfig,
    jito::JitoClient,
    jito_grpc::JitoGrpcClient,
    native_ix,
    pool_state_store::PoolStateStore,
    rate_limiter::RateLimiter,
    tokens::WSOL_MINT,
};

// ── Jito tip accounts ─────────────────────────────────────────────────────────

const TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

// ── Context ───────────────────────────────────────────────────────────────────

pub struct NoMetisCtx {
    pub jito: Arc<JitoClient>,
    pub jito_grpc: Option<Arc<JitoGrpcClient>>,
    pub jito_limiter: Arc<Mutex<RateLimiter>>,
    pub jito_grpc_limiter: Option<Arc<Mutex<RateLimiter>>>,
    pub trading_keypair: Arc<Keypair>,
    pub rpc_client: Arc<RpcClient>,
    pub blockhash_cache: Arc<BlockhashCache>,
    pub store: Arc<PoolStateStore>,
    pub cfg: NoMetisConfig,
    pub cu_limit: u32,
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct NoMetisMetrics {
    pub received: AtomicU64,
    pub skipped_hops: AtomicU64,
    pub skipped_amount: AtomicU64,
    pub skipped_no_builder: AtomicU64,
    pub skipped_tx_too_large: AtomicU64,
    pub sim_ok: AtomicU64,
    pub sim_err: AtomicU64,
    pub jito_rest_sent: AtomicU64,
    pub jito_grpc_sent: AtomicU64,
    pub jito_rate_limited: AtomicU64,
    pub jito_error: AtomicU64,
}

// ── Instruction helpers ────────────────────────────────────────────────────────

fn compute_budget_ix(cu_limit: u32) -> Instruction {
    Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: {
            let mut d = vec![0x02u8]; // SetComputeUnitLimit
            d.extend_from_slice(&cu_limit.to_le_bytes());
            d
        },
    }
}

fn jito_tip_ix(payer: &Pubkey, tip_lamports: u64) -> Instruction {
    let mut rng = rand::thread_rng();
    let addr = TIP_ACCOUNTS.choose(&mut rng).expect("tip accounts non-empty");
    let tip_account: Pubkey = addr.parse().expect("valid tip pubkey");
    #[allow(deprecated)]
    system_instruction::transfer(payer, &tip_account, tip_lamports)
}

// ── Per-hit processing ────────────────────────────────────────────────────────

async fn process_hit(hit: CycleHit, ctx: Arc<NoMetisCtx>, m: Arc<NoMetisMetrics>) {
    let hops = hit.hops();
    let cfg = &ctx.cfg;

    // Hop count filter.
    if hops < 2 || hops > 3 {
        m.skipped_hops.fetch_add(1, Relaxed);
        return;
    }
    if hops == 3 && !cfg.enable_3hop {
        m.skipped_hops.fetch_add(1, Relaxed);
        return;
    }

    // Amount cap — avoid sending large amounts during the test phase.
    if hit.amount_in > cfg.max_amount_lamports {
        m.skipped_amount.fetch_add(1, Relaxed);
        eprintln!(
            "[no_metis_skip] reason=amount_too_large amount={} max={}",
            hit.amount_in, cfg.max_amount_lamports,
        );
        return;
    }

    let wsol = Pubkey::from_str_const(WSOL_MINT);
    let user = ctx.trading_keypair.pubkey();

    // Build one native instruction per hop.
    let mut swap_ixs: Vec<Instruction> = Vec::with_capacity(hops);
    for hop_idx in 0..hops {
        let mint_in: Pubkey = if hop_idx == 0 {
            wsol
        } else {
            hit.intermediate_mints[hop_idx - 1]
        };
        let mint_out: Pubkey = if hop_idx + 1 == hops {
            wsol
        } else {
            hit.intermediate_mints[hop_idx]
        };
        let amount_in: u64 = if hop_idx == 0 {
            hit.amount_in
        } else {
            hit.intermediate_amounts.get(hop_idx - 1).copied().unwrap_or(0)
        };
        // Permissive min_out: 1 for intermediate, no-principal-loss for final hop.
        let min_out: u64 = if hop_idx + 1 == hops { hit.amount_in } else { 1 };

        let dex_name = hit.dex_names[hop_idx];
        let pool = &hit.pools[hop_idx];

        eprintln!(
            "[no_metis_hop] hop={hop_idx} dex={dex_name} pool={:.8} \
mint_in={:.8} mint_out={:.8} amount_in={amount_in} min_out={min_out}",
            pool,
            mint_in,
            mint_out,
        );

        match native_ix::build_swap(dex_name, pool, &user, &mint_in, &mint_out, amount_in, min_out, &ctx.store) {
            Ok(ix) => swap_ixs.push(ix),
            Err(e) => {
                m.skipped_no_builder.fetch_add(1, Relaxed);
                eprintln!("[no_metis_skip] reason={e} hop={hop_idx} dex={dex_name}");
                return;
            }
        }
    }

    // Assemble full instruction list and build V0 transaction (no ALTs).
    let blockhash = ctx.blockhash_cache.get();
    let mut all_ixs: Vec<Instruction> = Vec::with_capacity(hops + 2);
    all_ixs.push(compute_budget_ix(ctx.cu_limit));
    all_ixs.extend(swap_ixs);
    all_ixs.push(jito_tip_ix(&user, cfg.tip_lamports));

    let msg = match v0::Message::try_compile(&user, &all_ixs, &[], blockhash) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[no_metis_skip] reason=msg_compile_failed err={e}");
            return;
        }
    };
    let tx = match VersionedTransaction::try_new(VersionedMessage::V0(msg), &[ctx.trading_keypair.as_ref()]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[no_metis_skip] reason=sign_failed err={e}");
            return;
        }
    };

    // Size check: Solana max serialized transaction = 1232 bytes.
    let tx_bytes = match bincode::serialize(&tx) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[no_metis_skip] reason=serialize_failed err={e}");
            return;
        }
    };
    if tx_bytes.len() > 1232 {
        m.skipped_tx_too_large.fetch_add(1, Relaxed);
        eprintln!(
            "[no_metis_skip] reason=tx_too_large size={} hops={hops}",
            tx_bytes.len()
        );
        return;
    }

    let pool_labels: Vec<String> = hit.pools.iter()
        .map(|p| p.to_string()[..8].to_string())
        .collect();
    eprintln!(
        "[no_metis_candidate] hops={hops} amount_in={} profit_gross={} \
pools={} tx_bytes={} tip={}",
        hit.amount_in,
        hit.profit_gross,
        pool_labels.join("→"),
        tx_bytes.len(),
        cfg.tip_lamports,
    );

    // Optional RPC simulation.
    if cfg.simulate_first {
        match ctx.rpc_client.simulate_transaction(&tx) {
            Ok(resp) => {
                if let Some(err) = resp.value.err {
                    m.sim_err.fetch_add(1, Relaxed);
                    eprintln!(
                        "[no_metis_sim] FAILED hops={hops} amount_in={} err={err:?} logs={:?}",
                        hit.amount_in,
                        resp.value.logs.as_deref().unwrap_or(&[]),
                    );
                    return;
                }
                m.sim_ok.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_sim] OK hops={hops} amount_in={} units={}",
                    hit.amount_in,
                    resp.value.units_consumed.unwrap_or(0),
                );
            }
            Err(e) => {
                m.sim_err.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_sim] RPC_ERROR hops={hops} amount_in={} err={e}",
                    hit.amount_in,
                );
                return;
            }
        }
    }

    // ── Submit to Jito (REST first, gRPC fallback) ────────────────────────────
    let use_rest = ctx
        .jito_limiter
        .lock()
        .map(|mut l| l.try_acquire())
        .unwrap_or(false);

    if use_rest {
        match ctx.jito.send_bundle(&tx).await {
            Ok(bundle_id) => {
                m.jito_rest_sent.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_sent] via=REST bundle={bundle_id} hops={hops} \
amount_in={} profit_gross={} tip={}",
                    hit.amount_in, hit.profit_gross, cfg.tip_lamports,
                );
            }
            Err(e) => {
                m.jito_error.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_status] REST_ERROR hops={hops} amount_in={} err={e}",
                    hit.amount_in,
                );
            }
        }
        return;
    }

    // REST limiter full — try gRPC.
    if let (Some(grpc), Some(gl)) = (&ctx.jito_grpc, &ctx.jito_grpc_limiter) {
        let use_grpc = gl.lock().map(|mut l| l.try_acquire()).unwrap_or(false);
        if use_grpc {
            match grpc.send_bundle(&tx).await {
                Ok(bundle_id) => {
                    m.jito_grpc_sent.fetch_add(1, Relaxed);
                    eprintln!(
                        "[no_metis_sent] via=gRPC bundle={bundle_id} hops={hops} \
amount_in={} profit_gross={} tip={}",
                        hit.amount_in, hit.profit_gross, cfg.tip_lamports,
                    );
                }
                Err(e) => {
                    m.jito_error.fetch_add(1, Relaxed);
                    eprintln!(
                        "[no_metis_status] gRPC_ERROR hops={hops} amount_in={} err={e}",
                        hit.amount_in,
                    );
                }
            }
            return;
        }
    }

    m.jito_rate_limited.fetch_add(1, Relaxed);
    eprintln!(
        "[no_metis_status] RATE_LIMITED hops={hops} amount_in={}",
        hit.amount_in,
    );
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

pub fn spawn_no_metis_executor(
    mut rx: mpsc::Receiver<CycleHit>,
    ctx: Arc<NoMetisCtx>,
) -> Arc<NoMetisMetrics> {
    let metrics = Arc::new(NoMetisMetrics::default());

    // Periodic stats reporter.
    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                eprintln!(
                    "[no_metis] recv={} skip_hops={} skip_amt={} no_builder={} \
tx_large={} sim_ok={} sim_err={} rest_sent={} grpc_sent={} rate_lim={} jito_err={}",
                    m.received.load(Relaxed),
                    m.skipped_hops.load(Relaxed),
                    m.skipped_amount.load(Relaxed),
                    m.skipped_no_builder.load(Relaxed),
                    m.skipped_tx_too_large.load(Relaxed),
                    m.sim_ok.load(Relaxed),
                    m.sim_err.load(Relaxed),
                    m.jito_rest_sent.load(Relaxed),
                    m.jito_grpc_sent.load(Relaxed),
                    m.jito_rate_limited.load(Relaxed),
                    m.jito_error.load(Relaxed),
                );
            }
        });
    }

    let m = metrics.clone();
    tokio::spawn(async move {
        eprintln!(
            "[no_metis_executor] started — simulate={} tip={}L max_amount={}L enable_3hop={}",
            ctx.cfg.simulate_first,
            ctx.cfg.tip_lamports,
            ctx.cfg.max_amount_lamports,
            ctx.cfg.enable_3hop,
        );

        while let Some(hit) = rx.recv().await {
            m.received.fetch_add(1, Relaxed);
            let ctx = ctx.clone();
            let m = m.clone();
            tokio::spawn(async move { process_hit(hit, ctx, m).await });
        }

        eprintln!("[no_metis_executor] channel closed — exiting");
    });

    metrics
}
