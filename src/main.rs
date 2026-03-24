mod arbitrage;
mod config;
mod jito;
mod metis;
mod tokens;
mod transaction;
mod wallet;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::Signer;
use std::sync::Arc;
use tracing::{error, info, warn};

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

    // Verify WSOL ATA exists
    let wsol_mint = solana_sdk::pubkey::Pubkey::from_str_const(tokens::WSOL_MINT);
    let wsol_ata = spl_associated_token_account::get_associated_token_address(
        &trading_keypair.pubkey(),
        &wsol_mint,
    );
    match rpc_client.get_account(&wsol_ata) {
        Ok(_) => info!(ata = %wsol_ata, "WSOL ATA exists"),
        Err(e) => {
            warn!(
                ata = %wsol_ata,
                error = %e,
                "WSOL ATA not found — please create it before running"
            );
        }
    }

    // Create Metis client
    let metis = metis::MetisClient::new(&config.metis.url, config.performance.quote_timeout_ms);

    // Connect to Jito
    let mut jito_client = jito::JitoClient::connect(&config.jito.grpc_url, auth_keypair).await?;
    info!("connected to Jito gRPC");

    // Main loop
    info!("starting arbitrage scanner");
    loop {
        for token_mint in &token_mints {
            match arbitrage::scan_token(&metis, token_mint, &config).await {
                Ok(Some(opp)) => {
                    info!(
                        token = %opp.token_mint,
                        profit = opp.profit_lamports,
                        "opportunity found, executing..."
                    );

                    match arbitrage::execute_opportunity(
                        &opp,
                        &metis,
                        &mut jito_client,
                        &trading_keypair,
                        &rpc_client,
                        &config,
                    )
                    .await
                    {
                        Ok(uuid) => {
                            info!(uuid = %uuid, "arbitrage executed");
                        }
                        Err(e) => {
                            warn!(error = %e, "arbitrage execution failed");
                        }
                    }
                }
                Ok(None) => {
                    // No opportunity for this token at this time
                }
                Err(e) => {
                    error!(token = token_mint, error = %e, "scan error");
                }
            }
        }
    }
}
