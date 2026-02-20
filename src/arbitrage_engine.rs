/// Arbitrage detection and execution engine.
///
/// Connects BisonFi pool state + Tessera V oracle price to detect
/// cross-DEX arbitrage opportunities and execute them as Jito bundles.
///
/// Bundle structure (per friend's spec):
///   TX1: Jupiter swap instructions + Lighthouse balance assertion
///   TX2: Jito tip transfer (75% of net profit)
///
/// Slippage = 0: if exact output is not achieved, TX1 reverts with zero cost.
///
/// Minimum net profit threshold: MIN_NET_PROFIT_LAMPORTS (0.0001 SOL = 100,000 lamports).
/// Jito tip = 75% of net profit (above minimum).

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::bisonfi::{ArbDirection, ArbitrageOpportunity, BisonFiLayout, BisonFiPool};
use crate::jito_client::JitoClient;
use crate::jupiter_client::{
    collect_swap_instructions, decode_alt_addresses, JupiterClient, QuoteResponse,
    SwapInstructionsResponse,
};
use crate::tessera::{AtomicOraclePrice, TesseraLayout, TesseraPool};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Minimum net profit to execute arbitrage (0.0001 SOL = 100,000 lamports)
pub const MIN_NET_PROFIT_LAMPORTS: u64 = 100_000;

/// Percentage of net profit paid as Jito tip (75%)
pub const JITO_TIP_PERCENT: u64 = 75;

/// Maximum WSOL input amount for a single arbitrage (10 SOL)
pub const MAX_ARB_INPUT_LAMPORTS: u64 = 10 * 1_000_000_000;

/// Compute unit limit for TX1 (swap + Lighthouse)
pub const COMPUTE_UNIT_LIMIT: u32 = 800_000;

/// Lighthouse Program ID
pub const LIGHTHOUSE_PROGRAM_ID: &str = "L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95";

/// Jito tip accounts (8 addresses, load-balanced by slot)
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt13ib8T3s",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

// ─── Shared state ────────────────────────────────────────────────────────────

/// Shared arbitrage state, updated by the Geyser stream and read by workers.
pub struct ArbitrageState {
    /// Current BisonFi pool reserves (None until first update)
    pub bisonfi_pool: std::sync::RwLock<Option<BisonFiPool>>,
    /// Tessera V oracle price (atomic, lock-free reads)
    pub tessera_price: Arc<AtomicOraclePrice>,
    /// BisonFi account data layout (discovered at startup)
    pub bisonfi_layout: BisonFiLayout,
    /// Tessera V account data layout (discovered at startup)
    pub tessera_layout: TesseraLayout,
    /// Last time we detected and attempted an opportunity (to debounce)
    pub last_arb_attempt_ms: AtomicU64,
    /// Total opportunities detected
    pub opportunities_detected: AtomicU64,
    /// Total bundles submitted
    pub bundles_submitted: AtomicU64,
    /// Is the engine currently executing an arb? (prevents concurrent submissions)
    pub executing: AtomicBool,
}

impl ArbitrageState {
    pub fn new(bisonfi_layout: BisonFiLayout, tessera_layout: TesseraLayout) -> Arc<Self> {
        Arc::new(Self {
            bisonfi_pool: std::sync::RwLock::new(None),
            tessera_price: Arc::new(AtomicOraclePrice::default()),
            bisonfi_layout,
            tessera_layout,
            last_arb_attempt_ms: AtomicU64::new(0),
            opportunities_detected: AtomicU64::new(0),
            bundles_submitted: AtomicU64::new(0),
            executing: AtomicBool::new(false),
        })
    }

    /// Update BisonFi pool state from raw account data.
    pub fn update_bisonfi(&self, data: &[u8]) {
        match BisonFiPool::parse(data, &self.bisonfi_layout) {
            Some(pool) => {
                debug!(
                    "BisonFi pool update: WSOL={} USDC={} price=${:.4}/SOL",
                    pool.wsol_reserve,
                    pool.usdc_reserve,
                    pool.spot_price_usdc_per_wsol()
                );
                *self.bisonfi_pool.write().unwrap() = Some(pool);
            }
            None => {
                warn!("BisonFi account data could not be parsed — layout may need calibration");
            }
        }
    }

    /// Update Tessera V oracle price from raw account data.
    pub fn update_tessera(&self, data: &[u8]) {
        match TesseraPool::parse(data, &self.tessera_layout) {
            Some(pool) => {
                self.tessera_price.store(pool.oracle_price_per_lamport);
                debug!("Tessera V oracle update: ${:.4}/SOL", pool.usdc_per_sol());
            }
            None => {
                warn!("Tessera V account data could not be parsed — layout may need calibration");
            }
        }
    }

    /// Check if there's a profitable opportunity right now.
    /// Returns None if:
    ///   - Pool state is missing
    ///   - Oracle price is missing
    ///   - No profitable direction found
    ///   - Another execution is in progress
    pub fn check_opportunity(&self) -> Option<ArbitrageOpportunity> {
        if self.executing.load(Ordering::Acquire) {
            return None;
        }

        let oracle_price = self.tessera_price.load();
        if oracle_price <= 0.0 {
            return None;
        }

        let pool_guard = self.bisonfi_pool.read().unwrap();
        let pool = pool_guard.as_ref()?;

        pool.find_optimal_arbitrage(oracle_price, crate::tessera::TESSERA_FEE_BPS)
    }
}

// ─── Execution ───────────────────────────────────────────────────────────────

/// Execute a detected arbitrage opportunity as a Jito bundle.
///
/// Steps:
///   1. Get Jupiter Metis quote for the optimal amount (circular WSOL→WSOL)
///   2. Get swap instructions from Jupiter
///   3. Build TX1: compute budget + setup + swap + Lighthouse assertion
///   4. Build TX2: Jito tip (75% of net profit)
///   5. Simulate bundle via Jito RPC (free pre-verification)
///   6. Submit [TX1, TX2] to Jito block engine
pub async fn execute_arbitrage(
    opp: ArbitrageOpportunity,
    _state: Arc<ArbitrageState>,
    jupiter: Arc<JupiterClient>,
    jito: Arc<JitoClient>,
    wallet: Arc<Keypair>,
    recent_blockhash: Hash,
    jito_endpoint: &str,
    current_slot: u64,
) -> Result<String> {
    let start = Instant::now();

    // ── Step 1: Jupiter Metis quote with multi-amount probing ────────────────
    // Try multiple input amounts to find the most profitable trade size.
    // Jupiter routing may yield different net profits at different sizes.
    let probe_amounts = build_probe_amounts(opp.wsol_input);

    let mut best_quote: Option<QuoteResponse> = None;
    let mut best_net_profit: u64 = 0;

    for amount in &probe_amounts {
        match jupiter.quote_circular_wsol(*amount).await {
            Ok(q) => {
                let q_out = q.out_amount_u64();
                let q_in = q.in_amount_u64();
                if q_out > q_in {
                    let gross = q_out - q_in;
                    let tip = gross * JITO_TIP_PERCENT / 100;
                    let net = gross.saturating_sub(tip);
                    if net > best_net_profit {
                        best_net_profit = net;
                        best_quote = Some(q);
                    }
                }
            }
            Err(e) => {
                debug!("Probe amount {} failed: {e}", amount);
            }
        }
    }

    let quote = best_quote.ok_or_else(|| anyhow!("No profitable quote found across {} probe amounts", probe_amounts.len()))?;

    let quote_out = quote.out_amount_u64();
    let quote_in = quote.in_amount_u64();
    let gross_profit = quote_out - quote_in;
    let tip_lamports = gross_profit * JITO_TIP_PERCENT / 100;
    let net_profit = gross_profit.saturating_sub(tip_lamports);

    if net_profit < MIN_NET_PROFIT_LAMPORTS {
        return Err(anyhow!(
            "Net profit {net_profit} lamports below threshold {MIN_NET_PROFIT_LAMPORTS}"
        ));
    }

    info!(
        "Arbitrage: direction={} input={} expected_out={} gross_profit={} tip={} net_profit={}",
        match opp.direction {
            ArbDirection::WsolBisonfiUsdcTessera => "BisonFi→TesseraV",
            ArbDirection::WsolTesseraUsdcBisonfi => "TesseraV→BisonFi",
        },
        quote_in,
        quote_out,
        gross_profit,
        tip_lamports,
        net_profit
    );

    // ── Step 2: Swap instructions ────────────────────────────────────────────
    let swap_ixs = jupiter
        .swap_instructions(quote.clone(), &wallet.pubkey().to_string())
        .await
        .map_err(|e| anyhow!("Jupiter swap-instructions failed: {e}"))?;

    // ── Step 3: Build TX1 ────────────────────────────────────────────────────
    let alt_addresses = decode_alt_addresses(&swap_ixs);
    let alts = fetch_alts(&alt_addresses, &jito).await;

    let mut tx1_instructions: Vec<Instruction> = Vec::new();

    // Compute budget
    tx1_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        COMPUTE_UNIT_LIMIT,
    ));
    // No priority fee — we use the Jito tip for inclusion priority

    // All swap instructions from Jupiter
    let jup_ixs = collect_swap_instructions(&swap_ixs)
        .map_err(|e| anyhow!("Failed to collect swap instructions: {e}"))?;
    tx1_instructions.extend(jup_ixs);

    // Lighthouse balance assertion:
    // Assert that wallet SOL balance increased by at least net_profit after the swap.
    // This guarantees we don't execute at a loss even if oracle prices shift.
    if let Some(lighthouse_ix) = build_lighthouse_assertion(
        &wallet.pubkey(),
        net_profit,
    ) {
        tx1_instructions.push(lighthouse_ix);
    } else {
        warn!("Lighthouse assertion could not be built — proceeding without it");
    }

    // Build versioned message with ALTs
    let tx1_message = v0::Message::try_compile(
        &wallet.pubkey(),
        &tx1_instructions,
        &alts,
        recent_blockhash,
    )
    .map_err(|e| anyhow!("Failed to compile TX1 message: {e}"))?;

    let tx1 = VersionedTransaction::try_new(
        VersionedMessage::V0(tx1_message),
        &[wallet.as_ref()],
    )
    .map_err(|e| anyhow!("Failed to sign TX1: {e}"))?;

    // ── Step 4: Build TX2 (Jito tip) ─────────────────────────────────────────
    let tip_account = tip_account_for_slot(current_slot);
    let tip_ix = system_instruction::transfer(&wallet.pubkey(), &tip_account, tip_lamports);

    let tx2_message = v0::Message::try_compile(
        &wallet.pubkey(),
        &[tip_ix],
        &[],
        recent_blockhash,
    )
    .map_err(|e| anyhow!("Failed to compile TX2 message: {e}"))?;

    let tx2 = VersionedTransaction::try_new(
        VersionedMessage::V0(tx2_message),
        &[wallet.as_ref()],
    )
    .map_err(|e| anyhow!("Failed to sign TX2: {e}"))?;

    // ── Step 5: Simulate bundle (free pre-verification) ───────────────────────
    let bundle = vec![tx1, tx2];

    let sim_result = jito
        .simulate_bundle(bundle.clone(), None)
        .await
        .map_err(|e| anyhow!("Bundle simulation failed: {e}"))?;

    // Check each transaction in the simulation result
    for (i, tx_result) in sim_result.transaction_results.iter().enumerate() {
        if let Some(err) = &tx_result.err {
            return Err(anyhow!(
                "Bundle simulation TX{} failed: {:?}",
                i + 1,
                err
            ));
        }
    }

    debug!(
        "Bundle simulation passed ({} TXs OK) in {:.1}ms",
        sim_result.transaction_results.len(),
        start.elapsed().as_millis()
    );

    // ── Step 6: Submit bundle ─────────────────────────────────────────────────
    let bundle_id = jito
        .send_bundle_real(bundle, jito_endpoint)
        .await
        .map_err(|e| anyhow!("Jito bundle submission failed: {e}"))?;

    let elapsed = start.elapsed();
    info!(
        "Bundle submitted in {:.1}ms: id={bundle_id} net_profit={net_profit}L tip={tip_lamports}L",
        elapsed.as_millis()
    );

    Ok(bundle_id)
}

// ─── Lighthouse assertion ─────────────────────────────────────────────────────

/// Build a Lighthouse instruction that asserts the wallet's SOL balance has
/// increased by at least `min_increase_lamports` after the swap.
///
/// Lighthouse Protocol: https://github.com/jac0xb/lighthouse
/// Program ID: L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95
///
/// The lighthouse-sdk crate conflicts with solana-stream-sdk's version constraints,
/// so we construct the instruction manually using the known wire format.
///
/// Instruction: AssertAccountInfo with LamportsDelta assertion
///   [0..8]  discriminator = SHA256("global:assert_account_info")[..8]
///   [8]     assertion variant = 1 (Lamports delta)
///   [9]     operator = 5 (GreaterThanOrEqual)
///   [10..18] value = min_increase_lamports (u64 LE)
///
/// If execution reverts (wallet didn't gain enough lamports), the whole TX reverts
/// at zero cost — this is the atomic profit guarantee.
fn build_lighthouse_assertion(
    wallet: &Pubkey,
    min_increase_lamports: u64,
) -> Option<Instruction> {
    // Discriminator for AssertAccountInfo instruction
    // = first 8 bytes of SHA256("global:assert_account_info")
    // Computed from: echo -n "global:assert_account_info" | sha256sum
    // TODO: verify this matches the deployed Lighthouse v2 IDL discriminator
    const ASSERT_ACCOUNT_INFO_DISCRIMINATOR: [u8; 8] =
        [0x18, 0x0e, 0xf2, 0x9b, 0x44, 0xaa, 0xae, 0xe2];

    let mut data = Vec::with_capacity(18);
    data.extend_from_slice(&ASSERT_ACCOUNT_INFO_DISCRIMINATOR);
    data.push(1u8); // AccountInfoAssertion::LamportsDelta variant
    data.push(5u8); // IntegerOperator::GreaterThanOrEqual
    data.extend_from_slice(&min_increase_lamports.to_le_bytes());

    let accounts = vec![
        solana_sdk::instruction::AccountMeta::new_readonly(*wallet, false),
    ];

    Some(Instruction {
        program_id: Pubkey::from_str(LIGHTHOUSE_PROGRAM_ID)
            .expect("invalid lighthouse program id"),
        accounts,
        data,
    })
}

// ─── ALT fetching ────────────────────────────────────────────────────────────

/// Fetch Address Lookup Tables from on-chain via JitoClient's RPC.
/// Returns only successfully fetched ALTs.
async fn fetch_alts(
    addresses: &[Pubkey],
    jito: &JitoClient,
) -> Vec<AddressLookupTableAccount> {
    if addresses.is_empty() {
        return vec![];
    }
    // Fetch via RPC (JitoClient has an RPC client)
    let mut result = Vec::new();
    for addr in addresses {
        match jito.fetch_alt(addr).await {
            Ok(alt) => result.push(alt),
            Err(e) => warn!("Failed to fetch ALT {addr}: {e}"),
        }
    }
    result
}

// ─── Tip account selection ───────────────────────────────────────────────────

fn tip_account_for_slot(slot: u64) -> Pubkey {
    let idx = (slot as usize) % JITO_TIP_ACCOUNTS.len();
    Pubkey::from_str(JITO_TIP_ACCOUNTS[idx]).expect("invalid tip account")
}

// ─── Multi-amount probing ────────────────────────────────────────────────────

/// Build a set of probe amounts around the estimated optimal input.
/// Returns 5 amounts: 25%, 50%, 100%, 200%, and a fixed small amount (0.5 SOL).
/// All amounts are clamped to [0.01 SOL, MAX_ARB_INPUT_LAMPORTS].
fn build_probe_amounts(estimated_optimal: u64) -> Vec<u64> {
    let min_amount = 10_000_000u64; // 0.01 SOL
    let fixed_small = 500_000_000u64; // 0.5 SOL

    let mut amounts = vec![
        estimated_optimal / 4,
        estimated_optimal / 2,
        estimated_optimal,
        estimated_optimal.saturating_mul(2),
        fixed_small,
    ];

    // Deduplicate, clamp, and sort
    amounts.sort();
    amounts.dedup();
    amounts.retain(|&a| a >= min_amount && a <= MAX_ARB_INPUT_LAMPORTS);

    if amounts.is_empty() {
        amounts.push(fixed_small.min(MAX_ARB_INPUT_LAMPORTS));
    }

    amounts
}

// ─── Statistics ──────────────────────────────────────────────────────────────

pub fn log_stats(state: &ArbitrageState) {
    info!(
        "Arb stats: opportunities={} bundles_submitted={}",
        state.opportunities_detected.load(Ordering::Relaxed),
        state.bundles_submitted.load(Ordering::Relaxed),
    );
}
