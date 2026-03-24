mod arbitrage;
mod config;
mod jito;
mod metis;
mod rate_limiter;
mod tokens;
mod transaction;
mod wallet;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::Signer;
use std::sync::Arc;
use tracing::{error, info, warn};

use rate_limiter::RateLimiter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load config
    let config = config::Config::load("config.toml")?;
    info!("config loaded");

    // Load tokens
    let token_mints = tokens::load_tokens(&config.trading.tokens_file)?;
    info!(count = token_mints.len(), "tokens loaded");

    // Load keypairs
    let trading_keypair = wallet::read_keypair(&config.jito.trading_keypair)?;
    let auth_keypair = wallet::read_keypair(&config.jito.auth_keypair)?;
    info!(
        trading_wallet = %trading_keypair.pubkey(),
        auth_wallet = %auth_keypair.pubkey(),
        "keypairs loaded"
    );

    // Create RPC client
    let rpc_client = Arc::new(RpcClient::new(config.rpc.url.clone()));

    // Verify WSOL ATA exists (required because wrapAndUnwrapSol=false)
    let wsol_mint = solana_sdk::pubkey::Pubkey::from_str_const(tokens::WSOL_MINT);
    let wsol_ata = spl_associated_token_account::get_associated_token_address(
        &trading_keypair.pubkey(),
        &wsol_mint,
    );
    match rpc_client.get_account(&wsol_ata) {
        Ok(_) => info!(ata = %wsol_ata, "WSOL ATA verified"),
        Err(e) => {
            warn!(
                ata = %wsol_ata,
                error = %e,
                "WSOL ATA not found — create it before running!"
            );
        }
    }

    // Create Metis client (local self-hosted at 127.0.0.1:18080)
    let metis = metis::MetisClient::new(&config.metis.url, config.performance.quote_timeout_ms);

    // Connect to Jito gRPC with whitelisted auth keypair
    let mut jito_client = jito::JitoClient::connect(&config.jito.grpc_url, auth_keypair).await?;
    info!("connected to Jito gRPC");

    // Jito rate limiter: max N bundles per second, drop if exceeded (no queue)
    let mut jito_limiter = RateLimiter::new(config.jito.max_bundles_per_second);

    // Main loop — continuous scanning
    info!(
        tokens = token_mints.len(),
        min_sol = config.trading.min_amount_sol,
        max_sol = config.trading.max_amount_sol,
        step = config.trading.step_sol,
        "starting arbitrage scanner"
    );

    loop {
        for token_mint in &token_mints {
            if let Err(e) = arbitrage::scan_and_execute(
                &metis,
                token_mint,
                &config,
                &mut jito_client,
                &trading_keypair,
                &rpc_client,
                &mut jito_limiter,
            )
            .await
            {
                error!(token = token_mint.as_str(), error = %e, "scan error");
            }
        }
    }
}
