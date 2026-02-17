// Atomic Arbitrage Bot: BisonFi <-> Tessera V (WSOL/USDC)
//
// Strategy:
//   - Subscribe to BisonFi WSOL/USDC pool and Tessera V authority via Geyser
//   - On each account update, check for cross-DEX price discrepancy
//   - Execute atomic circular arbitrage via Jupiter Metis + Jito bundles
//   - Use Lighthouse to guarantee minimum profit on-chain
//
// Bundle format: TX1 (swap + Lighthouse assertion) + TX2 (Jito tip, 75% of profit)
// Slippage = 0: transaction reverts at zero cost if exact output is not achieved

#![allow(dead_code)]
#![allow(unused_imports)]

use anyhow::{Context, Result};
use backoff::{future::retry, ExponentialBackoff};
use dotenvy::dotenv;
use futures::StreamExt;
use log::{debug, error, info, warn};
use serde_jsonc;
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
};
use solana_stream_sdk::{
    GeyserGrpcClient, GeyserSubscribeRequest, GeyserSubscribeRequestFilterAccounts,
    GeyserSubscribeRequestFilterBlocks, GeyserSubscribeRequestFilterBlocksMeta,
    GeyserSubscribeRequestFilterEntry, GeyserSubscribeRequestFilterSlots,
    GeyserSubscribeRequestFilterTransactions, GeyserSubscribeUpdate, GeyserUpdateOneof,
    yellowstone_grpc_client::ClientTlsConfig,
};
use std::{
    collections::HashMap,
    env, fs,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

mod config;
use config::{commitment_from_str, Config};

mod wallet_manager;
use wallet_manager::WalletManager;

mod jito_client;
use jito_client::JitoClient;

mod nextblock_client;
use nextblock_client::NextBlockClient;

mod leader_oracle;
use leader_oracle::{GeoConfig, LeaderOracle, start_leader_schedule_updater};

mod bisonfi;
use bisonfi::{
    calibrate_layout as bisonfi_calibrate, BisonFiLayout, BisonFiPool,
    BISONFI_POOL_ADDRESS, BISONFI_PROGRAM_ID,
};

mod tessera;
use tessera::{
    calibrate_layout as tessera_calibrate, TesseraLayout, TesseraPool,
    TESSERA_AUTHORITY, TESSERA_PROGRAM_ID,
};

mod jupiter_client;
use jupiter_client::{JupiterClient, METIS_BASE_URL};

mod arbitrage_engine;
use arbitrage_engine::{execute_arbitrage, log_stats, ArbitrageState, MIN_NET_PROFIT_LAMPORTS};

// ─── Constants ────────────────────────────────────────────────────────────────

const JITO_FRANKFURT_ENDPOINT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf";

/// Minimum interval between consecutive arbitrage attempts (milliseconds).
/// Prevents hammering on every single account update.
const MIN_ARB_INTERVAL_MS: u64 = 50;

/// How often to print statistics (seconds)
const STATS_INTERVAL_SECS: u64 = 30;

/// Account addresses we subscribe to
const BISONFI_POOL_ACCOUNT: &str = "51FQwjrvo8J8zXUaKyAznJ5NYpoiTCuqAqCu3HAMB9NZ";
const TESSERA_AUTHORITY_ACCOUNT: &str = "8ekCy2jHHUbW2yeNGFWYJT9Hm9FW7SvZcZK66dSZCDiF";

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();

    info!("═══════════════════════════════════════════");
    info!(" Atomic Arbitrage Bot v1.0.0               ");
    info!(" BisonFi <-> Tessera V | WSOL/USDC         ");
    info!("═══════════════════════════════════════════");

    // ── Load configuration ───────────────────────────────────────────────────
    let grpc_endpoint = env::var("GRPC_ENDPOINT")
        .context("GRPC_ENDPOINT not set")?;
    let rpc_endpoint = env::var("SOLANA_RPC_ENDPOINT")
        .context("SOLANA_RPC_ENDPOINT not set")?;
    let jito_endpoint = env::var("JITO_BLOCK_ENGINE_URL")
        .unwrap_or_else(|_| JITO_FRANKFURT_ENDPOINT.to_string());
    let wallet_path = env::var("WALLET_KEYPAIR_PATH")
        .context("WALLET_KEYPAIR_PATH not set — single wallet for arbitrage")?;

    let config_path = env::var("CONFIG_PATH").unwrap_or_else(|_| "config.json".to_string());
    let config_str = fs::read_to_string(&config_path)
        .context(format!("Failed to read {config_path}"))?;
    let config: Config = serde_jsonc::from_str(&config_str)
        .context("Failed to parse config.json")?;

    // ── Load wallet ──────────────────────────────────────────────────────────
    let wallet = Arc::new(
        read_keypair_file(&wallet_path)
            .map_err(|e| anyhow::anyhow!("Failed to read wallet from {wallet_path}: {e}"))?,
    );
    info!("Wallet: {}", wallet.pubkey());

    // ── Initialize clients ───────────────────────────────────────────────────
    let jito = Arc::new(JitoClient::new(rpc_endpoint.clone()));
    let jupiter = Arc::new(JupiterClient::new(METIS_BASE_URL));
    info!("Jupiter Metis: {METIS_BASE_URL}");
    info!("Jito endpoint: {jito_endpoint}");

    // Verify Jupiter Metis is reachable
    if !jupiter.health_check().await {
        warn!(
            "Jupiter Metis at {METIS_BASE_URL} is not reachable. \
            Start with: ALLOW_CIRCULAR_ARBITRAGE=true ./metis"
        );
    } else {
        info!("Jupiter Metis: OK");
    }

    // ── Initialize Leader Oracle ─────────────────────────────────────────────
    let erpc_api = env::var("ERPC_LEADER_API_ENDPOINT")
        .unwrap_or_else(|_| "https://edge.erpc.global".to_string());
    let erpc_api_key = env::var("ERPC_API_KEY").unwrap_or_default();
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
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    let geo_config = GeoConfig {
        allowed_regions,
        allowed_countries,
        max_latency_ms,
    };
    let oracle = Arc::new(LeaderOracle::new(
        &erpc_api,
        &erpc_api_key,
        geo_config,
    ));

    // ── Calibrate pool layouts at startup ────────────────────────────────────
    let bisonfi_layout = calibrate_bisonfi_layout(&rpc_endpoint).await;
    let tessera_layout = calibrate_tessera_layout(&rpc_endpoint).await;

    // ── Initialize shared arbitrage state ────────────────────────────────────
    let arb_state = ArbitrageState::new(bisonfi_layout, tessera_layout);

    // ── Shared slot counter (updated from Geyser slot updates) ───────────────
    // Use a watch channel so the leader oracle updater can subscribe to slot changes
    let (slot_tx, slot_rx) = tokio::sync::watch::channel(0u64);
    let current_slot = Arc::new(AtomicU64::new(0));

    // ── Start leader oracle updater with slot receiver ───────────────────────
    {
        let oracle_for_updater = oracle.clone();
        tokio::spawn(async move {
            start_leader_schedule_updater(oracle_for_updater, slot_rx).await;
        });
    }

    // ── Statistics timer ─────────────────────────────────────────────────────
    {
        let state = arb_state.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(STATS_INTERVAL_SECS));
            loop {
                interval.tick().await;
                log_stats(&state);
            }
        });
    }

    // ── Start Geyser subscription loop ───────────────────────────────────────
    info!("Connecting to Geyser: {grpc_endpoint}");
    info!(
        "Subscribing to: BisonFi={BISONFI_POOL_ACCOUNT}, TesseraV={TESSERA_AUTHORITY_ACCOUNT}"
    );

    let backoff = ExponentialBackoff {
        initial_interval: Duration::from_secs(1),
        max_interval: Duration::from_secs(30),
        max_elapsed_time: None,
        ..Default::default()
    };

    retry(backoff, || {
        let grpc_endpoint = grpc_endpoint.clone();
        let config = config.clone();
        let arb_state = arb_state.clone();
        let oracle = oracle.clone();
        let jito = jito.clone();
        let jupiter = jupiter.clone();
        let wallet = wallet.clone();
        let jito_endpoint = jito_endpoint.clone();
        let current_slot = current_slot.clone();
        let rpc_endpoint = rpc_endpoint.clone();
        let slot_tx = slot_tx.clone();

        async move {
            run_geyser_loop(
                &grpc_endpoint,
                config,
                arb_state,
                oracle,
                jito,
                jupiter,
                wallet,
                &jito_endpoint,
                current_slot,
                slot_tx,
                &rpc_endpoint,
            )
            .await
            .map_err(|e| {
                error!("Geyser loop error: {e}. Reconnecting...");
                backoff::Error::transient(e)
            })
        }
    })
    .await
}

// ─── Geyser stream loop ───────────────────────────────────────────────────────

async fn run_geyser_loop(
    grpc_endpoint: &str,
    config: Config,
    arb_state: Arc<ArbitrageState>,
    oracle: Arc<LeaderOracle>,
    jito: Arc<JitoClient>,
    jupiter: Arc<JupiterClient>,
    wallet: Arc<Keypair>,
    jito_endpoint: &str,
    current_slot: Arc<AtomicU64>,
    slot_tx: tokio::sync::watch::Sender<u64>,
    rpc_endpoint: &str,
) -> Result<()> {
    // Use GeyserGrpcBuilder to connect
    use solana_stream_sdk::yellowstone_grpc_client::GeyserGrpcBuilder;
    let x_token = env::var("X_TOKEN").ok();
    let mut builder = GeyserGrpcBuilder::from_shared(grpc_endpoint.to_string())
        .context("Failed to create Geyser builder")?;
    if let Some(token) = x_token {
        builder = builder
            .x_token(Some(token.as_str()))
            .context("Failed to set x-token")?;
    }
    let mut client = builder
        .connect()
        .await
        .context("Failed to connect to Geyser")?;

    // Build subscription request from config
    let (accounts_filter, slots_filter) = build_subscription_filters(&config);

    let subscribe_request = GeyserSubscribeRequest {
        slots: slots_filter,
        accounts: accounts_filter,
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(commitment_from_str(
            config.commitment.as_deref().unwrap_or("Processed"),
        )),
        accounts_data_slice: Vec::new(),
        ping: None,
        from_slot: None,
    };

    let (mut _sink, mut stream) = client.subscribe_with_request(Some(subscribe_request)).await?;
    info!("Geyser subscription active");

    let last_arb_ms = Arc::new(AtomicU64::new(0));

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(update) => {
                handle_update(
                    update,
                    &arb_state,
                    &oracle,
                    &jito,
                    &jupiter,
                    &wallet,
                    jito_endpoint,
                    &current_slot,
                    &slot_tx,
                    &last_arb_ms,
                )
                .await;
            }
            Err(e) => {
                error!("Geyser stream error: {e}");
                return Err(e.into());
            }
        }
    }

    warn!("Geyser stream ended unexpectedly");
    Err(anyhow::anyhow!("Geyser stream ended"))
}

// ─── Per-update handler ───────────────────────────────────────────────────────

async fn handle_update(
    update: GeyserSubscribeUpdate,
    arb_state: &Arc<ArbitrageState>,
    oracle: &Arc<LeaderOracle>,
    jito: &Arc<JitoClient>,
    jupiter: &Arc<JupiterClient>,
    wallet: &Arc<Keypair>,
    jito_endpoint: &str,
    current_slot: &Arc<AtomicU64>,
    slot_tx: &tokio::sync::watch::Sender<u64>,
    last_arb_ms: &Arc<AtomicU64>,
) {
    match update.update_oneof {
        // ── Slot update: track current slot ─────────────────────────────────
        Some(GeyserUpdateOneof::Slot(slot_update)) => {
            current_slot.store(slot_update.slot, Ordering::Relaxed);
            let _ = slot_tx.send(slot_update.slot);
        }

        // ── Account update: parse pool state ─────────────────────────────────
        Some(GeyserUpdateOneof::Account(account_update)) => {
            let slot = account_update.slot;
            current_slot.store(slot, Ordering::Relaxed);

            if let Some(account) = account_update.account {
                let pubkey_str = bs58_encode_pubkey(&account.pubkey);

                if pubkey_str == BISONFI_POOL_ACCOUNT {
                    arb_state.update_bisonfi(&account.data);
                } else if pubkey_str == TESSERA_AUTHORITY_ACCOUNT {
                    arb_state.update_tessera(&account.data);
                }

                // ── Check for arbitrage opportunity ──────────────────────────
                check_and_execute(
                    arb_state,
                    oracle,
                    jito,
                    jupiter,
                    wallet,
                    jito_endpoint,
                    current_slot,
                    last_arb_ms,
                )
                .await;
            }
        }

        _ => {}
    }
}

// ─── Opportunity check + execution ───────────────────────────────────────────

async fn check_and_execute(
    arb_state: &Arc<ArbitrageState>,
    oracle: &Arc<LeaderOracle>,
    jito: &Arc<JitoClient>,
    jupiter: &Arc<JupiterClient>,
    wallet: &Arc<Keypair>,
    jito_endpoint: &str,
    current_slot: &Arc<AtomicU64>,
    last_arb_ms: &Arc<AtomicU64>,
) {
    // Debounce: don't check more often than MIN_ARB_INTERVAL_MS
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last = last_arb_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < MIN_ARB_INTERVAL_MS {
        return;
    }

    // Check leader is in acceptable region
    let slot = current_slot.load(Ordering::Relaxed);
    if !oracle.can_trade(slot).await {
        debug!("Skip: leader not in acceptable region (slot={slot})");
        return;
    }

    // Check for opportunity
    let opp = match arb_state.check_opportunity() {
        Some(o) => o,
        None => return,
    };

    if opp.gross_profit_lamports < MIN_NET_PROFIT_LAMPORTS {
        return;
    }

    arb_state
        .opportunities_detected
        .fetch_add(1, Ordering::Relaxed);

    info!(
        "Opportunity: {} input={}L expected_gross_profit={}L",
        match opp.direction {
            bisonfi::ArbDirection::WsolBisonfiUsdcTessera => "WSOL→BisonFi→USDC→TesseraV",
            bisonfi::ArbDirection::WsolTesseraUsdcBisonfi => "WSOL→TesseraV→USDC→BisonFi",
        },
        opp.wsol_input,
        opp.gross_profit_lamports,
    );

    // Mark attempt time (before async execution to prevent concurrent attempts)
    last_arb_ms.store(now_ms, Ordering::Relaxed);

    // Set executing flag
    if arb_state
        .executing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return; // Another execution is in progress
    }

    // Fetch recent blockhash
    let recent_blockhash = match jito.get_latest_blockhash().await {
        Ok(bh) => bh,
        Err(e) => {
            error!("Failed to get blockhash: {e}");
            arb_state.executing.store(false, Ordering::Release);
            return;
        }
    };

    // Get optimal Jito endpoint based on leader location
    let endpoint = oracle.get_optimal_jito_endpoint(slot).await;

    // Execute the arbitrage
    let state_clone = arb_state.clone();
    let jito_clone = jito.clone();
    let jupiter_clone = jupiter.clone();
    let wallet_clone = wallet.clone();
    let endpoint_clone = endpoint.clone();

    tokio::spawn(async move {
        match execute_arbitrage(
            opp,
            state_clone.clone(),
            jupiter_clone,
            jito_clone,
            wallet_clone,
            recent_blockhash,
            &endpoint_clone,
            slot,
        )
        .await
        {
            Ok(bundle_id) => {
                state_clone
                    .bundles_submitted
                    .fetch_add(1, Ordering::Relaxed);
                info!("Bundle submitted: {bundle_id}");
            }
            Err(e) => {
                warn!("Arbitrage execution failed: {e}");
            }
        }
        state_clone.executing.store(false, Ordering::Release);
    });
}

// ─── Startup calibration ──────────────────────────────────────────────────────

/// Fetch BisonFi pool account and calibrate the layout.
/// Falls back to default layout if calibration fails.
async fn calibrate_bisonfi_layout(rpc_endpoint: &str) -> BisonFiLayout {
    info!("Calibrating BisonFi pool layout ({BISONFI_POOL_ACCOUNT})...");

    match fetch_account_data(rpc_endpoint, BISONFI_POOL_ACCOUNT).await {
        Ok(data) => {
            // We need the vault balances to calibrate.
            // Attempt to get them via token accounts.
            // For now, use heuristic: scan all u64 values for plausible reserve amounts.
            // A full BisonFi-specific calibration would cross-reference with vault balances.
            // The default layout (offset 72/80) covers many Anchor AMMs.
            match BisonFiPool::parse(&data, &BisonFiLayout::default()) {
                Some(pool) => {
                    info!(
                        "BisonFi pool parsed (default layout): WSOL={} USDC={} price=${:.2}/SOL",
                        pool.wsol_reserve,
                        pool.usdc_reserve,
                        pool.spot_price_usdc_per_wsol()
                    );
                    // If values are plausible (WSOL reserve > 0.1 SOL, USDC > $1)
                    // keep the default layout, otherwise warn.
                    if pool.wsol_reserve < 100_000_000 || pool.usdc_reserve < 1_000_000 {
                        warn!(
                            "BisonFi reserves look too small at default offsets. \
                            Manual calibration may be needed. Set BISONFI_WSOL_OFFSET and \
                            BISONFI_USDC_OFFSET env vars to override."
                        );
                    }
                    BisonFiLayout::default()
                }
                None => {
                    // Try env var overrides
                    let wsol_off: usize = env::var("BISONFI_WSOL_OFFSET")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(72);
                    let usdc_off: usize = env::var("BISONFI_USDC_OFFSET")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(80);
                    warn!(
                        "BisonFi default layout failed. Using offsets from env: wsol={wsol_off} usdc={usdc_off}"
                    );
                    BisonFiLayout {
                        wsol_reserve_offset: wsol_off,
                        usdc_reserve_offset: usdc_off,
                        fee_bps_offset: None,
                    }
                }
            }
        }
        Err(e) => {
            warn!("BisonFi calibration: could not fetch account data ({e}). Using default layout.");
            BisonFiLayout::default()
        }
    }
}

/// Fetch Tessera V authority account and calibrate the layout.
async fn calibrate_tessera_layout(rpc_endpoint: &str) -> TesseraLayout {
    info!("Calibrating Tessera V layout ({TESSERA_AUTHORITY_ACCOUNT})...");

    // Use expected price from env or a reasonable SOL price estimate
    let expected_price: f64 = env::var("EXPECTED_SOL_PRICE_USD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150.0); // fallback estimate

    match fetch_account_data(rpc_endpoint, TESSERA_AUTHORITY_ACCOUNT).await {
        Ok(data) => {
            match tessera_calibrate(&data, expected_price) {
                Ok(layout) => {
                    info!("Tessera V layout calibrated (expected_sol=${expected_price:.0})");
                    layout
                }
                Err(e) => {
                    warn!("Tessera V calibration failed: {e}. Using default layout.");
                    TesseraLayout::default()
                }
            }
        }
        Err(e) => {
            warn!("Tessera V calibration: could not fetch account ({e}). Using default layout.");
            TesseraLayout::default()
        }
    }
}

// ─── Geyser subscription filter builder ──────────────────────────────────────

fn build_subscription_filters(
    config: &Config,
) -> (
    HashMap<String, GeyserSubscribeRequestFilterAccounts>,
    HashMap<String, GeyserSubscribeRequestFilterSlots>,
) {
    let mut accounts = HashMap::new();
    let mut slots = HashMap::new();

    // Account filters from config.json
    for (name, filter) in &config.accounts {
        accounts.insert(
            name.clone(),
            GeyserSubscribeRequestFilterAccounts {
                account: filter.account.clone().unwrap_or_default(),
                owner: filter.owner.clone().unwrap_or_default(),
                filters: vec![],
                nonempty_txn_signature: None,
            },
        );
    }

    // Always subscribe to slots (for leader oracle slot tracking)
    slots.insert(
        "all_slots".to_string(),
        GeyserSubscribeRequestFilterSlots {
            filter_by_commitment: None,
            interslot_updates: Some(false),
        },
    );

    (accounts, slots)
}

// ─── RPC helpers ─────────────────────────────────────────────────────────────

/// Fetch raw account data from Solana RPC.
async fn fetch_account_data(rpc_endpoint: &str, address: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [
            address,
            { "encoding": "base64", "commitment": "processed" }
        ]
    });

    let resp: serde_json::Value = client
        .post(rpc_endpoint)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    let b64 = resp["result"]["value"]["data"][0]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No data for account {address}"))?;

    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    Ok(BASE64.decode(b64)?)
}

// ─── Pubkey encoding helper ───────────────────────────────────────────────────

fn bs58_encode_pubkey(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}
