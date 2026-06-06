//! Per-pool price calibration: verify that RAM's swap output matches LiteSVM.
//!
//! Runs once after pool state is at least 60% warm. For every WSOL→token
//! edge in the pool registry it:
//!   1. Calls quote_edge() — what RAM's AMM math predicts.
//!   2. Builds a native swap instruction via native_ix::build_swap().
//!   3. Simulates the swap in LiteSVM with the test amount as input.
//!   4. Computes delta = sim_out − ram_out.
//!
//! Positive delta → RAM under-estimates (sim gives more tokens than RAM says).
//! Negative delta → RAM over-estimates (RAM says more tokens than LiteSVM gives).
//!
//! Output: /root/c/price_check (full per-pool table) + per-DEX summary to stderr.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::{
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};

use crate::{
    account_cache::AccountCache,
    arb_cycle::{build_edges, quote_edge},
    blockhash_cache::BlockhashCache,
    litesvm_sim::SimulatorPool,
    native_ix::{self, SPL_TOKEN},
    pool_state_store::PoolStateStore,
    pool_state_stream::PoolVaultPair,
    tokens::WSOL_MINT,
};

/// Test swap amount: 0.01 SOL = 10_000_000 lamports.
const TEST_AMOUNT: u64 = 10_000_000;
const REPORT_PATH: &str = "/root/c/price_check";

pub fn spawn_pool_calibrator(
    vault_pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    sim_pool: Arc<SimulatorPool>,
    sim_cache: Arc<AccountCache>,
    keypair: Arc<Keypair>,
    blockhash_cache: Arc<BlockhashCache>,
    cu_limit: u32,
) {
    tokio::spawn(async move {
        // Wait until at least 60% of pools are live in the store.
        loop {
            let live = store.live_pool_count();
            let total = store.pool_count;
            eprintln!("[calibrator] pool state: {live}/{total} live");
            if total > 0 && live * 100 / total >= 60 {
                eprintln!("[calibrator] warm enough — starting price check");
                break;
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }

        run_calibration(vault_pairs, store, sim_pool, sim_cache, keypair, blockhash_cache, cu_limit).await;
    });
}

struct CalibRecord {
    dex: &'static str,
    pool_short: String,
    mint_out_short: String,
    ram_out: Option<u64>,
    sim_out: Option<u64>,
    fail_msg: Option<String>,
}

async fn run_calibration(
    vault_pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    sim_pool: Arc<SimulatorPool>,
    sim_cache: Arc<AccountCache>,
    keypair: Arc<Keypair>,
    blockhash_cache: Arc<BlockhashCache>,
    cu_limit: u32,
) {
    let wsol_mint = Pubkey::from_str_const(WSOL_MINT);
    let user = keypair.pubkey();
    let blockhash = blockhash_cache.get();

    // Build all directed edges from the current store state.
    let edges = build_edges(&vault_pairs, &store);
    // Only test WSOL→token direction.
    let wsol_edges: Vec<_> = edges.iter().filter(|e| e.mint_in == wsol_mint).collect();

    eprintln!(
        "[calibrator] {} WSOL→X edges ({} total edges)",
        wsol_edges.len(),
        edges.len()
    );

    let mut records: Vec<CalibRecord> = Vec::new();

    for edge in &wsol_edges {
        let dex = edge.dex_kind.name();
        let pool_str = edge.pool.to_string();
        let pool_short = pool_str[..8.min(pool_str.len())].to_string();
        let mint_out_str = edge.mint_out.to_string();
        let mint_out_short = mint_out_str[..8.min(mint_out_str.len())].to_string();

        // Step 1: RAM's predicted output.
        let ram_out = quote_edge(edge, TEST_AMOUNT, &store);

        // Step 2: Build native swap instruction.
        let ix_result = native_ix::build_swap(
            dex,
            &edge.pool,
            &user,
            &edge.mint_in,
            &edge.mint_out,
            TEST_AMOUNT,
            1,
            &store,
        );

        let (sim_out, fail_msg) = match ix_result {
            Err(e) => (None, Some(format!("no_builder: {e}"))),
            Ok(swap_ix) => {
                let cu_ix = make_compute_budget_ix(cu_limit);
                let msg_result = v0::Message::try_compile(&user, &[cu_ix, swap_ix], &[], blockhash);
                let tx = msg_result.ok().and_then(|m| {
                    VersionedTransaction::try_new(VersionedMessage::V0(m), &[keypair.as_ref()]).ok()
                });

                match tx {
                    None => (None, Some("tx_compile_failed".to_string())),
                    Some(tx) => {
                        // Inject input WSOL ATA with test amount; output ATA empty.
                        let input_ata = spl_associated_token_account::get_associated_token_address(
                            &user, &wsol_mint,
                        );
                        let output_ata = spl_associated_token_account::get_associated_token_address(
                            &user, &edge.mint_out,
                        );
                        let mut overrides: HashMap<Pubkey, solana_account::Account> = HashMap::new();
                        overrides.insert(input_ata, make_token_account(&wsol_mint, &user, TEST_AMOUNT));
                        overrides.insert(output_ata, make_token_account(&edge.mint_out, &user, 0));

                        let sp = sim_pool.clone();
                        let sc = sim_cache.clone();
                        match tokio::task::spawn_blocking(move || {
                            sp.simulate_hop(&tx, &sc, &overrides, output_ata)
                        })
                        .await
                        {
                            Ok(Ok(o)) => (Some(o.out_balance), None),
                            Ok(Err(e)) => {
                                let s = e.to_string();
                                (None, Some(s[..s.len().min(120)].to_string()))
                            }
                            Err(e) => (None, Some(format!("spawn: {e}"))),
                        }
                    }
                }
            }
        };

        records.push(CalibRecord { dex, pool_short, mint_out_short, ram_out, sim_out, fail_msg });
    }

    write_and_print(&records);
}

fn write_and_print(records: &[CalibRecord]) {
    use std::fmt::Write as FmtWrite;

    // Accumulate per-DEX stats: (ok, fail, sum_delta, n_delta, min_delta, max_delta).
    let mut stats: HashMap<&'static str, (u64, u64, i64, u64, i64, i64)> = HashMap::new();
    for r in records {
        let s = stats.entry(r.dex).or_default();
        match (r.ram_out, r.sim_out) {
            (Some(ram), Some(sim)) => {
                let d = (sim as i64) - (ram as i64);
                s.0 += 1;
                s.2 += d;
                s.3 += 1;
                if s.3 == 1 || d < s.4 { s.4 = d; }
                if s.3 == 1 || d > s.5 { s.5 = d; }
            }
            _ => s.1 += 1,
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "=== Pool Calibration: {TEST_AMOUNT}L per swap, {} pools ===", records.len());
    let _ = writeln!(out, "delta = sim_out - ram_out  (+= RAM under-estimates, -= RAM over-estimates)");
    let _ = writeln!(out);
    let _ = writeln!(out, "--- DEX Summary ---");

    let mut dex_order: Vec<&'static str> = stats.keys().copied().collect();
    dex_order.sort_unstable();
    for dex in &dex_order {
        let (ok, fail, sum, n, mn, mx) = stats[dex];
        let avg = if n > 0 { sum / n as i64 } else { 0 };
        let _ = writeln!(
            out, "  {dex:<22} ok={ok:<4} fail={fail:<4} avg_delta={avg:>+8} min={mn:>+8} max={mx:>+8}"
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "--- Per-Pool Detail ---");
    for r in records {
        match (r.ram_out, r.sim_out) {
            (Some(ram), Some(sim)) => {
                let d = (sim as i64) - (ram as i64);
                let _ = writeln!(
                    out,
                    "  ok  {} {} tok={} ram={} sim={} delta={:+}",
                    r.dex, r.pool_short, r.mint_out_short, ram, sim, d
                );
            }
            (Some(ram), None) => {
                let msg = r.fail_msg.as_deref().unwrap_or("?");
                let _ = writeln!(
                    out,
                    "  ERR {} {} tok={} ram={} FAIL {}",
                    r.dex, r.pool_short, r.mint_out_short, ram,
                    &msg[..msg.len().min(100)]
                );
            }
            (None, _) => {
                let msg = r.fail_msg.as_deref().unwrap_or("?");
                let _ = writeln!(
                    out,
                    "  ERR {} {} tok={} NO_RAM_QUOTE {}",
                    r.dex, r.pool_short, r.mint_out_short,
                    &msg[..msg.len().min(100)]
                );
            }
        }
    }

    // Write to file.
    match std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(REPORT_PATH) {
        Ok(mut f) => {
            if f.write_all(out.as_bytes()).is_ok() {
                eprintln!("[calibrator] wrote {} bytes to {REPORT_PATH}", out.len());
            }
        }
        Err(e) => eprintln!("[calibrator] WARN: could not write {REPORT_PATH}: {e}"),
    }

    // Print summary to stderr.
    eprintln!("[calibrator] === DEX summary ===");
    for dex in &dex_order {
        let (ok, fail, sum, n, mn, mx) = stats[dex];
        let avg = if n > 0 { sum / n as i64 } else { 0 };
        eprintln!(
            "[calibrator]   {dex:<22} ok={ok} fail={fail} avg_delta={avg:+} min={mn:+} max={mx:+}"
        );
    }
}

fn make_compute_budget_ix(cu_limit: u32) -> Instruction {
    Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: {
            let mut d = vec![0x02u8];
            d.extend_from_slice(&cu_limit.to_le_bytes());
            d
        },
    }
}

fn make_token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> solana_account::Account {
    // SPL Token Account layout (165 bytes):
    //   [0..32]   mint
    //   [32..64]  owner
    //   [64..72]  amount (u64 LE)
    //   [108]     state = 1 (Initialized)
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(&mint.to_bytes());
    data[32..64].copy_from_slice(&owner.to_bytes());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    solana_account::Account {
        lamports: 2_039_280 + amount,
        data,
        owner: solana_address::Address::from(SPL_TOKEN.to_bytes()),
        executable: false,
        rent_epoch: u64::MAX,
    }
}
