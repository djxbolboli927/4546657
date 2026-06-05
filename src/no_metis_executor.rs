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

use std::collections::HashMap;
use std::io::Write as IoWrite;
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

// ── Dif accumulator ──────────────────────────────────────────────────────────

struct HopRecord {
    dex: &'static str,
    pool_short: String,
    amount_in: u64,
    ram_out: u64,
    sim_out: Option<u64>,
    revert_msg: Option<String>,
}

struct PathRecord {
    hit_serial: u64,
    pools_str: String,
    amount_in: u64,
    ram_profit: i64,
    hops: Vec<HopRecord>,
}

#[derive(Default, Clone)]
struct HopStats {
    ok: u64,
    revert: u64,
    delta_sum: i64,
    delta_n: u64,
    delta_min: i64,
    delta_max: i64,
}

impl HopStats {
    fn record(&mut self, sim_out: Option<u64>, ram_out: u64) {
        match sim_out {
            Some(out) => {
                self.ok += 1;
                let delta = (out as i64) - (ram_out as i64);
                self.delta_sum += delta;
                if self.delta_n == 0 {
                    self.delta_min = delta;
                    self.delta_max = delta;
                } else {
                    if delta < self.delta_min { self.delta_min = delta; }
                    if delta > self.delta_max { self.delta_max = delta; }
                }
                self.delta_n += 1;
            }
            None => self.revert += 1,
        }
    }
    fn avg(&self) -> i64 {
        if self.delta_n > 0 { self.delta_sum / self.delta_n as i64 } else { 0 }
    }
}

struct DifAccum {
    five_min_records: Vec<PathRecord>,
    hour_hop_stats: Vec<HopStats>,
    hour_ok: u64,
    hour_revert: u64,
    dif_path: String,
}

impl DifAccum {
    fn new(dif_path: String) -> Self {
        Self {
            five_min_records: Vec::new(),
            hour_hop_stats: vec![HopStats::default(); 4],
            hour_ok: 0,
            hour_revert: 0,
            dif_path,
        }
    }

    fn add(&mut self, rec: PathRecord) {
        // Update hour stats.
        let all_ok = rec.hops.iter().all(|h| h.sim_out.is_some());
        if all_ok { self.hour_ok += 1; } else { self.hour_revert += 1; }
        for (i, h) in rec.hops.iter().enumerate() {
            while self.hour_hop_stats.len() <= i {
                self.hour_hop_stats.push(HopStats::default());
            }
            self.hour_hop_stats[i].record(h.sim_out, h.ram_out);
        }
        self.five_min_records.push(rec);
    }

    fn flush_five_min(&mut self) -> String {
        use std::fmt::Write as _;
        let records = std::mem::take(&mut self.five_min_records);
        if records.is_empty() {
            return String::new();
        }

        // Compute 5-min hop stats from these records.
        let mut hop_stats: Vec<HopStats> = Vec::new();
        let mut ok_paths: u64 = 0;
        let mut rev_paths: u64 = 0;
        for rec in &records {
            let all_ok = rec.hops.iter().all(|h| h.sim_out.is_some());
            if all_ok { ok_paths += 1; } else { rev_paths += 1; }
            for (i, h) in rec.hops.iter().enumerate() {
                while hop_stats.len() <= i {
                    hop_stats.push(HopStats::default());
                }
                hop_stats[i].record(h.sim_out, h.ram_out);
            }
        }

        let ts = chrono_like_ts();
        let mut out = String::new();
        let _ = writeln!(out, "=== 5-min [{}] paths={} ok={} revert={} ===",
            ts, records.len(), ok_paths, rev_paths);
        for (i, hs) in hop_stats.iter().enumerate() {
            let _ = writeln!(out,
                "  hop[{i}] ok={} revert={} avg_delta={:+} min={:+} max={:+}",
                hs.ok, hs.revert, hs.avg(), hs.delta_min, hs.delta_max);
        }
        let _ = writeln!(out, "--- records ---");
        for rec in &records {
            let _ = writeln!(out, "  hit={} pools={} amount_in={} ram_profit={:+}",
                rec.hit_serial, rec.pools_str, rec.amount_in, rec.ram_profit);
            for (i, h) in rec.hops.iter().enumerate() {
                let sim_str = match h.sim_out {
                    Some(v) => {
                        let delta = (v as i64) - (h.ram_out as i64);
                        format!("sim_out={v} delta={delta:+}")
                    }
                    None => {
                        let msg = h.revert_msg.as_deref().unwrap_or("?");
                        format!("REVERT err=\"{}\"", &msg[..msg.len().min(80)])
                    }
                };
                let _ = writeln!(out,
                    "    h{i} {} pool={} in={} ram_out={} {}",
                    h.dex, h.pool_short, h.amount_in, h.ram_out, sim_str);
            }
        }
        out
    }

    fn hour_summary(&self) -> String {
        use std::fmt::Write as _;
        let ts = chrono_like_ts();
        let mut out = String::new();
        let total = self.hour_ok + self.hour_revert;
        let _ = writeln!(out, "=== 1-hour [{}] paths={} ok={} revert={} ===",
            ts, total, self.hour_ok, self.hour_revert);
        for (i, hs) in self.hour_hop_stats.iter().enumerate() {
            if hs.ok + hs.revert == 0 { continue; }
            let _ = writeln!(out,
                "  hop[{i}] ok={} revert={} avg_delta={:+} min={:+} max={:+}",
                hs.ok, hs.revert, hs.avg(), hs.delta_min, hs.delta_max);
        }
        out
    }

    fn reset_hour(&mut self) {
        self.hour_hop_stats = vec![HopStats::default(); 4];
        self.hour_ok = 0;
        self.hour_revert = 0;
    }
}

fn chrono_like_ts() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    format!("day{days}T{h:02}:{m:02}:{s:02}Z")
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

/// Same as `make_spl_token_account` but with a non-zero token balance.
/// For WSOL, lamports includes the wrapped amount (rent + amount).
fn make_spl_token_account_with_amount(
    mint: &Pubkey,
    owner: &Pubkey,
    amount: u64,
) -> solana_account::Account {
    let mut account = make_spl_token_account(mint, owner);
    account.data[64..72].copy_from_slice(&amount.to_le_bytes());
    account.lamports = 2_039_280 + amount;
    account
}

// ── Per-hop simulation ────────────────────────────────────────────────────────

/// Simulate each hop of `hit` as an independent single-instruction transaction.
/// Chained: hop N's sim output becomes hop N+1's injected input.
/// Returns one `HopRecord` per hop (or fewer if instruction-build fails).
async fn simulate_hops(
    hit: &CycleHit,
    ctx: &NoMetisCtx,
    sim_pool: Arc<crate::litesvm_sim::SimulatorPool>,
    sim_cache: Arc<AccountCache>,
    user: Pubkey,
    wsol: Pubkey,
    blockhash: solana_sdk::hash::Hash,
) -> Vec<HopRecord> {
    let hops = hit.hops();
    let mut results: Vec<HopRecord> = Vec::with_capacity(hops);
    let mut prev_sim_out: Option<u64> = None;

    for hop_idx in 0..hops {
        let mint_in = if hop_idx == 0 { wsol } else { hit.intermediate_mints[hop_idx - 1] };
        let mint_out = if hop_idx + 1 == hops { wsol } else { hit.intermediate_mints[hop_idx] };

        let amount_in = if hop_idx == 0 {
            hit.amount_in
        } else {
            prev_sim_out.unwrap_or_else(|| hit.intermediate_amounts.get(hop_idx - 1).copied().unwrap_or(0))
        };

        let ram_out = if hop_idx + 1 == hops {
            hit.amount_in + hit.profit_gross
        } else {
            hit.intermediate_amounts.get(hop_idx).copied().unwrap_or(0)
        };

        let dex_name = hit.dex_names[hop_idx];
        let pool = hit.pools[hop_idx];
        let pool_short = pool.to_string()[..8].to_string();

        let swap_ix = match native_ix::build_swap(
            dex_name, &pool, &user, &mint_in, &mint_out,
            amount_in, 1, &ctx.store,
        ) {
            Ok(ix) => ix,
            Err(e) => {
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: None,
                    revert_msg: Some(format!("build_swap: {e}")),
                });
                prev_sim_out = None;
                continue;
            }
        };

        let all_ixs = vec![compute_budget_ix(ctx.cu_limit), swap_ix];
        let msg = match solana_sdk::message::v0::Message::try_compile(&user, &all_ixs, &[], blockhash) {
            Ok(m) => m,
            Err(e) => {
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: None,
                    revert_msg: Some(format!("compile: {e}")),
                });
                prev_sim_out = None;
                continue;
            }
        };
        let tx = match VersionedTransaction::try_new(
            VersionedMessage::V0(msg),
            &[ctx.trading_keypair.as_ref()],
        ) {
            Ok(t) => t,
            Err(e) => {
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: None,
                    revert_msg: Some(format!("sign: {e}")),
                });
                prev_sim_out = None;
                continue;
            }
        };

        // Inject input ATA with exact amount; output ATA with 0.
        let input_ata = spl_associated_token_account::get_associated_token_address(&user, &mint_in);
        let output_ata = spl_associated_token_account::get_associated_token_address(&user, &mint_out);
        let mut overrides: HashMap<Pubkey, solana_account::Account> = HashMap::new();
        overrides.insert(input_ata, make_spl_token_account_with_amount(&mint_in, &user, amount_in));
        overrides.insert(output_ata, make_spl_token_account(&mint_out, &user));

        let sp = sim_pool.clone();
        let sc = sim_cache.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            sp.simulate_hop(&tx, &sc, &overrides, output_ata)
        }).await;

        match outcome {
            Ok(Ok(o)) => {
                prev_sim_out = Some(o.out_balance);
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: Some(o.out_balance),
                    revert_msg: None,
                });
            }
            Ok(Err(e)) => {
                prev_sim_out = None;
                let msg = e.to_string();
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: None,
                    revert_msg: Some(msg),
                });
            }
            Err(e) => {
                prev_sim_out = None;
                results.push(HopRecord {
                    dex: dex_name, pool_short, amount_in, ram_out,
                    sim_out: None,
                    revert_msg: Some(format!("spawn: {e}")),
                });
            }
        }
    }
    results
}

async fn process_hit(hit: CycleHit, ctx: Arc<NoMetisCtx>, m: Arc<NoMetisMetrics>, hit_serial: u64, dif: Arc<Mutex<DifAccum>>) {
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

    // ── Per-hop LiteSVM simulation ────────────────────────────────────────────
    if let (Some(sim_pool), Some(sim_cache)) = (&ctx.sim_pool, &ctx.sim_cache) {
        let pools_str = hit.pools.iter()
            .map(|p| p.to_string()[..8].to_string())
            .collect::<Vec<_>>()
            .join("→");

        let hop_records = simulate_hops(
            &hit, &ctx, sim_pool.clone(), sim_cache.clone(),
            user, wsol, blockhash,
        ).await;

        // Update counters.
        let all_ok = hop_records.iter().all(|h| h.sim_out.is_some());
        if all_ok {
            m.sim_ok.fetch_add(1, Relaxed);
        } else {
            m.sim_revert.fetch_add(1, Relaxed);
        }

        let ram_profit = hit.profit_gross as i64;
        let path_rec = PathRecord {
            hit_serial,
            pools_str,
            amount_in: hit.amount_in,
            ram_profit,
            hops: hop_records,
        };
        if let Ok(mut acc) = dif.lock() {
            acc.add(path_rec);
        }

        if !all_ok || cfg.dry_run {
            return;
        }
    } else {
        // No sim pool — dry_run still prevents sending.
        if cfg.dry_run {
            return;
        }
    }

    // Keep the full-tx mismatch guard when NOT in dry_run and sim is available.
    // (Re-run full-tx sim for the send decision so we know the exact WSOL delta.)
    let mut sim_passed = true;
    if let (Some(sim_pool), Some(sim_cache)) = (&ctx.sim_pool, &ctx.sim_cache) {
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
        let tx_clone = tx.clone();
        let sp = sim_pool.clone();
        let sc = sim_cache.clone();
        match tokio::task::spawn_blocking(move || sp.simulate_native(&tx_clone, &sc)).await {
            Ok(Ok(result)) => {
                let litesvm_profit = (result.wsol_after as i64) - (result.wsol_before as i64);
                let ram_profit = hit.profit_gross as i64;
                let delta = litesvm_profit - ram_profit;
                m.sum_delta_abs.fetch_add(delta.unsigned_abs() as i64, Relaxed);
                m.delta_count.fetch_add(1, Relaxed);
                let threshold = cfg.match_threshold_lamports as i64;
                if delta.abs() > threshold {
                    m.sim_mismatch.fetch_add(1, Relaxed);
                    sim_passed = false;
                }
            }
            Ok(Err(_)) | Err(_) => sim_passed = false,
        }
    }

    if !sim_passed {
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

    // Shared per-hop diagnostic accumulator.
    let dif_path = "/home/user/4546657/dif".to_string();
    let dif = Arc::new(Mutex::new(DifAccum::new(dif_path.clone())));

    // 5-minute reporter.
    {
        let dif = dif.clone();
        let m = metrics.clone();
        let dif_path_5m = dif_path.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(300));
            ticker.tick().await; // skip first immediate tick
            loop {
                ticker.tick().await;
                let (report, ok, rev) = {
                    let mut acc = match dif.lock() { Ok(a) => a, Err(_) => continue };
                    let ok = m.sim_ok.load(Relaxed);
                    let rev = m.sim_revert.load(Relaxed);
                    (acc.flush_five_min(), ok, rev)
                };
                if report.is_empty() {
                    eprintln!("[dif_5m] no sims in last 5 min (total ok={ok} revert={rev})");
                    continue;
                }
                // Terminal: summary line
                let first_two: Vec<&str> = report.lines().take(4).collect();
                for ln in &first_two { eprintln!("{ln}"); }
                // File: full report
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true).append(true).open(&dif_path_5m)
                {
                    let _ = f.write_all(report.as_bytes());
                    let _ = writeln!(f);
                }
            }
        });
    }

    // 1-hour reporter.
    {
        let dif = dif.clone();
        let dif_path_1h = dif_path.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(3600));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let summary = {
                    let mut acc = match dif.lock() { Ok(a) => a, Err(_) => continue };
                    let s = acc.hour_summary();
                    acc.reset_hour();
                    s
                };
                if summary.is_empty() { continue; }
                // Terminal: full summary
                for ln in summary.lines() { eprintln!("{ln}"); }
                // File: append
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true).append(true).open(&dif_path_1h)
                {
                    let _ = f.write_all(summary.as_bytes());
                    let _ = writeln!(f);
                }
            }
        });
    }

    let m = metrics.clone();
    tokio::spawn(async move {
        eprintln!(
            "[no_metis_executor] started — litesvm={has_litesvm} dry_run={} tip={}L \
max_amount={}L dif={}",
            ctx.cfg.dry_run, ctx.cfg.tip_lamports, ctx.cfg.max_amount_lamports, dif_path,
        );

        let mut hit_serial: u64 = 0;
        while let Some(hit) = rx.recv().await {
            hit_serial += 1;
            m.received.fetch_add(1, Relaxed);
            let ctx = ctx.clone();
            let m = m.clone();
            let dif = dif.clone();
            let serial = hit_serial;
            tokio::spawn(async move { process_hit(hit, ctx, m, serial, dif).await });
        }

        eprintln!("[no_metis_executor] channel closed — exiting");
    });

    metrics
}
