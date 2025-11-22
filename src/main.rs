#![allow(dead_code)]

use anyhow::{Context, Result};
use backoff::{future::retry, ExponentialBackoff};
use borsh::BorshDeserialize;
use bs58;
use crossbeam_channel::{unbounded, Receiver, Sender};
use dashmap::DashMap;
use dotenvy::dotenv;
use futures::{SinkExt, StreamExt};
use log::{error, info, warn};
use rayon::prelude::*;
use serde_jsonc;
use sha2::{Digest, Sha256};
use solana_entry::entry::Entry;
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use solana_stream_sdk::{
    GeyserGrpcClient, GeyserSubscribeRequest, GeyserSubscribeRequestFilterAccounts,
    GeyserSubscribeRequestFilterBlocks, GeyserSubscribeRequestFilterBlocksMeta,
    GeyserSubscribeRequestFilterEntry, GeyserSubscribeRequestFilterSlots,
    GeyserSubscribeRequestFilterTransactions, GeyserSubscribeUpdate, GeyserUpdateOneof,
    ShredstreamClient,
};
use std::{
    env, fs,
    str::FromStr,
    sync::{Arc, atomic::{AtomicUsize, AtomicU64, Ordering}},
    thread,
    time::{Duration, Instant},
};
use tonic::transport::ClientTlsConfig;

mod config;
use config::{commitment_from_str, Config};

mod wallet_manager;
use wallet_manager::WalletManager;

mod jito_client;
use jito_client::JitoClient;

mod transaction_builder;
use transaction_builder::TransactionBuilder;

mod pumpfun_instructions;
use pumpfun_instructions::{
    derive_bonding_curve,
};

mod spl_utils;

// ❌ حذف شد: mod bonding_curve_utils;
// دلیل: دیگر نیازی به خواندن creator از bonding curve نیست!

// ═══════════════════════════════════════════════════════════════
// Constants - OPTIMIZED FOR MAXIMUM PROFIT
// ═══════════════════════════════════════════════════════════════

const MIN_SOL_COST: u64 = LAMPORTS_PER_SOL / 20; // 0.05 SOL
const MAX_SOL_COST: u64 = LAMPORTS_PER_SOL * 10; // 10 SOL
const MIN_POOL_SOL: u64 = LAMPORTS_PER_SOL; // 1 SOL
const COMPUTE_BUDGET_PROGRAM_ID: &str = "ComputeBudget111111111111111111111111111111";
const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const WORKER_COUNT: usize = 6;

const PUMPFUN_FEE_BPS: u64 = 100;
const FEE_DENOMINATOR: u64 = 10000;

// OPTIMIZED STRATEGY
const SANDWICH_MIN_PROFIT_LAMPORTS: u64 = LAMPORTS_PER_SOL / 500; // 0.002 SOL
const SANDWICH_SAFETY_MARGIN: f64 = 0.98; // 98% fill rate!
const JITO_TIP_LAMPORTS: u64 = LAMPORTS_PER_SOL / 1000;
const ESTIMATED_NETWORK_FEE: u64 = 5000;

// Priority fee = SAME as victim!
const FRONT_RUN_FEE_MULTIPLIER: f64 = 1.0; // Same!
const BACK_RUN_FEE_MULTIPLIER: f64 = 1.0; // Same!
const BASE_PRIORITY_FEE: u64 = 50_000;

const PUMP_FUN_DISCRIMINATOR: [u8; 8] = [0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];

// Cleanup interval
const CLEANUP_INTERVAL_SECS: u64 = 300; // 5 minutes
const MAX_ACTIVITY_AGE_SECS: u64 = 600; // 10 minutes

// Simulation control
// ✅ DISABLED for maximum speed! RPC simulation is too slow for front-running.
// We use local AMM calculations instead (much faster & more accurate).
const ENABLE_RPC_SIMULATION: bool = false;

// ═══════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct PoolState {
    pub pool_address: String,
    pub token_amount: u64,
    pub sol_amount: u64,
    pub slot: u64,
    pub last_update: Instant,
    pub total_volume_lamports: u64,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
}

#[derive(BorshDeserialize, Debug, Clone)]
pub struct PumpFunBondingCurve {
    pub discriminator: [u8; 8],
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
}

#[derive(BorshDeserialize, Debug)]
struct BuyInstructionArgs {
    _discriminator: u64,
    token_amount: u64,
    max_sol_cost: u64,
}

struct ShredsData {
    slot: u64,
    entries_raw: Vec<u8>,
}

// ✅ اصلاح شد: creator_address → creator_vault
#[derive(Debug, Clone)]
struct TransactionInfo {
    buyer: String,
    mint: String,
    bonding_curve: String,
    max_sol: u64,
    token_amount: u64,
    priority_fee: u64,
    signature: String,
    timestamp: Instant,
    slot: u64,
    creator_vault: Option<String>,  // ✅ تغییر نام از creator_address
}

#[derive(Debug, Clone)]
struct SandwichSimulation {
    victim_tx: TransactionInfo,

    front_run_sol: u64,
    front_run_tokens: u64,
    victim_cost_after_frontrun: u64,
    back_run_sol: u64,

    gross_profit: i64,
    net_profit: i64,
    roi_percent: f64,

    total_fees: u64,

    is_profitable: bool,
}

struct GlobalStats {
    profitable_count: AtomicUsize,
    unprofitable_count: AtomicUsize,
    total_tx_processed: AtomicUsize,
    start_time: Instant,
    shreds_received: AtomicUsize,
    geyser_updates: AtomicUsize,
    total_profit_lamports: AtomicU64,
    bundles_sent: AtomicUsize,
    bundles_failed: AtomicUsize,

    skipped_no_pool: AtomicUsize,
    skipped_low_sol: AtomicUsize,
    skipped_same_block: AtomicUsize,
    skipped_no_creator: AtomicUsize,
}

impl GlobalStats {
    fn new() -> Self {
        GlobalStats {
            profitable_count: AtomicUsize::new(0),
            unprofitable_count: AtomicUsize::new(0),
            total_tx_processed: AtomicUsize::new(0),
            start_time: Instant::now(),
            shreds_received: AtomicUsize::new(0),
            geyser_updates: AtomicUsize::new(0),
            total_profit_lamports: AtomicU64::new(0),
            bundles_sent: AtomicUsize::new(0),
            bundles_failed: AtomicUsize::new(0),
            skipped_no_pool: AtomicUsize::new(0),
            skipped_low_sol: AtomicUsize::new(0),
            skipped_same_block: AtomicUsize::new(0),
            skipped_no_creator: AtomicUsize::new(0),
        }
    }
}

struct WorkerPool {
    workers: Vec<Sender<TransactionInfo>>,
    next_worker: AtomicUsize,
    dropped_count: AtomicUsize,
    processed_count: AtomicUsize,
}

impl WorkerPool {
    fn new(
        worker_count: usize,
        pool_tracker: PoolTracker,
        stats: Arc<GlobalStats>,
        jito_client: Arc<JitoClient>,
        wallet_manager: Arc<WalletManager>,
        tx_builder: Arc<TransactionBuilder>,
        recent_activity: RecentActivity,
    ) -> Self {
        let mut workers = Vec::new();

        for id in 0..worker_count {
            let (tx, rx) = crossbeam_channel::bounded::<TransactionInfo>(0);
            workers.push(tx);

            let tracker = pool_tracker.clone();
            let stats_clone = stats.clone();
            let jito_clone = jito_client.clone();
            let wallet_clone = wallet_manager.clone();
            let builder_clone = tx_builder.clone();
            let activity_clone = recent_activity.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    unified_worker_thread(
                        id,
                        rx,
                        tracker,
                        stats_clone,
                        jito_clone,
                        wallet_clone,
                        builder_clone,
                        activity_clone,
                    ).await;
                });
            });
        }

        WorkerPool {
            workers,
            next_worker: AtomicUsize::new(0),
            dropped_count: AtomicUsize::new(0),
            processed_count: AtomicUsize::new(0),
        }
    }

    fn try_send_to_worker(&self, tx_info: TransactionInfo) {
        let worker_count = self.workers.len();
        let mut attempts = 0;

        while attempts < worker_count {
            let worker_idx = self.next_worker.fetch_add(1, Ordering::Relaxed) % worker_count;

            match self.workers[worker_idx].try_send(tx_info.clone()) {
                Ok(_) => {
                    self.processed_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    attempts += 1;
                }
            }
        }

        self.dropped_count.fetch_add(1, Ordering::Relaxed);
    }
}

type PoolTracker = Arc<DashMap<String, PoolState>>;

// ═══════════════════════════════════════════════════════════════
// Same-Block Detection (FILTER 1)
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct RecentTxActivity {
    last_buy_slot: Option<u64>,
    last_sell_slot: Option<u64>,
    last_activity: Instant,
}

type RecentActivity = Arc<DashMap<String, RecentTxActivity>>;

fn has_same_block_buy_sell(
    address: &str,
    mint: &str,
    current_slot: u64,
    recent_activity: &RecentActivity,
) -> bool {
    let key = format!("{}:{}", address, mint);

    if let Some(activity) = recent_activity.get(&key) {
        if let Some(sell_slot) = activity.last_sell_slot {
            if sell_slot == current_slot {
                return true;
            }
        }

        if let Some(buy_slot) = activity.last_buy_slot {
            if buy_slot == current_slot {
                return true;
            }
        }
    }

    false
}

fn record_buy_activity(
    address: &str,
    mint: &str,
    slot: u64,
    recent_activity: &RecentActivity,
) {
    let key = format!("{}:{}", address, mint);

    recent_activity.entry(key)
        .and_modify(|activity| {
            activity.last_buy_slot = Some(slot);
            activity.last_activity = Instant::now();
        })
        .or_insert_with(|| RecentTxActivity {
            last_buy_slot: Some(slot),
            last_sell_slot: None,
            last_activity: Instant::now(),
        });
}

fn cleanup_old_activity(recent_activity: &RecentActivity) {
    let cutoff = Instant::now() - Duration::from_secs(MAX_ACTIVITY_AGE_SECS);
    let mut removed = 0;

    recent_activity.retain(|_, activity| {
        if activity.last_activity < cutoff {
            removed += 1;
            false
        } else {
            true
        }
    });

    if removed > 0 {
        info!("🧹 Cleaned {} old activity records", removed);
    }
}

// ═══════════════════════════════════════════════════════════════
// AMM Calculations
// ═══════════════════════════════════════════════════════════════

fn calculate_token_out_with_fee(sol_in: u64, sol_reserve: u64, token_reserve: u64) -> u64 {
    let sol_in_after_fee = sol_in - (sol_in * PUMPFUN_FEE_BPS / FEE_DENOMINATOR);
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_sol = sol_reserve + sol_in_after_fee;
    let new_token = k / (new_sol as u128);
    token_reserve.saturating_sub(new_token as u64)
}

fn calculate_sol_in_with_fee(token_out: u64, sol_reserve: u64, token_reserve: u64) -> u64 {
    if token_out >= token_reserve {
        return u64::MAX;
    }

    let new_token = token_reserve - token_out;
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_sol = k / (new_token as u128);
    let sol_needed = (new_sol as u64).saturating_sub(sol_reserve);

    let sol_with_fee = (sol_needed as u128 * FEE_DENOMINATOR as u128 / (FEE_DENOMINATOR - PUMPFUN_FEE_BPS) as u128) as u64;

    sol_with_fee
}

fn calculate_token_out_with_fee_swapped(token_in: u64, token_reserve: u64, sol_reserve: u64) -> u64 {
    let token_in_after_fee = token_in - (token_in * PUMPFUN_FEE_BPS / FEE_DENOMINATOR);
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_token = token_reserve + token_in_after_fee;
    let new_sol = k / (new_token as u128);
    sol_reserve.saturating_sub(new_sol as u64)
}

// ═══════════════════════════════════════════════════════════════
// Sandwich Simulation (FILTER 2) - OPTIMIZED!
// ═══════════════════════════════════════════════════════════════

fn simulate_sandwich_attack(
    victim_tx: &TransactionInfo,
    pool: &PoolState,
) -> SandwichSimulation {
    let v_sol = pool.virtual_sol_reserves;
    let v_token = pool.virtual_token_reserves;

    let victim_cost_without_frontrun = calculate_sol_in_with_fee(
        victim_tx.token_amount,
        v_sol,
        v_token
    );

    if victim_cost_without_frontrun >= victim_tx.max_sol {
        return SandwichSimulation {
            victim_tx: victim_tx.clone(),
            front_run_sol: 0,
            front_run_tokens: 0,
            victim_cost_after_frontrun: 0,
            back_run_sol: 0,
            gross_profit: 0,
            net_profit: 0,
            roi_percent: 0.0,
            total_fees: 0,
            is_profitable: false,
        };
    }

    let remaining_space = victim_tx.max_sol - victim_cost_without_frontrun;
    let target_frontrun_sol = (remaining_space as f64 * SANDWICH_SAFETY_MARGIN) as u64;

    let front_run_tokens = calculate_token_out_with_fee(
        target_frontrun_sol,
        v_sol,
        v_token
    );

    let frontrun_fee = target_frontrun_sol * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let v_sol_after_fr = v_sol + (target_frontrun_sol - frontrun_fee);
    let v_token_after_fr = v_token - front_run_tokens;

    let victim_cost_after_frontrun = calculate_sol_in_with_fee(
        victim_tx.token_amount,
        v_sol_after_fr,
        v_token_after_fr
    );

    if victim_cost_after_frontrun > victim_tx.max_sol {
        return SandwichSimulation {
            victim_tx: victim_tx.clone(),
            front_run_sol: target_frontrun_sol,
            front_run_tokens,
            victim_cost_after_frontrun,
            back_run_sol: 0,
            gross_profit: 0,
            net_profit: -(target_frontrun_sol as i64),
            roi_percent: -100.0,
            total_fees: frontrun_fee,
            is_profitable: false,
        };
    }

    let victim_fee = victim_cost_after_frontrun * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let v_sol_after_victim = v_sol_after_fr + (victim_cost_after_frontrun - victim_fee);
    let v_token_after_victim = v_token_after_fr - victim_tx.token_amount;

    let back_run_sol = calculate_token_out_with_fee_swapped(
        front_run_tokens,
        v_token_after_victim,
        v_sol_after_victim
    );

    let backrun_fee = back_run_sol * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let network_fees = ESTIMATED_NETWORK_FEE * 2;
    let total_fees = frontrun_fee + backrun_fee + JITO_TIP_LAMPORTS + network_fees;
    let gross_profit = (back_run_sol as i64) - (target_frontrun_sol as i64);
    let net_profit = gross_profit - (total_fees as i64);

    let roi_percent = if target_frontrun_sol > 0 {
        (net_profit as f64 / target_frontrun_sol as f64) * 100.0
    } else {
        0.0
    };

    SandwichSimulation {
        victim_tx: victim_tx.clone(),
        front_run_sol: target_frontrun_sol,
        front_run_tokens,
        victim_cost_after_frontrun,
        back_run_sol,
        gross_profit,
        net_profit,
        roi_percent,
        total_fees,
        is_profitable: net_profit > SANDWICH_MIN_PROFIT_LAMPORTS as i64,
    }
}

fn print_simulation_result(sim: &SandwichSimulation, worker_id: usize, profitable: bool) {
    let status = if profitable { "✅ PROFITABLE" } else { "❌ UNPROFITABLE" };

    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║ {} SANDWICH [Worker {}]", status, worker_id);
    info!("╠═══════════════════════════════════════════════════════════╣");
    info!("║ VICTIM:");
    info!("║   Signature: ...{}", &sim.victim_tx.signature[sim.victim_tx.signature.len()-8..]);
    info!("║   Token Amount: {}", sim.victim_tx.token_amount);
    info!("║   Max SOL: {:.6}", sim.victim_tx.max_sol as f64 / LAMPORTS_PER_SOL as f64);
    info!("║   Priority Fee: {} μLamp", sim.victim_tx.priority_fee);
    info!("╠═══════════════════════════════════════════════════════════╣");
    info!("║ ATTACK:");
    info!("║   1. Front-run: {:.6} SOL → {} tokens",
          sim.front_run_sol as f64 / LAMPORTS_PER_SOL as f64,
          sim.front_run_tokens);
    info!("║   2. Victim pays: {:.6} SOL (limit: {:.6})",
          sim.victim_cost_after_frontrun as f64 / LAMPORTS_PER_SOL as f64,
          sim.victim_tx.max_sol as f64 / LAMPORTS_PER_SOL as f64);
    info!("║   3. Back-run: {} tokens → {:.6} SOL",
          sim.front_run_tokens,
          sim.back_run_sol as f64 / LAMPORTS_PER_SOL as f64);
    info!("╠═══════════════════════════════════════════════════════════╣");
    info!("║ PROFIT:");
    info!("║   Gross: {:.6} SOL", sim.gross_profit as f64 / LAMPORTS_PER_SOL as f64);
    info!("║   Fees: {:.6} SOL", sim.total_fees as f64 / LAMPORTS_PER_SOL as f64);
    info!("║   Net: {:.6} SOL", sim.net_profit as f64 / LAMPORTS_PER_SOL as f64);
    info!("║   ROI: {:.2}%", sim.roi_percent);
    info!("╚═══════════════════════════════════════════════════════════╝");
}

// ═══════════════════════════════════════════════════════════
// ✅✅✅ FIXED WORKER THREAD - بخش اصلی تصحیح شده! ✅✅✅
// ═══════════════════════════════════════════════════════════

async fn unified_worker_thread(
    worker_id: usize,
    rx: Receiver<TransactionInfo>,
    pool_tracker: PoolTracker,
    stats: Arc<GlobalStats>,
    jito_client: Arc<JitoClient>,
    wallet_manager: Arc<WalletManager>,
    tx_builder: Arc<TransactionBuilder>,
    recent_activity: RecentActivity,
) {
    info!("Worker {} started 🚀", worker_id);

    for tx_info in rx.iter() {
        stats.total_tx_processed.fetch_add(1, Ordering::Relaxed);

        // ═══════════════════════════════════════════════════════════
        // FILTER 1: Pool exists?
        // ═══════════════════════════════════════════════════════════
        let pool_state = match pool_tracker.get(&tx_info.bonding_curve) {
            Some(pool) => pool.clone(),
            None => {
                stats.skipped_no_pool.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // ═══════════════════════════════════════════════════════════
        // FILTER 2: Minimum SOL?
        // ═══════════════════════════════════════════════════════════
        if tx_info.max_sol < MIN_SOL_COST {
            stats.skipped_low_sol.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // ═══════════════════════════════════════════════════════════
        // FILTER 3: Same-block buy+sell detection
        // ═══════════════════════════════════════════════════════════
        if has_same_block_buy_sell(
            &tx_info.buyer,
            &tx_info.mint,
            tx_info.slot,
            &recent_activity,
        ) {
            stats.skipped_same_block.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        record_buy_activity(
            &tx_info.buyer,
            &tx_info.mint,
            tx_info.slot,
            &recent_activity,
        );

        // ═══════════════════════════════════════════════════════════
        // ✅ PASSED ALL FILTERS - Try simulation!
        // ═══════════════════════════════════════════════════════════

        let simulation = simulate_sandwich_attack(&tx_info, &pool_state);

        if simulation.is_profitable {
            stats.profitable_count.fetch_add(1, Ordering::Relaxed);
            stats.total_profit_lamports.fetch_add(simulation.net_profit as u64, Ordering::Relaxed);
            print_simulation_result(&simulation, worker_id, true);

            // ═══════════════════════════════════════════════════════════
            // 🚀 EXECUTION - Real bundle submission
            // ═══════════════════════════════════════════════════════════

            let mint = match Pubkey::from_str(&tx_info.mint) {
                Ok(m) => m,
                Err(e) => {
                    error!("❌ [W{}] Invalid mint pubkey: {}", worker_id, e);
                    continue;
                }
            };

            // ✅ محاسبه bonding_curve از mint
            let bonding_curve = derive_bonding_curve(&mint);
            info!("✅ [W{}] Bonding curve calculated: {}", worker_id, bonding_curve);

            // ═══════════════════════════════════════════════════════════
            // 🔥🔥🔥 CRITICAL FIX: استفاده از Creator Vault از تراکنش قربانی!
            // ═══════════════════════════════════════════════════════════

            // ✅ دریافت creator_vault مستقیماً از تراکنش قربانی (account #9)
            let creator_vault_str = match &tx_info.creator_vault {
                Some(cv) => cv,
                None => {
                    error!("❌ [W{}] No creator vault in victim tx!", worker_id);
                    error!("   This should never happen - victim tx must have creator_vault");
                    stats.skipped_no_creator.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            let creator_vault = match Pubkey::from_str(creator_vault_str) {
                Ok(cv) => cv,
                Err(e) => {
                    error!("❌ [W{}] Invalid creator vault pubkey: {}", worker_id, e);
                    continue;
                }
            };

            info!("✅ [W{}] Creator Vault from victim tx: {}", worker_id, creator_vault);
            info!("   Mint: {}", mint);
            info!("   Bonding Curve: {}", bonding_curve);

            // ❌ حذف شد: کل بخش get_creator_from_bonding_curve
            // ❌ حذف شد: کل بخش derive_creator_vault
            // ✅ فقط از creator_vault که از تراکنش قربانی آمده استفاده می‌کنیم!

            // ═══════════════════════════════════════════════════════════
            // 💰 Priority Fees - Same as victim!
            // ═══════════════════════════════════════════════════════════
            let front_run_priority_fee = if tx_info.priority_fee > 0 {
                (tx_info.priority_fee as f64 * FRONT_RUN_FEE_MULTIPLIER) as u64
            } else {
                BASE_PRIORITY_FEE
            };

            let back_run_priority_fee = if tx_info.priority_fee > 0 {
                (tx_info.priority_fee as f64 * BACK_RUN_FEE_MULTIPLIER) as u64
            } else {
                BASE_PRIORITY_FEE
            };

            info!("💰 [W{}] Priority Fees:", worker_id);
            info!("   Victim: {} μLamp", tx_info.priority_fee);
            info!("   Front-run: {} μLamp", front_run_priority_fee);
            info!("   Back-run: {} μLamp", back_run_priority_fee);

            // ═══════════════════════════════════════════════════════════
            // 📦 Get blockhash
            // ═══════════════════════════════════════════════════════════
            let blockhash = match tx_builder.get_recent_blockhash().await {
                Ok(bh) => bh,
                Err(e) => {
                    error!("❌ [W{}] Failed to get blockhash: {}", worker_id, e);
                    stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            let jito_tip_account_str = jito_client.get_tip_account();
            let jito_tip_account = match Pubkey::from_str(jito_tip_account_str) {
                Ok(jta) => jta,
                Err(e) => {
                    error!("❌ [W{}] Invalid Jito tip account: {}", worker_id, e);
                    stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            // ═══════════════════════════════════════════════════════════
            // 🔨 Build transactions
            // ═══════════════════════════════════════════════════════════
            let front_tx = match tx_builder.build_front_run_transaction(
                &wallet_manager.front_runner,
                &mint,
                &bonding_curve,
                &creator_vault,  // ✅ از تراکنش قربانی!
                simulation.front_run_tokens,
                simulation.front_run_sol,
                front_run_priority_fee,
                blockhash,
            ).await {
                Ok(tx) => tx,
                Err(e) => {
                    error!("❌ [W{}] Failed to build front-run tx: {}", worker_id, e);
                    stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            let back_tx = match tx_builder.build_back_run_transaction(
                &wallet_manager.front_runner,
                &mint,
                &bonding_curve,
                &creator_vault,  // ✅ از تراکنش قربانی!
                simulation.front_run_tokens,
                simulation.back_run_sol.saturating_sub(simulation.total_fees),
                back_run_priority_fee,
                JITO_TIP_LAMPORTS,
                &jito_tip_account,
                blockhash,
            ).await {
                Ok(tx) => tx,
                Err(e) => {
                    error!("❌ [W{}] Failed to build back-run tx: {}", worker_id, e);
                    stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            // ═══════════════════════════════════════════════════════════
            // 🔬 Optional RPC SIMULATION
            // ═══════════════════════════════════════════════════════════
            if ENABLE_RPC_SIMULATION {
                info!("🔬 [W{}] Simulating transactions with RPC...", worker_id);

                // شبیه‌سازی Front-run
                match jito_client.simulate_transaction(&front_tx).await {
                    Ok(sim_result) => {
                        if let Some(err) = sim_result.err {
                            error!("❌ [W{}] Front-run simulation FAILED!", worker_id);
                            error!("   Error: {:?}", err);
                            if let Some(logs) = sim_result.logs {
                                error!("   📋 Logs:");
                                for log in logs.iter().take(15) {
                                    error!("   🔸 {}", log);
                                }
                            }
                            stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        info!("✅ [W{}] Front-run simulation SUCCESS", worker_id);
                        if let Some(units) = sim_result.units_consumed {
                            info!("   CU consumed: {}", units);
                        }
                    }
                    Err(e) => {
                        error!("❌ [W{}] Front-run simulation error: {}", worker_id, e);
                        stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }

                // شبیه‌سازی Back-run
                match jito_client.simulate_transaction(&back_tx).await {
                    Ok(sim_result) => {
                        if let Some(err) = sim_result.err {
                            error!("❌ [W{}] Back-run simulation FAILED!", worker_id);
                            error!("   Error: {:?}", err);
                            if let Some(logs) = sim_result.logs {
                                error!("   📋 Logs:");
                                for log in logs.iter().take(15) {
                                    error!("   🔸 {}", log);
                                }
                            }
                            stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }

                        info!("✅ [W{}] Back-run simulation SUCCESS", worker_id);
                        if let Some(units) = sim_result.units_consumed {
                            info!("   CU consumed: {}", units);
                        }
                    }
                    Err(e) => {
                        error!("❌ [W{}] Back-run simulation error: {}", worker_id, e);
                        stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }

                info!("🎉 [W{}] Both simulations PASSED!", worker_id);
            } else {
                info!("⚡ [W{}] Skipping RPC simulation", worker_id);
            }

            // ═══════════════════════════════════════════════════════════
            // 📤 ACTUAL BUNDLE SENDING
            // ═══════════════════════════════════════════════════════════
            info!("📦 [W{}] Sending bundle to Jito...", worker_id);
            info!("   🎯 Victim: {}", tx_info.signature);

            // Get transaction signatures for logging
            let front_tx_sig = bs58::encode(&front_tx.signatures[0]).into_string();
            let back_tx_sig = bs58::encode(&back_tx.signatures[0]).into_string();

            match jito_client.send_bundle_with_victim(
                vec![front_tx, back_tx],
                Some(tx_info.signature.clone()),
            ).await {
                Ok(bundle_id) => {
                    info!("✅ [W{}] Bundle sent!", worker_id);
                    info!("   Bundle ID: {}", bundle_id);
                    info!("   Tip: {:.6} SOL", JITO_TIP_LAMPORTS as f64 / LAMPORTS_PER_SOL as f64);
                    info!("   Expected profit: {:.6} SOL", simulation.net_profit as f64 / LAMPORTS_PER_SOL as f64);
                    stats.bundles_sent.fetch_add(1, Ordering::Relaxed);

                    // Log detailed bundle info
                    jito_client.log_bundle_details(
                        &bundle_id,
                        &tx_info.signature,
                        &front_tx_sig,
                        &back_tx_sig,
                    );

                    // Check bundle status after delay
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    match jito_client.check_bundle_status(vec![bundle_id.clone()]).await {
                        Ok(statuses) => {
                            for status in statuses {
                                info!("📊 [W{}] Bundle Status:", worker_id);
                                info!("   Bundle ID: {}", status.bundle_id);
                                info!("   Confirmation: {}", status.confirmation_status);
                                info!("   Slot: {}", status.slot);
                                if let Some(err) = status.err {
                                    error!("   ❌ Error: {:?}", err);
                                    error!("   This is why Jito rejected the bundle!");
                                } else {
                                    info!("   ✅ No error - bundle accepted!");
                                }
                            }
                        }
                        Err(e) => {
                            warn!("⚠️ [W{}] Could not check bundle status: {}", worker_id, e);
                        }
                    }
                }
                Err(e) => {
                    error!("❌ [W{}] Bundle failed: {}", worker_id, e);
                    stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            stats.unprofitable_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    info!("Worker {} stopped", worker_id);
}

// ═══════════════════════════════════════════════════════════════
// Helper Functions
// ═══════════════════════════════════════════════════════════════

fn parse_pool_data(data: &[u8]) -> Option<(PoolState, bool)> {
    if data.len() < 49 {
        return None;
    }

    if let Ok(curve) = PumpFunBondingCurve::try_from_slice(data) {
        if curve.discriminator == PUMP_FUN_DISCRIMINATOR {
            if curve.real_sol_reserves > 0 && curve.real_sol_reserves < 10_000_000 * LAMPORTS_PER_SOL {
                let state = PoolState {
                    pool_address: String::new(),
                    token_amount: curve.real_token_reserves,
                    sol_amount: curve.real_sol_reserves,
                    virtual_token_reserves: curve.virtual_token_reserves,
                    virtual_sol_reserves: curve.virtual_sol_reserves,
                    slot: 0,
                    last_update: Instant::now(),
                    total_volume_lamports: 0,
                };
                return Some((state, true));
            }
        }
    }

    if data.len() >= 40 {
        if let (Ok(vt), Ok(vs), Ok(rt), Ok(rs)) = (
            data[8..16].try_into().map(u64::from_le_bytes),
            data[16..24].try_into().map(u64::from_le_bytes),
            data[24..32].try_into().map(u64::from_le_bytes),
            data[32..40].try_into().map(u64::from_le_bytes),
        ) {
            if rs > 0 && rs < 10_000_000 * LAMPORTS_PER_SOL {
                let state = PoolState {
                    pool_address: String::new(),
                    token_amount: rt,
                    sol_amount: rs,
                    virtual_token_reserves: vt,
                    virtual_sol_reserves: vs,
                    slot: 0,
                    last_update: Instant::now(),
                    total_volume_lamports: 0,
                };
                return Some((state, false));
            }
        }
    }

    None
}

fn get_priority_fee(tx: &VersionedTransaction) -> u64 {
    let compute_budget_pubkey = Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_ID).unwrap();

    for instruction in tx.message.instructions() {
        if let Some(program_id) = tx.message.static_account_keys().get(instruction.program_id_index as usize) {
            if program_id == &compute_budget_pubkey && instruction.data.len() == 9 && instruction.data[0] == 3 {
                if let Ok(value_bytes) = instruction.data[1..9].try_into() {
                    return u64::from_le_bytes(value_bytes);
                }
            }
        }
    }
    0
}

// ✅ اصلاح شد: creator_address → creator_vault
fn extract_transaction_info(
    tx: VersionedTransaction,
    pump_fun_program_id: &Pubkey,
    buy_discriminator: &[u8],
    current_slot: u64,
) -> Option<TransactionInfo> {
    let account_keys = tx.message.static_account_keys();

    for instruction in tx.message.instructions() {
        if let Some(program_id) = account_keys.get(instruction.program_id_index as usize) {
            if program_id == pump_fun_program_id && instruction.data.starts_with(buy_discriminator) {
                if let Ok(args) = BuyInstructionArgs::try_from_slice(&instruction.data) {
                    if args.max_sol_cost >= MIN_SOL_COST && args.max_sol_cost <= MAX_SOL_COST {

                        let buyer_pubkey = account_keys.get(0);
                        let mint_pubkey = instruction.accounts.get(2)
                            .and_then(|&idx| account_keys.get(idx as usize));

                        // ✅ دریافت Creator Vault از account #9 (نه محاسبه!)
                        let creator_vault = instruction.accounts.get(9)
                            .and_then(|&idx| account_keys.get(idx as usize))
                            .map(|pk| pk.to_string());

                        let priority_fee = get_priority_fee(&tx);

                        if let (Some(buyer), Some(mint)) = (buyer_pubkey, mint_pubkey) {
                            // ✅ محاسبه bonding_curve و تبدیل به string برای DashMap
                            let bonding_curve_pk = derive_bonding_curve(mint);
                            let bonding_curve = bonding_curve_pk.to_string();

                            return Some(TransactionInfo {
                                buyer: buyer.to_string(),
                                mint: mint.to_string(),
                                bonding_curve,
                                max_sol: args.max_sol_cost,
                                token_amount: args.token_amount,
                                priority_fee,
                                signature: bs58::encode(&tx.signatures[0]).into_string(),
                                timestamp: Instant::now(),
                                slot: current_slot,
                                creator_vault,  // ✅ تغییر از creator_address
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

fn start_processing_thread(
    rx: Receiver<ShredsData>,
    worker_pool: Arc<WorkerPool>,
    stats: Arc<GlobalStats>,
) {
    let pump_fun_program_id = Pubkey::from_str(PUMP_FUN_PROGRAM_ID).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(b"global:buy");
    let buy_discriminator = hasher.finalize()[..8].to_vec();

    thread::spawn(move || {
        info!("Shreds processor started");

        for received_data in rx.iter() {
            stats.shreds_received.fetch_add(1, Ordering::Relaxed);

            if let Ok(entries) = bincode::deserialize::<Vec<Entry>>(&received_data.entries_raw) {
                let transactions: Vec<TransactionInfo> = entries
                    .into_par_iter()
                    .flat_map(|entry| {
                        entry.transactions
                            .into_par_iter()
                            .filter_map(|tx| {
                                extract_transaction_info(
                                    tx,
                                    &pump_fun_program_id,
                                    &buy_discriminator,
                                    received_data.slot,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();

                if !transactions.is_empty() {
                    info!("🎯 Found {} PumpFun buy transactions", transactions.len());
                }

                for tx_info in transactions.into_iter().rev() {
                    worker_pool.try_send_to_worker(tx_info);
                }
            }
        }
    });
}

async fn handle_account_update(msg: &GeyserSubscribeUpdate, pool_tracker: &PoolTracker, stats: &Arc<GlobalStats>) {
    if let Some(GeyserUpdateOneof::Account(account_info)) = &msg.update_oneof {
        if let Some(account) = &account_info.account {
            stats.geyser_updates.fetch_add(1, Ordering::Relaxed);

            let account_pubkey = bs58::encode(&account.pubkey).into_string();
            let data_len = account.data.len();

            if data_len < 49 || data_len > 512 {
                return;
            }

            if let Some((mut pool_state, _)) = parse_pool_data(&account.data) {
                if pool_state.sol_amount < MIN_POOL_SOL {
                    return;
                }

                pool_state.pool_address = account_pubkey.clone();
                pool_state.slot = account_info.slot;
                pool_state.last_update = Instant::now();

                if let Some(existing) = pool_tracker.get(&account_pubkey) {
                    pool_state.total_volume_lamports = existing.total_volume_lamports;
                }

                pool_tracker.insert(account_pubkey, pool_state);
            }
        }
    }
}

async fn run_shreds_task(
    endpoint: String,
    tx: Sender<ShredsData>,
) -> Result<()> {
    info!("Connecting to ShredStream: {}", endpoint);
    let mut client = ShredstreamClient::connect(&endpoint).await?;
    info!("Connected to ShredStream");

    let request = ShredstreamClient::create_empty_entries_request();
    let mut stream = client.subscribe_entries(request).await?;

    while let Some(message) = stream.message().await? {
        let data = ShredsData {
            slot: message.slot,
            entries_raw: message.entries,
        };
        let _ = tx.send(data);
    }

    Ok(())
}

async fn run_geyser_task(
    grpc_endpoint: String,
    x_token: Option<String>,
    request: GeyserSubscribeRequest,
    pool_tracker: PoolTracker,
    stats: Arc<GlobalStats>,
) -> Result<()> {
    let mut reconnect_count = 0;

    loop {
        let result: Result<()> = async {
            let mut client = retry(ExponentialBackoff::default(), || {
                let grpc_endpoint = grpc_endpoint.clone();
                let x_token = x_token.clone();
                async move {
                    let mut builder = GeyserGrpcClient::build_from_shared(grpc_endpoint.clone())?;
                    if let Some(token) = x_token {
                        builder = builder.x_token(Some(token))?;
                    }
                    if grpc_endpoint.starts_with("https://") {
                        builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
                    }
                    builder.connect().await.map_err(backoff::Error::transient)
                }
            }).await?;

            info!("Connected to Geyser");
            let (mut sink, mut stream) = client.subscribe().await?;
            sink.send(request.clone()).await?;
            reconnect_count = 0;

            while let Some(message) = stream.next().await {
                match message {
                    Ok(msg) => handle_account_update(&msg, &pool_tracker, &stats).await,
                    Err(e) => {
                        error!("Stream error: {:?}", e);
                        break;
                    }
                }
            }

            warn!("Stream ended");
            Ok(())
        }.await;

        if result.is_err() {
            reconnect_count += 1;
            tokio::time::sleep(Duration::from_secs(reconnect_count.min(30))).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();

    info!("═══════════════════════════════════════════════════════════");
    info!("MEV Bot v15.0 - CREATOR VAULT FIX 🚀");
    info!("Workers: {}", WORKER_COUNT);
    info!("RPC Simulation: {}", if ENABLE_RPC_SIMULATION { "ENABLED" } else { "DISABLED" });
    info!("═══════════════════════════════════════════════════════════");
    info!("🎯 STRATEGY:");
    info!("   Filter 1: Multi-block trades only");
    info!("   Filter 2: Min profit = 0.002 SOL");
    info!("   Slippage fill: 98% (aggressive!)");
    info!("   Priority fee: SAME as victim (stealth!)");
    info!("   ✅ Creator vault: FROM VICTIM TX (NO RPC!)");
    info!("   Target: Maximum speed & accuracy!");
    info!("═══════════════════════════════════════════════════════════");

    let wallet_manager = Arc::new(WalletManager::new(
        env::var("FRONT_RUNNER_KEYPAIR_PATH").context("FRONT_RUNNER_KEYPAIR_PATH missing")?,
        env::var("TOKEN_RECEIVER_KEYPAIR_PATH").context("TOKEN_RECEIVER_KEYPAIR_PATH missing")?,
        env::var("TIP_PAYER_KEYPAIR_PATH").context("TIP_PAYER_KEYPAIR_PATH missing")?
    ).context("Failed to initialize wallet manager")?);

    let (front_runner_address, token_receiver_address, tip_payer_address) = wallet_manager.get_addresses();
    info!("═══════════════════════════════════════════════════════════");
    info!("║ 🔐 WALLET CONFIGURATION:");
    info!("║    Front Runner: {}", front_runner_address);
    info!("║    Token Receiver: {}", token_receiver_address);
    info!("║    Tip Payer: {}", tip_payer_address);
    info!("╚═══════════════════════════════════════════════════════════");

    let shred_endpoint = env::var("SHRED_ENDPOINT").context("SHRED_ENDPOINT missing")?;
    let grpc_endpoint = env::var("GRPC_ENDPOINT").context("GRPC_ENDPOINT missing")?;
    let rpc_endpoint = env::var("SOLANA_RPC_ENDPOINT").context("SOLANA_RPC_ENDPOINT missing")?;

    let x_token = env::var("X_TOKEN").ok();

    let config_content = fs::read_to_string("config.json")?;
    let config: Config = serde_jsonc::from_str(&config_content)?;

    let request = GeyserSubscribeRequest {
        commitment: config.commitment.as_deref().map(commitment_from_str),
        transactions: config.transactions.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterTransactions::from(v)))
            .collect(),
        accounts: config.accounts.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterAccounts::from(v)))
            .collect(),
        slots: config.slots.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterSlots::from(v)))
            .collect(),
        blocks: config.blocks.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterBlocks::from(v)))
            .collect(),
        blocks_meta: config.blocks_meta.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterBlocksMeta::from(v)))
            .collect(),
        entry: config.entry.iter()
            .map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterEntry::from(v)))
            .collect(),
        transactions_status: Default::default(),
        accounts_data_slice: vec![],
        from_slot: None,
        ping: None,
    };

    let pool_tracker: PoolTracker = Arc::new(DashMap::with_capacity(10000));
    let stats = Arc::new(GlobalStats::new());

    let jito_client = Arc::new(JitoClient::new(rpc_endpoint.clone()));

    let tx_builder = Arc::new(TransactionBuilder::new(&rpc_endpoint));
    let recent_activity: RecentActivity = Arc::new(DashMap::new());

    let worker_pool = Arc::new(WorkerPool::new(
        WORKER_COUNT,
        pool_tracker.clone(),
        stats.clone(),
        jito_client.clone(),
        wallet_manager.clone(),
        tx_builder.clone(),
        recent_activity.clone(),
    ));

    let (shreds_tx, shreds_rx) = unbounded::<ShredsData>();
    start_processing_thread(shreds_rx, worker_pool.clone(), stats.clone());

    // Cleanup task
    let activity_for_cleanup = recent_activity.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));
        loop {
            interval.tick().await;
            cleanup_old_activity(&activity_for_cleanup);
        }
    });

    // Status Report
    let stats_report = stats.clone();
    let pool_tracker_report = pool_tracker.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;

            let uptime = stats_report.start_time.elapsed();
            let profitable = stats_report.profitable_count.load(Ordering::Relaxed);
            let unprofitable = stats_report.unprofitable_count.load(Ordering::Relaxed);
            let total_tx = stats_report.total_tx_processed.load(Ordering::Relaxed);
            let shreds = stats_report.shreds_received.load(Ordering::Relaxed);
            let geyser = stats_report.geyser_updates.load(Ordering::Relaxed);
            let total_profit = stats_report.total_profit_lamports.load(Ordering::Relaxed);
            let pools = pool_tracker_report.len();
            let bundles_sent = stats_report.bundles_sent.load(Ordering::Relaxed);
            let bundles_failed = stats_report.bundles_failed.load(Ordering::Relaxed);

            let skip_no_pool = stats_report.skipped_no_pool.load(Ordering::Relaxed);
            let skip_low_sol = stats_report.skipped_low_sol.load(Ordering::Relaxed);
            let skip_same_block = stats_report.skipped_same_block.load(Ordering::Relaxed);
            let skip_no_creator = stats_report.skipped_no_creator.load(Ordering::Relaxed);
            let total_skipped = skip_no_pool + skip_low_sol + skip_same_block + skip_no_creator;
            let total_simulated = profitable + unprofitable;

            info!("╔═══════════════════════════════════════════════════════════╗");
            info!("║ 📊 STATUS REPORT - Uptime: {:?}", uptime);
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ 📦 Data:");
            info!("║    Shreds: {} | Geyser: {} | Pools: {}", shreds, geyser, pools);
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ 🎯 Processing:");
            info!("║    Received: {}", total_tx);
            info!("║    Simulated: {} ({:.1}%)",
                  total_simulated,
                  if total_tx > 0 { (total_simulated as f64 / total_tx as f64) * 100.0 } else { 0.0 });
            info!("║    Skipped: {} ({:.1}%)",
                  total_skipped,
                  if total_tx > 0 { (total_skipped as f64 / total_tx as f64) * 100.0 } else { 0.0 });
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ ⏭️  Skip Breakdown:");
            info!("║    No pool: {} ({:.1}%)", skip_no_pool,
                  if total_skipped > 0 { (skip_no_pool as f64 / total_skipped as f64) * 100.0 } else { 0.0 });
            info!("║    Low SOL: {} ({:.1}%)", skip_low_sol,
                  if total_skipped > 0 { (skip_low_sol as f64 / total_skipped as f64) * 100.0 } else { 0.0 });
            info!("║    Same-block: {} ({:.1}%)", skip_same_block,
                  if total_skipped > 0 { (skip_same_block as f64 / total_skipped as f64) * 100.0 } else { 0.0 });
            info!("║    No creator: {} ({:.1}%)", skip_no_creator,
                  if total_skipped > 0 { (skip_no_creator as f64 / total_skipped as f64) * 100.0 } else { 0.0 });
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ 💰 Simulations:");
            info!("║    Profitable: {} ✅", profitable);
            info!("║    Unprofitable: {} ❌", unprofitable);
            info!("║    Success rate: {:.2}%",
                  if total_simulated > 0 {
                      (profitable as f64 / total_simulated as f64) * 100.0
                  } else { 0.0 });
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ 📤 Bundles:");
            info!("║    Sent: {} ✅", bundles_sent);
            info!("║    Failed: {} ❌", bundles_failed);
            info!("╠═══════════════════════════════════════════════════════════╣");
            info!("║ 💰 Potential Profit: {:.6} SOL",
                  total_profit as f64 / LAMPORTS_PER_SOL as f64);
            info!("╚═══════════════════════════════════════════════════════════╝");
        }
    });

    let geyser_handle = {
        let pool_tracker = pool_tracker.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            if let Err(e) = run_geyser_task(grpc_endpoint, x_token, request, pool_tracker, stats).await {
                error!("Geyser error: {:?}", e);
            }
        })
    };

    let shreds_handle = {
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_shreds_task(shred_endpoint.clone(), shreds_tx.clone()).await {
                    error!("Shreds error: {:?}", e);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    tokio::try_join!(geyser_handle, shreds_handle)?;

    Ok(())
}
