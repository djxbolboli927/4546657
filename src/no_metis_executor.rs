//! No-Metis executor: builds native DEX instructions, validates via LiteSVM,
//! and sends matching transactions directly to Jito.
//!
//! Flow per 2-hop CycleHit:
//!   1. Skip if not exactly 2 hops (3-hop disabled for now — too large for 1232-byte limit).
//!   2. Skip if both hops are from the same DEX family (not real arb).
//!   3. Skip if amount_in > max_amount_lamports.
//!   4. Build one native Instruction per hop via native_ix::build_swap().
//!      → On unsupported DEX: log [no_metis_skip] reason=missing_native_ix_builder.
//!   5. Assemble V0 transaction: compute_budget + swap_ixs + jito_tip.
//!   6. Reject if serialized size > 1232 bytes → [no_metis_skip] reason=tx_too_large.
//!   7. Simulate via LiteSVM (if sim_pool is available).
//!      → Record wsol delta vs RAM-calculated profit_gross.
//!      → On revert: log [no_metis_sim] ok=false, do NOT send.
//!   8. Send to Jito if sim succeeded and delta within match_threshold.
//!      → REST first, gRPC fallback.

use std::sync::{
    atomic::{AtomicI64, AtomicU64, Ordering::Relaxed},
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
    account_cache::AccountCache,
    arb_cycle::CycleHit,
    blockhash_cache::BlockhashCache,
    config::NoMetisConfig,
    jito::JitoClient,
    jito_grpc::JitoGrpcClient,
    litesvm_sim::SimulatorPool,
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

// ── DEX family ────────────────────────────────────────────────────────────────

fn dex_family(name: &str) -> &str {
    match name {
        "RaydiumAmmV4" | "RaydiumCpmm" | "RaydiumClmm" => "Raydium",
        "OrcaWhirlpoolV1" => "Orca",
        "MeteoraDammV2" | "MeteoraDlmm" => "Meteora",
        "PumpSwap" => "PumpFun",
        other => other,
    }
}

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
    /// LiteSVM simulation pool (None if .so files unavailable).
    pub sim_pool: Option<Arc<SimulatorPool>>,
    /// Account cache fed to LiteSVM for simulation.
    pub sim_cache: Option<Arc<AccountCache>>,
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct NoMetisMetrics {
    pub received: AtomicU64,
    pub skipped_hops: AtomicU64,
    pub skipped_same_family: AtomicU64,
    pub skipped_amount: AtomicU64,
    pub skipped_no_builder: AtomicU64,
    pub skipped_tx_too_large: AtomicU64,
    /// LiteSVM simulation succeeded (tx did not revert).
    pub sim_ok: AtomicU64,
    /// LiteSVM simulation reverted.
    pub sim_revert: AtomicU64,
    /// Sim succeeded but delta > match_threshold → not sent.
    pub sim_mismatch: AtomicU64,
    /// Sum of |delta| for avg calculation (lamports).
    pub sum_delta_abs: AtomicI64,
    pub delta_count: AtomicU64,
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

/// Build a valid-but-empty SPL Token account for simulation injection.
/// Intermediate-hop ATAs for the user's wallet don't appear in the Yellowstone
/// cache (which filters by DEX program owner, not user wallet). Without a valid
/// initialized account, LiteSVM gives LiteSVM gives a zero-byte account and SPL Token
/// rejects the swap with "insufficient funds".
fn make_spl_token_account(mint: &Pubkey, owner: &Pubkey) -> solana_account::Account {
    // SPL Token Account layout (165 bytes):
    //   [0..32]    mint
    //   [32..64]   owner
    //   [64..72]   amount (u64 LE, 0 = empty)
    //   [72..76]   delegate discriminant (0 = None)
    //   [76..108]  delegate pubkey (zeroed)
    //   [108]      state (1 = Initialized)
    //   [109..113] is_native discriminant (0 = None)
    //   [113..121] is_native value (zeroed)
    //   [121..129] delegated_amount (u64 LE)
    //   [129..133] close_authority discriminant (0 = None)
    //   [133..165] close_authority pubkey (zeroed)
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(&mint.to_bytes());
    data[32..64].copy_from_slice(&owner.to_bytes());
    data[108] = 1; // AccountState::Initialized
    solana_account::Account {
        lamports: 2_039_280, // rent-exempt minimum for a 165-byte account
        data,
        owner: solana_address::Address::from(native_ix::SPL_TOKEN.to_bytes()),
        executable: false,
        rent_epoch: u64::MAX,
    }
}

async fn process_hit(hit: CycleHit, ctx: Arc<NoMetisCtx>, m: Arc<NoMetisMetrics>, hit_serial: u64) {
    let hops = hit.hops();
    let cfg = &ctx.cfg;

    // Only 2-hop for now (3-hop usually exceeds 1232 bytes without ALTs).
    if hops != 2 {
        m.skipped_hops.fetch_add(1, Relaxed);
        return;
    }

    // DEX family filter: buying and selling on the same family is not real arb.
    let fam0 = dex_family(hit.dex_names[0]);
    let fam1 = dex_family(hit.dex_names[1]);
    if fam0 == fam1 {
        m.skipped_same_family.fetch_add(1, Relaxed);
        return;
    }

    // Amount cap.
    if hit.amount_in > cfg.max_amount_lamports {
        m.skipped_amount.fetch_add(1, Relaxed);
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
        // min_out: 1 for intermediate hops; principal-guard or free for final hop.
        let min_out: u64 = if hop_idx + 1 == hops {
            cfg.final_min_out(hit.amount_in)
        } else {
            1
        };

        let dex_name = hit.dex_names[hop_idx];
        let pool = &hit.pools[hop_idx];

        match native_ix::build_swap(
            dex_name, pool, &user, &mint_in, &mint_out,
            amount_in, min_out, &ctx.store,
        ) {
            Ok(ix) => swap_ixs.push(ix),
            Err(e) => {
                m.skipped_no_builder.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_skip] hit={hit_serial} reason={e} hop={hop_idx} dex={dex_name}"
                );
                return;
            }
        }
    }

    // Assemble V0 transaction (no ALTs).
    let blockhash = ctx.blockhash_cache.get();
    let mut all_ixs: Vec<Instruction> = Vec::with_capacity(hops + 2);
    all_ixs.push(compute_budget_ix(ctx.cu_limit));
    all_ixs.extend(swap_ixs);
    all_ixs.push(jito_tip_ix(&user, cfg.tip_lamports));

    let msg = match v0::Message::try_compile(&user, &all_ixs, &[], blockhash) {
        Ok(m) => m,
        Err(_) => return,
    };
    let tx = match VersionedTransaction::try_new(
        VersionedMessage::V0(msg),
        &[ctx.trading_keypair.as_ref()],
    ) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Size check: Solana max serialized transaction = 1232 bytes.
    let tx_bytes = match bincode::serialize(&tx) {
        Ok(b) => b,
        Err(_) => return,
    };
    if tx_bytes.len() > 1232 {
        m.skipped_tx_too_large.fetch_add(1, Relaxed);
        return;
    }

    let pool_labels: Vec<String> = hit.pools.iter()
        .map(|p| p.to_string()[..8].to_string())
        .collect();
    eprintln!(
        "[no_metis_candidate] hit={hit_serial} hops={hops} amount_in={} profit_gross={} \
pools={} dexes={} tx_bytes={} tip={}",
        hit.amount_in, hit.profit_gross,
        pool_labels.join("→"),
        hit.dex_names.join("→"),
        tx_bytes.len(), cfg.tip_lamports,
    );

    // ── LiteSVM simulation ────────────────────────────────────────────────────
    let mut sim_passed = true;
    if let (Some(sim_pool), Some(sim_cache)) = (&ctx.sim_pool, &ctx.sim_cache) {
        // Inject valid empty SPL token ATAs for every intermediate hop mint.
        // The Yellowstone subscription covers only DEX-program-owned accounts
        // and the user's WSOL ATA. Intermediate token ATAs for the user wallet
        // never stream in, so LiteSVM would see a zero-byte account and SPL Token
        // would reject the swap with "insufficient funds". We pre-populate the
        // cache with a 165-byte Initialized account so the program can read/write it.
        for intermediate_mint in &hit.intermediate_mints {
            let ata = spl_associated_token_account::get_associated_token_address(
                &user, intermediate_mint,
            );
            let needs_inject = match sim_cache.get(&ata) {
                None => true,
                Some(ref acct) => acct.data.len() < 109 || acct.data[108] != 1,
            };
            if needs_inject {
                sim_cache.inject_account(ata, make_spl_token_account(intermediate_mint, &user));
            }
        }

        // Run simulation on a blocking thread (LiteSVM is CPU-bound).
        let tx_clone = tx.clone();
        let sim_pool = sim_pool.clone();
        let sim_cache = sim_cache.clone();
        let sim_result = tokio::task::spawn_blocking(move || {
            sim_pool.simulate_native(&tx_clone, &sim_cache)
        }).await;

        match sim_result {
            Ok(Ok(result)) => {
                // delta = LiteSVM profit - RAM profit
                let litesvm_profit = (result.wsol_after as i64) - (result.wsol_before as i64);
                let ram_profit = hit.profit_gross as i64;
                let delta = litesvm_profit - ram_profit;
                let delta_abs = delta.unsigned_abs() as i64;

                m.sim_ok.fetch_add(1, Relaxed);
                m.sum_delta_abs.fetch_add(delta_abs, Relaxed);
                m.delta_count.fetch_add(1, Relaxed);

                eprintln!(
                    "[no_metis_sim] hit={hit_serial} ok=true \
wsol_before={} wsol_after={} litesvm_profit={:+} ram_profit={ram_profit:+} \
delta={delta:+} units={}",
                    result.wsol_before, result.wsol_after, litesvm_profit,
                    result.compute_units,
                );

                let threshold = cfg.match_threshold_lamports as i64;
                if delta.abs() > threshold {
                    m.sim_mismatch.fetch_add(1, Relaxed);
                    if cfg.dry_run {
                        eprintln!(
                            "[no_metis_sim] hit={hit_serial} MISMATCH delta={delta:+} \
threshold=±{threshold} (dry_run: not blocking)"
                        );
                    } else {
                        eprintln!(
                            "[no_metis_sim] hit={hit_serial} MISMATCH delta={delta:+} \
threshold=±{threshold} — NOT sending"
                        );
                        sim_passed = false;
                    }
                }
            }
            Ok(Err(e)) => {
                m.sim_revert.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_sim] hit={hit_serial} ok=false err={e} — NOT sending"
                );
                sim_passed = false;
            }
            Err(e) => {
                eprintln!(
                    "[no_metis_sim] hit={hit_serial} spawn_blocking_err={e} — NOT sending"
                );
                sim_passed = false;
            }
        }
    }

    if !sim_passed {
        return;
    }

    // In dry-run mode: simulate everything but never send to Jito.
    if cfg.dry_run {
        return;
    }

    // ── Extract signature for post-send status check ──────────────────────────
    let signature = tx.signatures.first().copied();

    // ── Submit to Jito (REST first, gRPC fallback) ────────────────────────────
    let use_rest = ctx
        .jito_limiter
        .lock()
        .map(|mut l| l.try_acquire())
        .unwrap_or(false);

    let bundle_id = if use_rest {
        match ctx.jito.send_bundle(&tx).await {
            Ok(id) => {
                m.jito_rest_sent.fetch_add(1, Relaxed);
                Some(("REST", id))
            }
            Err(e) => {
                m.jito_error.fetch_add(1, Relaxed);
                eprintln!(
                    "[no_metis_status] hit={hit_serial} REST_ERROR err={e}"
                );
                None
            }
        }
    } else if let (Some(grpc), Some(gl)) = (&ctx.jito_grpc, &ctx.jito_grpc_limiter) {
        let use_grpc = gl.lock().map(|mut l| l.try_acquire()).unwrap_or(false);
        if use_grpc {
            match grpc.send_bundle(&tx).await {
                Ok(id) => {
                    m.jito_grpc_sent.fetch_add(1, Relaxed);
                    Some(("gRPC", id))
                }
                Err(e) => {
                    m.jito_error.fetch_add(1, Relaxed);
                    eprintln!(
                        "[no_metis_status] hit={hit_serial} gRPC_ERROR err={e}"
                    );
                    None
                }
            }
        } else {
            m.jito_rate_limited.fetch_add(1, Relaxed);
            eprintln!("[no_metis_status] hit={hit_serial} RATE_LIMITED");
            None
        }
    } else {
        m.jito_rate_limited.fetch_add(1, Relaxed);
        eprintln!("[no_metis_status] hit={hit_serial} RATE_LIMITED");
        None
    };

    if let Some((via, bid)) = bundle_id {
        let sig_str = signature
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".to_string());
        eprintln!(
            "[no_metis_sent] hit={hit_serial} via={via} bundle={bid} signature={sig_str} \
hops={hops} amount_in={} profit_gross={} tip={}",
            hit.amount_in, hit.profit_gross, cfg.tip_lamports,
        );

        // Post-send status check after 20 s.
        if let Some(sig) = signature {
            let rpc = ctx.rpc_client.clone();
            let bid_clone = bid.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(20)).await;
                match rpc.get_signature_status(&sig) {
                    Ok(Some(Ok(()))) => eprintln!(
                        "[no_metis_status] hit={hit_serial} signature={sig} \
bundle={bid_clone} landed=true"
                    ),
                    Ok(Some(Err(e))) => eprintln!(
                        "[no_metis_status] hit={hit_serial} signature={sig} \
bundle={bid_clone} landed=false err={e:?}"
                    ),
                    Ok(None) => eprintln!(
                        "[no_metis_status] hit={hit_serial} signature={sig} \
bundle={bid_clone} landed=false not_found"
                    ),
                    Err(e) => eprintln!(
                        "[no_metis_status] hit={hit_serial} signature={sig} \
bundle={bid_clone} rpc_err={e}"
                    ),
                }
            });
        }
    }
}

// ── Spawn ─────────────────────────────────────────────────────────────────────

pub fn spawn_no_metis_executor(
    mut rx: mpsc::Receiver<CycleHit>,
    ctx: Arc<NoMetisCtx>,
) -> Arc<NoMetisMetrics> {
    let metrics = Arc::new(NoMetisMetrics::default());
    let has_litesvm = ctx.sim_pool.is_some();

    // Periodic stats reporter: cumulative every 30 s, per-minute delta every 60 s.
    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            ticker.tick().await;
            let mut tick_count: u64 = 0;
            let mut snap_sim_ok: u64 = 0;
            let mut snap_sim_rev: u64 = 0;
            loop {
                ticker.tick().await;
                tick_count += 1;
                let recv       = m.received.load(Relaxed);
                let skip_h     = m.skipped_hops.load(Relaxed);
                let skip_fam   = m.skipped_same_family.load(Relaxed);
                let skip_amt   = m.skipped_amount.load(Relaxed);
                let skip_bldr  = m.skipped_no_builder.load(Relaxed);
                let skip_sz    = m.skipped_tx_too_large.load(Relaxed);
                let sim_ok     = m.sim_ok.load(Relaxed);
                let sim_rev    = m.sim_revert.load(Relaxed);
                let sim_mis    = m.sim_mismatch.load(Relaxed);
                let d_count    = m.delta_count.load(Relaxed);
                let d_sum      = m.sum_delta_abs.load(Relaxed);
                let avg_delta  = if d_count > 0 { d_sum / d_count as i64 } else { 0 };
                let rest       = m.jito_rest_sent.load(Relaxed);
                let grpc       = m.jito_grpc_sent.load(Relaxed);
                let rl         = m.jito_rate_limited.load(Relaxed);
                let jerr       = m.jito_error.load(Relaxed);
                eprintln!(
                    "[no_metis] recv={recv} skip_hops={skip_h} same_family={skip_fam} \
skip_amt={skip_amt} no_builder={skip_bldr} tx_large={skip_sz} | \
sim_ok={sim_ok} revert={sim_rev} mismatch={sim_mis} avg_delta={avg_delta}L | \
rest_sent={rest} grpc_sent={grpc} rate_lim={rl} jito_err={jerr}"
                );
                // Every 2 ticks = 60 s: print per-minute simulation rate.
                if tick_count % 2 == 0 {
                    let ok_delta  = sim_ok.saturating_sub(snap_sim_ok);
                    let rev_delta = sim_rev.saturating_sub(snap_sim_rev);
                    snap_sim_ok  = sim_ok;
                    snap_sim_rev = sim_rev;
                    eprintln!(
                        "[no_metis_1m] sim_ok/min={ok_delta} sim_revert/min={rev_delta} \
total_sim={}",
                        sim_ok + sim_rev
                    );
                }
            }
        });
    }

    let m = metrics.clone();
    tokio::spawn(async move {
        eprintln!(
            "[no_metis_executor] started — litesvm={has_litesvm} dry_run={} tip={}L \
max_amount={}L match_threshold=±{}L final_min_out={}L",
            ctx.cfg.dry_run,
            ctx.cfg.tip_lamports,
            ctx.cfg.max_amount_lamports,
            ctx.cfg.match_threshold_lamports,
            ctx.cfg.final_min_out_lamports,
        );

        let mut hit_serial: u64 = 0;
        while let Some(hit) = rx.recv().await {
            hit_serial += 1;
            m.received.fetch_add(1, Relaxed);
            let ctx = ctx.clone();
            let m = m.clone();
            let serial = hit_serial;
            tokio::spawn(async move { process_hit(hit, ctx, m, serial).await });
        }

        eprintln!("[no_metis_executor] channel closed — exiting");
    });

    metrics
}
