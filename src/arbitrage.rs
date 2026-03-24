use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{signature::Keypair, signer::Signer};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::jito::JitoClient;
use crate::metis::MetisClient;
use crate::tokens::WSOL_MINT;
use crate::transaction;

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Represents a profitable arbitrage opportunity.
#[derive(Debug)]
pub struct Opportunity {
    pub token_mint: String,
    pub input_lamports: u64,
    pub output_lamports: u64,
    pub profit_lamports: u64,
    pub tip_lamports: u64,
}

/// Scan a single token for arbitrage opportunities across the stepped amount range.
/// Returns the best opportunity found (highest profit), if any.
pub async fn scan_token(
    metis: &MetisClient,
    token_mint: &str,
    config: &Config,
) -> Result<Option<Opportunity>> {
    let min_lamports = (config.trading.min_amount_sol * LAMPORTS_PER_SOL) as u64;
    let max_lamports = (config.trading.max_amount_sol * LAMPORTS_PER_SOL) as u64;
    let step_lamports = (config.trading.step_sol * LAMPORTS_PER_SOL) as u64;

    let mut best: Option<Opportunity> = None;

    let mut amount = min_lamports;
    while amount <= max_lamports {
        // Leg 1: WSOL → Token
        let quote1 = match metis.get_quote(WSOL_MINT, token_mint, amount).await {
            Ok(q) => q,
            Err(e) => {
                debug!(token = token_mint, amount, error = %e, "leg1 quote failed");
                amount += step_lamports;
                continue;
            }
        };

        let token_amount: u64 = quote1
            .out_amount
            .parse()
            .unwrap_or(0);
        if token_amount == 0 {
            amount += step_lamports;
            continue;
        }

        // Leg 2: Token → WSOL
        let quote2 = match metis.get_quote(token_mint, WSOL_MINT, token_amount).await {
            Ok(q) => q,
            Err(e) => {
                debug!(token = token_mint, amount, error = %e, "leg2 quote failed");
                amount += step_lamports;
                continue;
            }
        };

        let output_wsol: u64 = quote2
            .out_amount
            .parse()
            .unwrap_or(0);

        // Check profitability
        if output_wsol > amount {
            let raw_profit = output_wsol - amount;
            let tip = transaction::calculate_tip(
                raw_profit,
                config.jito.tip_profit_percent,
                config.jito.tip_min_lamports,
                config.jito.tip_max_lamports,
            );

            // Net profit after tip
            if raw_profit > tip + config.trading.min_profit_lamports {
                let net_profit = raw_profit - tip;
                info!(
                    token = token_mint,
                    input_sol = amount as f64 / LAMPORTS_PER_SOL,
                    output_sol = output_wsol as f64 / LAMPORTS_PER_SOL,
                    profit_lamports = net_profit,
                    tip_lamports = tip,
                    "profitable opportunity found"
                );

                let opp = Opportunity {
                    token_mint: token_mint.to_string(),
                    input_lamports: amount,
                    output_lamports: output_wsol,
                    profit_lamports: net_profit,
                    tip_lamports: tip,
                };

                if best
                    .as_ref()
                    .map_or(true, |b| net_profit > b.profit_lamports)
                {
                    best = Some(opp);
                }
            }
        }

        amount += step_lamports;
    }

    Ok(best)
}

/// Execute an arbitrage opportunity: fetch swap instructions, build tx, send bundle.
pub async fn execute_opportunity(
    opp: &Opportunity,
    metis: &MetisClient,
    jito: &mut JitoClient,
    trading_keypair: &Keypair,
    rpc_client: &RpcClient,
    _config: &Config,
) -> Result<String> {
    let user_pubkey = trading_keypair.pubkey().to_string();

    // Re-quote to get fresh data
    let quote1 = metis
        .get_quote(WSOL_MINT, &opp.token_mint, opp.input_lamports)
        .await?;

    let token_amount: u64 = quote1.out_amount.parse()?;
    let quote2 = metis
        .get_quote(&opp.token_mint, WSOL_MINT, token_amount)
        .await?;

    // Verify still profitable
    let output_wsol: u64 = quote2.out_amount.parse()?;
    if output_wsol <= opp.input_lamports {
        anyhow::bail!("opportunity no longer profitable on re-quote");
    }

    // Get swap instructions for both legs
    let swap_ixs1 = metis.get_swap_instructions(&user_pubkey, &quote1).await?;
    let swap_ixs2 = metis.get_swap_instructions(&user_pubkey, &quote2).await?;

    // Get recent blockhash
    let recent_blockhash = rpc_client.get_latest_blockhash()?;

    // Build the versioned transaction
    let tx = transaction::build_arb_transaction(
        &swap_ixs1,
        &swap_ixs2,
        trading_keypair,
        opp.tip_lamports,
        recent_blockhash,
        rpc_client,
        None, // no priority fee — we rely on Jito tip
    )?;

    // Check transaction size
    let tx_bytes = bincode::serialize(&tx)?;
    if tx_bytes.len() > 1232 {
        warn!(
            size = tx_bytes.len(),
            "transaction exceeds 1232 bytes, skipping"
        );
        anyhow::bail!(
            "transaction too large: {} bytes (max 1232)",
            tx_bytes.len()
        );
    }

    info!(
        token = opp.token_mint,
        profit = opp.profit_lamports,
        tip = opp.tip_lamports,
        tx_size = tx_bytes.len(),
        "sending bundle to Jito"
    );

    // Send bundle
    let uuid = jito.send_bundle(&tx).await?;
    info!(uuid = %uuid, "bundle sent, awaiting confirmation");

    // Poll for bundle status (best-effort)
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match jito.get_bundle_status(&uuid).await? {
            Some(status) if status == "Landed" || status == "landed" => {
                info!(uuid = %uuid, "bundle landed successfully");
                return Ok(uuid);
            }
            Some(status) if status.contains("Failed") || status.contains("failed") => {
                warn!(uuid = %uuid, status = %status, "bundle failed");
                anyhow::bail!("bundle failed: {}", status);
            }
            Some(status) => {
                debug!(uuid = %uuid, status = %status, "bundle pending");
            }
            None => {
                debug!(uuid = %uuid, "no status yet");
            }
        }
    }

    Ok(uuid)
}
