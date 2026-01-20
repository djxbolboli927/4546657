#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

use anyhow::{Context, Result};
use backoff::{future::retry, ExponentialBackoff};
use bincode;
use borsh::BorshDeserialize;
use bs58;
use crossbeam_channel::{unbounded, Receiver, Sender};
use dashmap::DashMap;
use dotenvy::dotenv;
use futures::{SinkExt, StreamExt};
use log::{error, info, warn, debug};
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
    yellowstone_grpc_client::ClientTlsConfig,
};
use std::{
    env, fs,
    str::FromStr,
    sync::{Arc, atomic::{AtomicUsize, AtomicU64, Ordering}},
    thread,
    time::{Duration, Instant},
};

mod config;
use config::{commitment_from_str, Config};

mod wallet_manager;
use wallet_manager::WalletManager;

mod jito_client;
use jito_client::{JitoClient, TargetTxStatus, TokenProgramType};

mod transaction_builder;
use transaction_builder::TransactionBuilder;

mod pumpfun_instructions;
use pumpfun_instructions::derive_bonding_curve;

mod spl_utils;

// 🌍 NEW: Leader Oracle Module
mod leader_oracle;
use leader_oracle::{LeaderOracle, GeoConfig, start_leader_schedule_updater};

// ═══════════════════════════════════════════════════════════════
// CONSTANTS
// ═══════════════════════════════════════════════════════════════

const MIN_SOL_COST: u64 = LAMPORTS_PER_SOL / 100;
const MAX_SOL_COST: u64 = LAMPORTS_PER_SOL * 10;

const MIN_POOL_SOL: u64 = LAMPORTS_PER_SOL;
const COMPUTE_BUDGET_PROGRAM_ID: &str = "ComputeBudget1111111111111111111111111111111";
const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const WORKER_COUNT: usize = 6;

const PUMPFUN_FEE_BPS: u64 = 100;
const FEE_DENOMINATOR: u64 = 10000;

const SANDWICH_MIN_PROFIT_LAMPORTS: u64 = LAMPORTS_PER_SOL / 500;
const SANDWICH_SAFETY_MARGIN: f64 = 0.90;
// ✅ Jito tip: 0.005 SOL (5,000,000 lamports)
const JITO_TIP_LAMPORTS: u64 = LAMPORTS_PER_SOL / 200;
const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];
const ESTIMATED_NETWORK_FEE: u64 = 5000;

const FRONT_RUN_FEE_MULTIPLIER: f64 = 1.0;
const BACK_RUN_FEE_MULTIPLIER: f64 = 1.0;
const BASE_PRIORITY_FEE: u64 = 50_000;

const PUMP_FUN_DISCRIMINATOR: [u8; 8] = [0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];

const CLEANUP_INTERVAL_SECS: u64 = 300;
const MAX_ACTIVITY_AGE_SECS: u64 = 600;

// ═══════════════════════════════════════════════════════════════
// DATA STRUCTURES
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
    max_sol: u64,
}

struct ShredsData {
    slot: u64,
    entries_raw: Vec<u8>,
}

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
    creator_vault: Option<String>,
    fee_recipient: Option<String>,
    bonding_curve_token_account: Option<String>,
    token_program_id: Option<String>,
    // ✅ NEW: ذخیره کل تراکنش برای شبیه‌سازی victim
    full_transaction: VersionedTransaction,
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
    bundles_landed: AtomicUsize,
    bundles_failed: AtomicUsize,
    skipped_no_pool: AtomicUsize,
    skipped_low_sol: AtomicUsize,
    skipped_same_block: AtomicUsize,
    skipped_no_creator: AtomicUsize,
    skipped_target_confirmed: AtomicUsize,
    skipped_simulation_failed: AtomicUsize,
    // ⏱️ Timing Filter
    skipped_late_tx: AtomicUsize, // تراکنش‌های بیش از 150ms قدیمی
    // 🌍 Leader Oracle Stats
    skipped_leader_outside_europe: AtomicUsize,
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
            bundles_landed: AtomicUsize::new(0),
            bundles_failed: AtomicUsize::new(0),
            skipped_no_pool: AtomicUsize::new(0),
            skipped_low_sol: AtomicUsize::new(0),
            skipped_same_block: AtomicUsize::new(0),
            skipped_no_creator: AtomicUsize::new(0),
            skipped_target_confirmed: AtomicUsize::new(0),
            skipped_simulation_failed: AtomicUsize::new(0),
            skipped_late_tx: AtomicUsize::new(0),
            skipped_leader_outside_europe: AtomicUsize::new(0),
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
        leader_oracle: Arc<LeaderOracle>, // 🌍 NEW
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
            let oracle_clone = leader_oracle.clone(); // 🌍 NEW

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
                        oracle_clone, // 🌍 NEW
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

#[derive(Debug, Clone)]
struct RecentTxActivity {
    last_buy_slot: Option<u64>,
    last_sell_slot: Option<u64>,
    last_activity: Instant,
}

type RecentActivity = Arc<DashMap<String, RecentTxActivity>>;

fn has_same_block_buy_sell(address: &str, mint: &str, current_slot: u64, recent_activity: &RecentActivity) -> bool {
    let key = format!("{}:{}", address, mint);
    if let Some(activity) = recent_activity.get(&key) {
        if let Some(sell_slot) = activity.last_sell_slot {
            if sell_slot == current_slot { return true; }
        }
        if let Some(buy_slot) = activity.last_buy_slot {
            if buy_slot == current_slot { return true; }
        }
    }
    false
}

fn record_buy_activity(address: &str, mint: &str, slot: u64, recent_activity: &RecentActivity) {
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
    recent_activity.retain(|_, activity| activity.last_activity >= cutoff);
}

fn calculate_token_out_with_fee(sol_in: u64, sol_reserve: u64, token_reserve: u64) -> u64 {
    let sol_in_after_fee = sol_in - (sol_in * PUMPFUN_FEE_BPS / FEE_DENOMINATOR);
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_sol = sol_reserve + sol_in_after_fee;
    let new_token = k / (new_sol as u128);
    token_reserve.saturating_sub(new_token as u64)
}

fn calculate_sol_in_with_fee(token_out: u64, sol_reserve: u64, token_reserve: u64) -> u64 {
    if token_out >= token_reserve { return u64::MAX; }
    let new_token = token_reserve - token_out;
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_sol = k / (new_token as u128);
    let sol_needed = (new_sol as u64).saturating_sub(sol_reserve);
    (sol_needed as u128 * FEE_DENOMINATOR as u128 / (FEE_DENOMINATOR - PUMPFUN_FEE_BPS) as u128) as u64
}

fn calculate_token_out_with_fee_swapped(token_in: u64, token_reserve: u64, sol_reserve: u64) -> u64 {
    let token_in_after_fee = token_in - (token_in * PUMPFUN_FEE_BPS / FEE_DENOMINATOR);
    let k = (sol_reserve as u128) * (token_reserve as u128);
    let new_token = token_reserve + token_in_after_fee;
    let new_sol = k / (new_token as u128);
    sol_reserve.saturating_sub(new_sol as u64)
}

fn simulate_sandwich_attack(victim_tx: &TransactionInfo, pool: &PoolState) -> SandwichSimulation {
    let v_sol = pool.virtual_sol_reserves;
    let v_token = pool.virtual_token_reserves;

    let victim_cost_without_frontrun = calculate_sol_in_with_fee(victim_tx.token_amount, v_sol, v_token);

    if victim_cost_without_frontrun >= victim_tx.max_sol {
        return SandwichSimulation {
            victim_tx: victim_tx.clone(),
            front_run_sol: 0, front_run_tokens: 0, victim_cost_after_frontrun: 0, back_run_sol: 0,
            gross_profit: 0, net_profit: 0, roi_percent: 0.0, total_fees: 0, is_profitable: false,
        };
    }

    let remaining_space = victim_tx.max_sol - victim_cost_without_frontrun;
    let target_frontrun_sol = (remaining_space as f64 * SANDWICH_SAFETY_MARGIN) as u64;

    let front_run_tokens = calculate_token_out_with_fee(target_frontrun_sol, v_sol, v_token);
    let frontrun_fee = target_frontrun_sol * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let v_sol_after_fr = v_sol + (target_frontrun_sol - frontrun_fee);
    let v_token_after_fr = v_token - front_run_tokens;

    let victim_cost_after_frontrun = calculate_sol_in_with_fee(victim_tx.token_amount, v_sol_after_fr, v_token_after_fr);

    if victim_cost_after_frontrun > victim_tx.max_sol {
        return SandwichSimulation {
            victim_tx: victim_tx.clone(),
            front_run_sol: target_frontrun_sol, front_run_tokens,
            victim_cost_after_frontrun, back_run_sol: 0, gross_profit: 0,
            net_profit: -(target_frontrun_sol as i64), roi_percent: -100.0,
            total_fees: frontrun_fee, is_profitable: false,
        };
    }

    let victim_fee = victim_cost_after_frontrun * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let v_sol_after_victim = v_sol_after_fr + (victim_cost_after_frontrun - victim_fee);
    let v_token_after_victim = v_token_after_fr - victim_tx.token_amount;

    let back_run_sol = calculate_token_out_with_fee_swapped(front_run_tokens, v_token_after_victim, v_sol_after_victim);
    let backrun_fee = back_run_sol * PUMPFUN_FEE_BPS / FEE_DENOMINATOR;
    let network_fees = ESTIMATED_NETWORK_FEE * 2;
    let total_fees = frontrun_fee + backrun_fee + JITO_TIP_LAMPORTS + network_fees;
    let gross_profit = (back_run_sol as i64) - (target_frontrun_sol as i64);
    let net_profit = gross_profit - (total_fees as i64);

    let roi_percent = if target_frontrun_sol > 0 { (net_profit as f64 / target_frontrun_sol as f64) * 100.0 } else { 0.0 };

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
    if profitable {
        info!("💰 W{} | Profit: {:.4} SOL | ROI: {:.1}% | Tx: ...{}",
            worker_id,
            sim.net_profit as f64 / LAMPORTS_PER_SOL as f64,
            sim.roi_percent,
            &sim.victim_tx.signature[sim.victim_tx.signature.len()-8..]
        );
    }
}

// ═══════════════════════════════════════════════════════════
// 🌍 WORKER THREAD with Leader Oracle Integration
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
    leader_oracle: Arc<LeaderOracle>, // 🌍 NEW
) {
    info!("Worker {} started 🚀 (with Leader Oracle)", worker_id);

    for tx_info in rx.iter() {
        stats.total_tx_processed.fetch_add(1, Ordering::Relaxed);

        // ═══════════════════════════════════════════════════════════
        // 🌍 LEADER ORACLE CHECK (HIGHEST PRIORITY)
        // ═══════════════════════════════════════════════════════════
        if !leader_oracle.can_trade(tx_info.slot).await {
            stats.skipped_leader_outside_europe.fetch_add(1, Ordering::Relaxed);
            debug!("⛔ Slot {}: Leader outside Europe - SKIPPING", tx_info.slot);
            continue;
        }

        // Pool check
        let pool_state = match pool_tracker.get(&tx_info.bonding_curve) {
            Some(pool) => pool.clone(),
            None => {
                stats.skipped_no_pool.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Cost check
        if tx_info.max_sol > MAX_SOL_COST {
            stats.skipped_low_sol.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // ⏱️ Timing filter: Skip transactions older than 150ms
        // (فقط 150 میلی‌ثانیه اول بلاک - بعد از آن دیر است)
        let tx_age = tx_info.timestamp.elapsed();
        if tx_age.as_millis() > 150 {
            stats.skipped_late_tx.fetch_add(1, Ordering::Relaxed);
            debug!("⏭️  Skipping late tx ({}ms old): ...{}", tx_age.as_millis(), &tx_info.signature[tx_info.signature.len()-8..]);
            continue;
        }

        // Same block check
        if has_same_block_buy_sell(&tx_info.buyer, &tx_info.mint, tx_info.slot, &recent_activity) {
            stats.skipped_same_block.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // ═══════════════════════════════════════════════════════════
        // 🧮 LOCAL PROFITABILITY SIMULATION (فقط محاسبه محلی)
        // ═══════════════════════════════════════════════════════════
        // Local Simulation
        let simulation = simulate_sandwich_attack(&tx_info, &pool_state);

        if simulation.front_run_sol == 0 || simulation.front_run_tokens == 0 {
            stats.skipped_simulation_failed.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let is_profit = simulation.is_profitable;
        if is_profit {
            stats.profitable_count.fetch_add(1, Ordering::Relaxed);
            print_simulation_result(&simulation, worker_id, true);
        } else {
            stats.unprofitable_count.fetch_add(1, Ordering::Relaxed);
            // ⏭️ فقط profitable ها را می‌فرستیم
            continue;
        }

        // Prepare Data
        let mint = match Pubkey::from_str(&tx_info.mint) {
            Ok(m) => m,
            Err(_) => {
                error!("   ❌ [Bundle] Mint parse failed");
                continue;
            }
        };

        let blockhash = match tx_builder.get_recent_blockhash().await {
            Ok(bh) => bh,
            Err(e) => {
                error!("   ❌ [Bundle] Blockhash fetch failed: {}", e);
                stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let creator_vault_str = match &tx_info.creator_vault {
            Some(cv) => cv,
            None => {
                error!("   ❌ [Bundle] No creator vault");
                stats.skipped_no_creator.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let creator_vault = match Pubkey::from_str(creator_vault_str) {
            Ok(cv) => cv,
            Err(_) => {
                error!("   ❌ [Bundle] Creator vault parse failed");
                continue;
            }
        };

        let token_program_id_str = match &tx_info.token_program_id {
            Some(tp) => tp,
            None => {
                error!("   ❌ [Bundle] No token program ID");
                continue;
            }
        };

        let token_program_type = if token_program_id_str == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb" {
            TokenProgramType::Token2022Program
        } else {
            TokenProgramType::TokenProgram
        };

        let token_program_id_pubkey = match Pubkey::from_str(token_program_id_str) {
            Ok(pk) => pk,
            Err(_) => {
                error!("   ❌ [Bundle] Token program ID parse failed");
                continue;
            }
        };

        let safe_front_run_sol = (simulation.front_run_sol as f64 * 1.70) as u64;

        // 🌍 انتخاب Jito endpoint بهینه (Frankfurt = بهترین برای سرور ما)
        let optimal_jito_endpoint = leader_oracle.get_optimal_jito_endpoint(tx_info.slot).await;

        // Build Transactions
        let front_tx = match tx_builder.build_front_run_transaction(
            &wallet_manager.front_runner,
            &mint,
            &creator_vault,
            simulation.front_run_tokens,
            safe_front_run_sol,
            50_000,
            blockhash,
            token_program_type,
            &token_program_id_pubkey,
        ).await {
            Ok(tx) => tx,
            Err(e) => {
                error!("   ❌ Front-Run Build Failed: {}", e);
                continue;
            }
        };

        // ═══════════════════════════════════════════════════════════
        // 🚀 REAL JITO BUNDLE SENDING (ارسال واقعی به Block Engine)
        // ═══════════════════════════════════════════════════════════

        // Parse Jito tip account
        let jito_tip_account = match JITO_TIP_ACCOUNTS[0].parse::<Pubkey>() {
            Ok(pk) => pk,
            Err(_) => {
                error!("   ❌ Invalid Jito tip account");
                continue;
            }
        };

        // Build back-run transaction with Jito tip
        let back_tx = match tx_builder.build_back_run_transaction(
            &wallet_manager.front_runner,
            &mint,
            &creator_vault,
            simulation.front_run_tokens,
            0, // min_sol_output
            50_000, // priority fee
            JITO_TIP_LAMPORTS, // 0.005 SOL tip
            &jito_tip_account,
            blockhash,
            token_program_type,
            &token_program_id_pubkey,
        ).await {
            Ok(tx) => tx,
            Err(e) => {
                error!("   ❌ Back-run build failed: {}", e);
                continue;
            }
        };

        // Create bundle: [front-run, back-run+tip] (victim NOT included)
        // Victim will execute naturally between our transactions
        let bundle = vec![
            VersionedTransaction::from(front_tx),
            VersionedTransaction::from(back_tx),
        ];

        // Send bundle to Jito Block Engine
        info!("🚀 Sending 2-tx bundle to Jito (tip: {} SOL)...", JITO_TIP_LAMPORTS as f64 / LAMPORTS_PER_SOL as f64);

        // Count as attempted (before send to track all tries)
        stats.bundles_sent.fetch_add(1, Ordering::Relaxed);

        let bundle_uuid = match jito_client.send_bundle_real(
            bundle,
            &optimal_jito_endpoint,
        ).await {
            Ok(uuid) => {
                info!("   ✅ Bundle accepted by Jito! UUID: {}", uuid);
                uuid
            }
            Err(e) => {
                error!("   ❌ Bundle rejected: {}", e);
                stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Wait briefly before checking status
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Check bundle status
        match jito_client.get_bundle_status(
            vec![bundle_uuid.clone()],
            &optimal_jito_endpoint
        ).await {
            Ok(statuses) => {
                if let Some(status) = statuses.first() {
                    match status.status.as_str() {
                        "Landed" => {
                            info!("   🎯 LANDED! Slot: {:?}", status.landed_slot);
                            stats.bundles_landed.fetch_add(1, Ordering::Relaxed);
                            if is_profit {
                                stats.total_profit_lamports.fetch_add(
                                    simulation.net_profit as u64,
                                    Ordering::Relaxed
                                );
                            }
                        }
                        "Failed" => {
                            error!("   ❌ Bundle FAILED");
                            stats.bundles_failed.fetch_add(1, Ordering::Relaxed);
                        }
                        "Pending" => {
                            info!("   ⏳ Bundle PENDING");
                        }
                        _ => {
                            info!("   ❓ Status: {}", status.status);
                        }
                    }
                }
            }
            Err(e) => {
                error!("   ⚠️  Status check error: {}", e);
            }
        }
    }
    info!("Worker {} stopped", worker_id);
}

// ═══════════════════════════════════════════════════════════
// Helper Functions (unchanged)
// ═══════════════════════════════════════════════════════════

fn parse_pool_data(data: &[u8]) -> Option<(PoolState, bool)> {
    if data.len() < 49 { return None; }
    if let Ok(curve) = PumpFunBondingCurve::try_from_slice(data) {
        if curve.discriminator == PUMP_FUN_DISCRIMINATOR {
            if curve.real_sol_reserves > 0 && curve.real_sol_reserves < 10_000_000 * LAMPORTS_PER_SOL {
                let state = PoolState {
                    pool_address: String::new(), token_amount: curve.real_token_reserves, sol_amount: curve.real_sol_reserves,
                    virtual_token_reserves: curve.virtual_token_reserves, virtual_sol_reserves: curve.virtual_sol_reserves,
                    slot: 0, last_update: Instant::now(), total_volume_lamports: 0,
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
                    pool_address: String::new(), token_amount: rt, sol_amount: rs,
                    virtual_token_reserves: vt, virtual_sol_reserves: vs,
                    slot: 0, last_update: Instant::now(), total_volume_lamports: 0,
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

fn extract_transaction_info(tx: VersionedTransaction, pump_fun_program_id: &Pubkey, buy_discriminator: &[u8], current_slot: u64) -> Option<TransactionInfo> {
    let account_keys = tx.message.static_account_keys();

    // Known Solana token program IDs
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    let token_2022_program = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();

    for instruction in tx.message.instructions() {
        if let Some(program_id) = account_keys.get(instruction.program_id_index as usize) {
            if program_id == pump_fun_program_id && instruction.data.starts_with(buy_discriminator) {
                if let Ok(args) = BuyInstructionArgs::try_from_slice(&instruction.data) {
                    if args.max_sol >= MIN_SOL_COST && args.max_sol <= MAX_SOL_COST {
                        let buyer_pubkey = account_keys.get(0);
                        let mint_pubkey = instruction.accounts.get(2).and_then(|&idx| account_keys.get(idx as usize));
                        let fee_recipient = instruction.accounts.get(1).and_then(|&idx| account_keys.get(idx as usize)).map(|pk| pk.to_string());
                        let bonding_curve = instruction.accounts.get(3).and_then(|&idx| account_keys.get(idx as usize)).map(|pk| pk.to_string());
                        let bonding_curve_token_account = instruction.accounts.get(4).and_then(|&idx| account_keys.get(idx as usize)).map(|pk| pk.to_string());
                        let creator_vault = instruction.accounts.get(9).and_then(|&idx| account_keys.get(idx as usize)).map(|pk| pk.to_string());
                        let priority_fee = get_priority_fee(&tx);

                        // 🔍 SMART TOKEN PROGRAM DETECTION
                        // Try multiple account indices and search through all accounts
                        let mut token_program_id: Option<String> = None;

                        // Strategy 1: Check common indices (7, 8, 10)
                        for idx in [7, 8, 10].iter() {
                            if let Some(account_idx) = instruction.accounts.get(*idx) {
                                if let Some(pubkey) = account_keys.get(*account_idx as usize) {
                                    if pubkey == &token_program || pubkey == &token_2022_program {
                                        token_program_id = Some(pubkey.to_string());
                                        break;
                                    }
                                }
                            }
                        }

                        // Strategy 2: If not found, search ALL instruction accounts
                        if token_program_id.is_none() {
                            for account_idx in &instruction.accounts {
                                if let Some(pubkey) = account_keys.get(*account_idx as usize) {
                                    if pubkey == &token_program || pubkey == &token_2022_program {
                                        token_program_id = Some(pubkey.to_string());
                                        break;
                                    }
                                }
                            }
                        }

                        // Strategy 3: Default to standard Token Program
                        if token_program_id.is_none() {
                            token_program_id = Some(token_program.to_string());
                        }

                        if let (Some(buyer), Some(mint), Some(bonding_curve)) = (buyer_pubkey, mint_pubkey, &bonding_curve) {
                            return Some(TransactionInfo {
                                buyer: buyer.to_string(), mint: mint.to_string(), bonding_curve: bonding_curve.clone(),
                                max_sol: args.max_sol, token_amount: args.token_amount, priority_fee,
                                signature: bs58::encode(&tx.signatures[0]).into_string(), timestamp: Instant::now(),
                                slot: current_slot, creator_vault, fee_recipient, bonding_curve_token_account, token_program_id,
                                full_transaction: tx,  // ✅ ذخیره کل تراکنش
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

fn start_processing_thread(rx: Receiver<ShredsData>, worker_pool: Arc<WorkerPool>, stats: Arc<GlobalStats>) {
    let pump_fun_program_id = Pubkey::from_str(PUMP_FUN_PROGRAM_ID).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(b"global:buy");
    let buy_discriminator = hasher.finalize()[..8].to_vec();

    thread::spawn(move || {
        info!("Shreds processor started");
        for received_data in rx.iter() {
            stats.shreds_received.fetch_add(1, Ordering::Relaxed);
            if let Ok(entries) = bincode::deserialize::<Vec<Entry>>(&received_data.entries_raw) {
                let transactions: Vec<TransactionInfo> = entries.into_par_iter()
                    .flat_map(|entry| {
                        entry.transactions.into_par_iter()
                            .filter_map(|tx| extract_transaction_info(tx, &pump_fun_program_id, &buy_discriminator, received_data.slot))
                            .collect::<Vec<_>>()
                    }).collect();

                for tx_info in transactions.into_iter().rev() {
                    worker_pool.try_send_to_worker(tx_info);
                }
            }
        }
    });
}

async fn handle_account_update(
    msg: &GeyserSubscribeUpdate,
    pool_tracker: &PoolTracker,
    stats: &Arc<GlobalStats>,
    slot_tx: &tokio::sync::watch::Sender<u64>,
) {
    if let Some(GeyserUpdateOneof::Account(account_info)) = &msg.update_oneof {
        if let Some(account) = &account_info.account {
            stats.geyser_updates.fetch_add(1, Ordering::Relaxed);
            let account_pubkey = bs58::encode(&account.pubkey).into_string();

            // 🌍 Update current slot for Leader Oracle
            let _ = slot_tx.send(account_info.slot);

            if let Some((mut pool_state, _)) = parse_pool_data(&account.data) {
                if pool_state.sol_amount < MIN_POOL_SOL { return; }
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

async fn run_shreds_task(endpoint: String, tx: Sender<ShredsData>) -> Result<()> {
    info!("Connecting to ShredStream: {}", endpoint);
    let mut client = ShredstreamClient::connect(&endpoint).await?;
    info!("Connected to ShredStream");
    let request = ShredstreamClient::create_empty_entries_request();
    let mut stream = client.subscribe_entries(request).await?;
    while let Some(message) = stream.message().await? {
        let _ = tx.send(ShredsData { slot: message.slot, entries_raw: message.entries });
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// 🚀 OPTIMIZED GEYSER TASK - Yellowstone v10/v11 + Agave v2.x
// ═══════════════════════════════════════════════════════════
// با TCP Keepalive، Aggressive Timeouts، و بازیابی سریع
async fn run_geyser_task(
    grpc_endpoint: String,
    x_token: Option<String>,
    request: GeyserSubscribeRequest,
    pool_tracker: PoolTracker,
    stats: Arc<GlobalStats>,
    slot_tx: tokio::sync::watch::Sender<u64>,
) -> Result<()> {
    let mut reconnect_count = 0;

    info!("🔧 Geyser connection mode: Yellowstone v10/v11 + Agave v2.x");
    info!("⚡ TCP Tuning: Keepalive=15s, ConnectTimeout=5s");

    loop {
        let result: Result<()> = async {
            // ═══════════════════════════════════════════════════════════
            // 🔌 AGGRESSIVE CONNECTION WITH TCP TUNING
            // ═══════════════════════════════════════════════════════════
            let mut client = retry(
                ExponentialBackoff {
                    max_elapsed_time: Some(Duration::from_secs(60)),
                    max_interval: Duration::from_secs(5),
                    ..Default::default()
                },
                || {
                    let grpc_endpoint = grpc_endpoint.clone();
                    let x_token = x_token.clone();
                    async move {
                        let mut builder = GeyserGrpcClient::build_from_shared(grpc_endpoint.clone())?;

                        // Auth token
                        if let Some(token) = x_token {
                            builder = builder.x_token(Some(token))?;
                        }

                        // TLS for HTTPS
                        if grpc_endpoint.starts_with("https://") {
                            builder = builder.tls_config(ClientTlsConfig::new().with_enabled_roots())?;
                        }

                        // ⚡ TCP TUNING: Aggressive timeouts
                        builder = builder
                            .connect_timeout(Duration::from_secs(5))
                            .timeout(Duration::from_secs(30))
                            .tcp_keepalive(Some(Duration::from_secs(15)))
                            .http2_keep_alive_interval(Duration::from_secs(10))
                            .keep_alive_timeout(Duration::from_secs(5))
                            .http2_adaptive_window(true);

                        builder.connect().await.map_err(backoff::Error::transient)
                    }
                },
            ).await?;

            info!("✅ Connected to Geyser with optimized TCP settings!");

            // ═══════════════════════════════════════════════════════════
            // 📡 SUBSCRIBE WITH STREAMING
            // ═══════════════════════════════════════════════════════════
            let (mut sink, mut stream) = client.subscribe().await?;
            sink.send(request.clone()).await?;

            info!("📡 Subscribed to Geyser stream (Yellowstone v10/v11)");

            // Reset reconnect counter on successful connection
            reconnect_count = 0;

            // ═══════════════════════════════════════════════════════════
            // 🔄 STREAM PROCESSING LOOP
            // ═══════════════════════════════════════════════════════════
            while let Some(message) = stream.next().await {
                match message {
                    Ok(msg) => {
                        // Process account updates with zero-copy optimizations
                        handle_account_update(&msg, &pool_tracker, &stats, &slot_tx).await;
                    }
                    Err(e) => {
                        error!("❌ Stream error: {:?}", e);
                        break;
                    }
                }
            }

            warn!("⚠️  Geyser stream ended unexpectedly");
            Ok(())
        }
        .await;

        // ═══════════════════════════════════════════════════════════
        // 🔄 RECONNECTION LOGIC
        // ═══════════════════════════════════════════════════════════
        if result.is_err() {
            reconnect_count += 1;
            let backoff_delay = (reconnect_count * 2).min(30);
            error!("🔄 Geyser reconnecting in {}s (attempt #{})...", backoff_delay, reconnect_count);
            tokio::time::sleep(Duration::from_secs(backoff_delay)).await;
        } else {
            // Stream ended cleanly, reconnect immediately
            warn!("🔄 Geyser stream closed cleanly, reconnecting...");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();

    info!("═══════════════════════════════════════════════════════════");
    info!("🌍 MEV BOT with LEADER ORACLE (Geographic-Aware Trading) 🌍");
    info!("═══════════════════════════════════════════════════════════");

    // ═══════════════════════════════════════════════════════════
    // 🌍 INITIALIZE LEADER ORACLE
    // ═══════════════════════════════════════════════════════════
    let erpc_leader_endpoint = env::var("ERPC_LEADER_API_ENDPOINT")
        .unwrap_or_else(|_| "https://edge.erpc.global".to_string());
    let erpc_api_key = env::var("ERPC_API_KEY")
        .context("ERPC_API_KEY missing in .env")?;

    // Parse geo config from environment
    let allowed_countries: Vec<String> = env::var("ALLOWED_COUNTRIES")
        .unwrap_or_else(|_| "DE,NL,FR,GB,CH,BE,PL,SE,FI".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let allowed_regions: Vec<String> = env::var("ALLOWED_REGIONS")
        .unwrap_or_else(|_| "Europe,EU".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let max_latency_ms: u64 = env::var("MAX_LATENCY_MS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .unwrap_or(30);

    let geo_config = GeoConfig {
        allowed_countries,
        allowed_regions,
        max_latency_ms,
    };

    let leader_oracle = Arc::new(LeaderOracle::new(
        &erpc_leader_endpoint,
        &erpc_api_key,
        geo_config,
    ));

    // Channel for current slot updates
    let (slot_tx, slot_rx) = tokio::sync::watch::channel::<u64>(0);

    // Start background leader schedule updater
    let oracle_clone = leader_oracle.clone();
    tokio::spawn(async move {
        start_leader_schedule_updater(oracle_clone, slot_rx).await;
    });

    // ═══════════════════════════════════════════════════════════
    // INITIALIZE WALLETS
    // ═══════════════════════════════════════════════════════════
    let wallet_manager = Arc::new(WalletManager::new(
        env::var("FRONT_RUNNER_KEYPAIR_PATH").context("FRONT_RUNNER_KEYPAIR_PATH missing")?,
        env::var("TOKEN_RECEIVER_KEYPAIR_PATH").context("TOKEN_RECEIVER_KEYPAIR_PATH missing")?,
        env::var("TIP_PAYER_KEYPAIR_PATH").context("TIP_PAYER_KEYPAIR_PATH missing")?
    ).context("Failed to initialize wallet manager")?);

    let (front_runner_address, _, _) = wallet_manager.get_addresses();
    info!("🔐 Wallet: {}", front_runner_address);

    // ═══════════════════════════════════════════════════════════
    // INITIALIZE CLIENTS
    // ═══════════════════════════════════════════════════════════
    let shred_endpoint = env::var("SHRED_ENDPOINT").context("SHRED_ENDPOINT missing")?;
    let grpc_endpoint = env::var("GRPC_ENDPOINT").context("GRPC_ENDPOINT missing")?;
    let rpc_endpoint = env::var("SOLANA_RPC_ENDPOINT").context("SOLANA_RPC_ENDPOINT missing")?;
    let x_token = env::var("X_TOKEN").ok();

    let config_content = fs::read_to_string("config.json")?;
    let config: Config = serde_jsonc::from_str(&config_content)?;

    let request = GeyserSubscribeRequest {
        commitment: config.commitment.as_deref().map(commitment_from_str),
        transactions: config.transactions.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterTransactions::from(v))).collect(),
        accounts: config.accounts.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterAccounts::from(v))).collect(),
        slots: config.slots.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterSlots::from(v))).collect(),
        blocks: config.blocks.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterBlocks::from(v))).collect(),
        blocks_meta: config.blocks_meta.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterBlocksMeta::from(v))).collect(),
        entry: config.entry.iter().map(|(k, v)| (k.clone(), GeyserSubscribeRequestFilterEntry::from(v))).collect(),
        transactions_status: Default::default(),
        accounts_data_slice: vec![],
        from_slot: None,
        ping: None,
    };

    let pool_tracker = Arc::new(DashMap::with_capacity(10000));
    let stats = Arc::new(GlobalStats::new());
    let jito_client = Arc::new(JitoClient::new(rpc_endpoint.clone()));
    let tx_builder = Arc::new(TransactionBuilder::new(&rpc_endpoint));
    let recent_activity = Arc::new(DashMap::new());

    // 🌍 Get current slot from RPC to initialize Leader Oracle
    info!("🔍 Fetching current slot from RPC...");
    let initial_slot = match tx_builder.rpc_client.get_slot() {
        Ok(slot) => {
            info!("✅ Current slot: {}", slot);
            slot
        }
        Err(e) => {
            warn!("⚠️  Failed to get current slot: {}, using 0", e);
            0
        }
    };

    // Send initial slot to Leader Oracle updater
    let _ = slot_tx.send(initial_slot);

    // ═══════════════════════════════════════════════════════════
    // INITIALIZE WORKER POOL (with Leader Oracle)
    // ═══════════════════════════════════════════════════════════
    let worker_pool = Arc::new(WorkerPool::new(
        WORKER_COUNT,
        pool_tracker.clone(),
        stats.clone(),
        jito_client.clone(),
        wallet_manager.clone(),
        tx_builder.clone(),
        recent_activity.clone(),
        leader_oracle.clone(), // 🌍 Pass oracle to workers
    ));

    let (shreds_tx, shreds_rx) = unbounded::<ShredsData>();
    start_processing_thread(shreds_rx, worker_pool.clone(), stats.clone());

    let activity_for_cleanup = recent_activity.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));
        loop { interval.tick().await; cleanup_old_activity(&activity_for_cleanup); }
    });

    // ═══════════════════════════════════════════════════════════
    // 📊 REPORTING TASK (هر 60 ثانیه)
    // ═══════════════════════════════════════════════════════════
    let stats_for_reporting = stats.clone();
    let oracle_for_reporting = leader_oracle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            print_detailed_report(&stats_for_reporting, &oracle_for_reporting).await;
        }
    });

    let geyser_handle = {
        let pool_tracker = pool_tracker.clone();
        let stats = stats.clone();
        let slot_tx_clone = slot_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_geyser_task(grpc_endpoint, x_token, request, pool_tracker, stats, slot_tx_clone).await {
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

// ═══════════════════════════════════════════════════════════
// 📊 DETAILED REPORTING FUNCTION
// ═══════════════════════════════════════════════════════════
async fn print_detailed_report(stats: &Arc<GlobalStats>, oracle: &Arc<LeaderOracle>) {
    let elapsed = stats.start_time.elapsed().as_secs();
    let hours = elapsed / 3600;
    let minutes = (elapsed % 3600) / 60;
    let seconds = elapsed % 60;

    // گرفتن آمار Leader Oracle
    let oracle_stats = oracle.get_cache_stats().await;
    let european_percent = if oracle_stats.total_slots > 0 {
        (oracle_stats.europe_count as f64 / oracle_stats.total_slots as f64) * 100.0
    } else {
        0.0
    };

    // آمار شبیه‌سازی
    let total_simulations = stats.profitable_count.load(Ordering::Relaxed)
        + stats.unprofitable_count.load(Ordering::Relaxed);
    let profitable = stats.profitable_count.load(Ordering::Relaxed);
    let unprofitable = stats.unprofitable_count.load(Ordering::Relaxed);
    let profitable_percent = if total_simulations > 0 {
        (profitable as f64 / total_simulations as f64) * 100.0
    } else {
        0.0
    };

    info!("╔═══════════════════════════════════════════════════════════════════════════════╗");
    info!("║                          📊 DETAILED PERFORMANCE REPORT                       ║");
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");
    info!("║  ⏱️  Uptime: {:02}:{:02}:{:02}                                                     ║", hours, minutes, seconds);
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");

    // Transaction Sources
    info!("║  📡 TRANSACTION SOURCES                                                       ║");
    info!("║     • ShredStream Received:  {:>10}                                       ║", stats.shreds_received.load(Ordering::Relaxed));
    info!("║     • Geyser Updates:        {:>10}                                       ║", stats.geyser_updates.load(Ordering::Relaxed));
    info!("║     • Total Processed:       {:>10}                                       ║", stats.total_tx_processed.load(Ordering::Relaxed));
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");

    // Leader Oracle Stats
    info!("║  🌍 LEADER ORACLE (Geographic Filtering)                                     ║");
    info!("║     • Total Slots Checked:   {:>10}                                       ║", oracle_stats.total_slots);
    info!("║     • European Leaders:      {:>10} ({:>5.1}%)                           ║", oracle_stats.europe_count, european_percent);
    info!("║     • Trades Blocked:        {:>10} (non-European leaders)               ║", stats.skipped_leader_outside_europe.load(Ordering::Relaxed));
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");

    // Local Simulation Stats
    info!("║  🧮 LOCAL SIMULATION (Profitability Analysis)                                ║");
    info!("║     • Total Simulations:     {:>10}                                       ║", total_simulations);
    info!("║     • Profitable:            {:>10} ({:>5.1}%)                           ║", profitable, profitable_percent);
    info!("║     • Unprofitable:          {:>10} ({:>5.1}%)                           ║", unprofitable, 100.0 - profitable_percent);
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");

    // Jito Bundle Stats
    let bundles_sent = stats.bundles_sent.load(Ordering::Relaxed);
    let bundles_landed = stats.bundles_landed.load(Ordering::Relaxed);
    let bundles_failed = stats.bundles_failed.load(Ordering::Relaxed);
    let bundle_success_rate = if bundles_sent > 0 {
        (bundles_landed as f64 / bundles_sent as f64) * 100.0
    } else {
        0.0
    };

    info!("║  📦 JITO BUNDLE STATS (Real Execution - 3-tx Sandwich)                       ║");
    info!("║     • Bundles Sent:          {:>10}                                       ║", bundles_sent);
    info!("║     • Bundles Landed:        {:>10} ({:>5.1}%)                           ║", bundles_landed, bundle_success_rate);
    info!("║     • Bundles Failed:        {:>10}                                       ║", bundles_failed);
    info!("╠═══════════════════════════════════════════════════════════════════════════════╣");

    // Skip Reasons
    info!("║  ⏭️  SKIP REASONS                                                             ║");
    info!("║     • No Pool Data:          {:>10}                                       ║", stats.skipped_no_pool.load(Ordering::Relaxed));
    info!("║     • Low SOL:               {:>10}                                       ║", stats.skipped_low_sol.load(Ordering::Relaxed));
    info!("║     • Late TX (>150ms):      {:>10}                                       ║", stats.skipped_late_tx.load(Ordering::Relaxed));
    info!("║     • Same Block:            {:>10}                                       ║", stats.skipped_same_block.load(Ordering::Relaxed));
    info!("║     • No Creator:            {:>10}                                       ║", stats.skipped_no_creator.load(Ordering::Relaxed));
    info!("║     • Target Confirmed:      {:>10}                                       ║", stats.skipped_target_confirmed.load(Ordering::Relaxed));
    info!("║     • Simulation Failed:     {:>10}                                       ║", stats.skipped_simulation_failed.load(Ordering::Relaxed));
    info!("╚═══════════════════════════════════════════════════════════════════════════════╝");
}
