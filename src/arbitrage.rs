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
struct Opportunity {
    token_mint: String,
    tip_lamports: u64,
    quote_leg1: QuoteResponse,
    quote_leg2: QuoteResponse,
}

/// Scan ALL tokens at each amount step before moving to the next step.
///
/// Loop order:
///   for amount in min..max step step:
///     for token in tokens:
///       quote WSOL→Token, Token→WSOL
///       if profitable → execute immediately
///
/// This ensures all tokens get tested at the same amount level,
/// rather than exhausting all amounts for one token before moving on.
pub async fn scan_all_tokens(
    metis: &MetisClient,
    token_mints: &[String],
    config: &Config,
    jito: &JitoClient,
    trading_keypair: &Keypair,
    rpc_client: &RpcClient,
    jito_limiter: &mut RateLimiter,
) -> Result<()> {
    let min_lamports = (config.trading.min_amount_sol * LAMPORTS_PER_SOL) as u64;
    let max_lamports = (config.trading.max_amount_sol * LAMPORTS_PER_SOL) as u64;
    let step_lamports = (config.trading.step_sol * LAMPORTS_PER_SOL) as u64;

    let mut amount = min_lamports;
    while amount <= max_lamports {
        // Test ALL tokens at this amount
        for token_mint in token_mints {
            // Leg 1: WSOL → Token
            let quote1 = match metis.get_quote(WSOL_MINT, token_mint, amount).await {
                Ok(q) => q,
                Err(_) => continue,
            };

            let token_amount: u64 = match quote1.out_amount.parse() {
                Ok(v) if v > 0 => v,
                _ => continue,
            };

            // Leg 2: Token → WSOL
            let quote2 = match metis.get_quote(token_mint, WSOL_MINT, token_amount).await {
                Ok(q) => q,
                Err(_) => continue,
            };

            let output_wsol: u64 = quote2.out_amount.parse().unwrap_or(0);

            // Check profitability
            if output_wsol <= amount {
                continue;
            }

            let raw_profit = output_wsol - amount;
            let tip = transaction::calculate_tip(
                raw_profit,
                config.jito.tip_profit_percent,
                config.jito.tip_min_lamports,
                config.jito.tip_max_lamports,
            );

            if raw_profit <= tip + config.trading.min_profit_lamports {
                continue;
            }

            let net_profit = raw_profit - tip;

            // Check Jito rate limit — drop if exceeded (no queue)
            if !jito_limiter.try_acquire() {
                debug!(token = token_mint.as_str(), "jito rate limit hit, dropping");
                continue;
            }

            info!(
                token = token_mint.as_str(),
                input_sol = amount as f64 / LAMPORTS_PER_SOL,
                output_sol = output_wsol as f64 / LAMPORTS_PER_SOL,
                profit_lamports = net_profit,
                tip_lamports = tip,
                "PROFITABLE — executing"
            );

            let opp = Opportunity {
                token_mint: token_mint.clone(),
                tip_lamports: tip,
                quote_leg1: quote1,
                quote_leg2: quote2,
            };

            match execute_opportunity(&opp, metis, jito, trading_keypair, rpc_client).await {
                Ok(uuid) => {
                    info!(
                        uuid = %uuid,
                        token = opp.token_mint.as_str(),
                        profit = net_profit,
                        "bundle sent to Jito"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        chain = ?e.chain().skip(1).map(|c| c.to_string()).collect::<Vec<_>>(),
                        token = opp.token_mint.as_str(),
                        input_lamports = amount,
                        "execution failed"
                    );
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
    jito: &JitoClient,
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
        anyhow::bail!("tx too large: {} bytes", tx_bytes.len());
    }

    // Send bundle to Jito
    let uuid = jito.send_bundle(&tx).await?;

    Ok(uuid)
}
