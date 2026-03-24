use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Client for the Metis (Jupiter self-hosted) routing engine.
pub struct MetisClient {
    base_url: String,
    http: Client,
}

// ---------- Quote types ----------

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub price_impact_pct: String,
    pub route_plan: serde_json::Value,
    #[serde(default)]
    pub context_slot: Option<u64>,
    /// Catch-all for extra fields returned by Metis.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------- Swap-instructions types ----------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsRequest {
    pub user_public_key: String,
    pub quote_response: serde_json::Value,
    pub wrap_and_unwrap_sol: bool,
    pub use_shared_accounts: bool,
    pub dynamic_compute_unit_limit: bool,
    pub skip_user_accounts_rpc_calls: bool,
    pub as_legacy_transaction: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    #[serde(default)]
    pub compute_budget_instructions: Vec<InstructionData>,
    #[serde(default)]
    pub setup_instructions: Vec<InstructionData>,
    pub swap_instruction: InstructionData,
    #[serde(default)]
    pub cleanup_instruction: Option<InstructionData>,
    #[serde(default)]
    pub address_lookup_table_addresses: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InstructionData {
    pub program_id: String,
    pub accounts: Vec<AccountMeta>,
    pub data: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl MetisClient {
    pub fn new(base_url: &str, timeout_ms: u64) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .pool_max_idle_per_host(10)
            .tcp_nodelay(true)
            .build()
            .expect("failed to build http client");
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// Get a quote from Metis.
    ///
    /// Parameters per Jupiter/Metis docs:
    /// - slippageBps=0: zero slippage, tx reverts if exact amount not met
    /// - onlyDirectRoutes=true: single-hop for speed
    /// - maxAccounts=24: keep tx within 1232 byte limit
    /// - forJitoBundle=true: excludes Jito-incompatible DEXes
    /// - swapMode=ExactIn: exact input amount
    /// - restrictIntermediateTokens=false: allow all intermediate tokens
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount_lamports: u64,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}\
             &slippageBps=0\
             &onlyDirectRoutes=true\
             &maxAccounts=24\
             &swapMode=ExactIn\
             &forJitoBundle=true\
             &restrictIntermediateTokens=false",
            self.base_url, input_mint, output_mint, amount_lamports
        );

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("quote request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("quote failed: {} — {}", status, body);
        }

        let quote: QuoteResponse = resp.json().await.context("failed to parse quote response")?;
        Ok(quote)
    }

    /// Get swap instructions for a given quote.
    ///
    /// Parameters per Jupiter docs:
    /// - wrapAndUnwrapSol=false: WSOL ATA must already exist
    /// - useSharedAccounts=true: use shared intermediate accounts
    /// - dynamicComputeUnitLimit=true: Metis calculates optimal CU
    /// - skipUserAccountsRpcCalls=true: skip RPC calls for speed
    /// - asLegacyTransaction=false: use VersionedTransaction v0 with ALT support
    pub async fn get_swap_instructions(
        &self,
        user_pubkey: &str,
        quote_response: &QuoteResponse,
    ) -> Result<SwapInstructionsResponse> {
        let quote_value = serde_json::to_value(quote_response)?;

        let body = SwapInstructionsRequest {
            user_public_key: user_pubkey.to_string(),
            quote_response: quote_value,
            wrap_and_unwrap_sol: false,
            use_shared_accounts: true,
            dynamic_compute_unit_limit: true,
            skip_user_accounts_rpc_calls: true,
            as_legacy_transaction: false,
        };

        let url = format!("{}/swap-instructions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("swap-instructions request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("swap-instructions failed: {} — {}", status, body);
        }

        let swap_ixs: SwapInstructionsResponse = resp
            .json()
            .await
            .context("failed to parse swap-instructions response")?;
        Ok(swap_ixs)
    }
}
