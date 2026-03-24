use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{signature::Keypair, signer::Signer};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::jito::JitoClient;
use crate::metis::{MetisClient, QuoteResponse};
use crate::rate_limiter::RateLimiter;
use crate::tokens::WSOL_MINT;
use crate::transaction;

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Represents a profitable arbitrage opportunity with quotes ready for execution.
pub struct Opportunity {
    token_mint: String,
    #[allow(dead_code)]
    input_lamports: u64,
    #[allow(dead_code)]
    profit_lamports: u64,
    tip_lamports: u64,
    pub quote_leg1: QuoteResponse,
    pub quote_leg2: QuoteResponse,
}

/// Scan a single token across stepped amounts and execute immediately when profitable.
///
/// Key design: NO re-quote. When a profitable pair of quotes is found,
/// swap instructions are fetched and the bundle is sent immediately.
/// Speed is everything — quotes go stale in milliseconds.
pub async fn scan_and_execute(
    metis: &MetisClient,
    token_mint: &str,
    config: &Config,
    jito: &mut JitoClient,
    trading_keypair: &Keypair,
    rpc_client: &RpcClient,
    jito_limiter: &mut RateLimiter,
) -> Result<()> {
    let min_lamports = (config.trading.min_amount_sol * LAMPORTS_PER_SOL) as u64;
    let max_lamports = (config.trading.max_amount_sol * LAMPORTS_PER_SOL) as u64;
    let step_lamports = (config.trading.step_sol * LAMPORTS_PER_SOL) as u64;

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

        let token_amount: u64 = match quote1.out_amount.parse() {
            Ok(v) if v > 0 => v,
            _ => {
                amount += step_lamports;
                continue;
            }
        };

        // Leg 2: Token → WSOL
        let quote2 = match metis.get_quote(token_mint, WSOL_MINT, token_amount).await {
            Ok(q) => q,
            Err(e) => {
                debug!(token = token_mint, amount, error = %e, "leg2 quote failed");
                amount += step_lamports;
                continue;
            }
        };

        let output_wsol: u64 = quote2.out_amount.parse().unwrap_or(0);

        // Check profitability: output must exceed input + min_profit
        if output_wsol > amount {
            let raw_profit = output_wsol - amount;
            let tip = transaction::calculate_tip(
                raw_profit,
                config.jito.tip_profit_percent,
                config.jito.tip_min_lamports,
                config.jito.tip_max_lamports,
            );

            // Net profit after tip must exceed minimum
            if raw_profit > tip + config.trading.min_profit_lamports {
                let net_profit = raw_profit - tip;

                // Check Jito rate limit — if exceeded, DROP immediately (no queue)
                if !jito_limiter.try_acquire() {
                    warn!(
                        token = token_mint,
                        profit = net_profit,
                        "jito rate limit hit, dropping opportunity"
                    );
                    amount += step_lamports;
                    continue;
                }

                info!(
                    token = token_mint,
                    input_sol = amount as f64 / LAMPORTS_PER_SOL,
                    output_sol = output_wsol as f64 / LAMPORTS_PER_SOL,
                    profit_lamports = net_profit,
                    tip_lamports = tip,
                    "PROFITABLE — executing immediately"
                );

                let opp = Opportunity {
                    token_mint: token_mint.to_string(),
                    input_lamports: amount,
                    profit_lamports: net_profit,
                    tip_lamports: tip,
                    quote_leg1: quote1,
                    quote_leg2: quote2,
                };

                // Execute immediately — no re-quote, speed is critical
                match execute_opportunity(&opp, metis, jito, trading_keypair, rpc_client).await {
                    Ok(uuid) => {
                        info!(uuid = %uuid, token = token_mint, profit = net_profit, "bundle sent");
                    }
                    Err(e) => {
                        warn!(error = %e, token = token_mint, "execution failed");
                    }
                }
            }
        }

        amount += step_lamports;
    }

    Ok(())
}

/// Execute an arbitrage opportunity using the already-obtained quotes.
/// NO re-quote — uses the exact quotes from scanning for maximum speed.
async fn execute_opportunity(
    opp: &Opportunity,
    metis: &MetisClient,
    jito: &mut JitoClient,
    trading_keypair: &Keypair,
    rpc_client: &RpcClient,
) -> Result<String> {
    let user_pubkey = trading_keypair.pubkey().to_string();

    // Get swap instructions for both legs using the SAME quotes (no re-quote)
    let swap_ixs1 = metis
        .get_swap_instructions(&user_pubkey, &opp.quote_leg1)
        .await?;
    let swap_ixs2 = metis
        .get_swap_instructions(&user_pubkey, &opp.quote_leg2)
        .await?;

    // Get recent blockhash
    let recent_blockhash = rpc_client.get_latest_blockhash()?;

    // Build the versioned transaction (both swaps + Jito tip in one tx)
    let tx = transaction::build_arb_transaction(
        &swap_ixs1,
        &swap_ixs2,
        trading_keypair,
        opp.tip_lamports,
        recent_blockhash,
        rpc_client,
    )?;

    // Check transaction size (max 1232 bytes for Solana)
    let tx_bytes = bincode::serialize(&tx)?;
    if tx_bytes.len() > 1232 {
        anyhow::bail!(
            "tx too large: {} bytes (max 1232)",
            tx_bytes.len()
        );
    }

    debug!(
        tx_size = tx_bytes.len(),
        tip = opp.tip_lamports,
        "sending bundle"
    );

    // Send bundle to Jito — no waiting, fire and forget for speed
    let uuid = jito.send_bundle(&tx).await?;

    Ok(uuid)
}
