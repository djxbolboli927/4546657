/// Jupiter Metis local API client.
///
/// Calls the self-hosted Jupiter Metis binary at 127.0.0.1:8080.
/// Metis must be running with ALLOW_CIRCULAR_ARBITRAGE=true.
///
/// Flow:
///   1. GET /quote → receive best route + expected output
///   2. POST /swap-instructions → receive raw Solana instructions
///   3. Caller assembles transaction from those instructions + Lighthouse assertion
///
/// Slippage is always set to 0 for atomic arbitrage: either the exact output is
/// achieved or the transaction reverts with zero cost.

use anyhow::{anyhow, Result};
use log::{debug, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const METIS_BASE_URL: &str = "http://127.0.0.1:8080";

/// Mints
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// ─── API response types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u32,
    pub route_plan: Vec<RoutePlanStep>,
    pub context_slot: Option<u64>,
    pub time_taken: Option<f64>,
}

impl QuoteResponse {
    pub fn in_amount_u64(&self) -> u64 {
        self.in_amount.parse().unwrap_or(0)
    }
    pub fn out_amount_u64(&self) -> u64 {
        self.out_amount.parse().unwrap_or(0)
    }
    pub fn profit_lamports(&self) -> i64 {
        self.out_amount_u64() as i64 - self.in_amount_u64() as i64
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: Option<String>,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub fee_mint: String,
    pub fee_amount: String,
}

// ─── Swap instructions response ─────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsRequest {
    pub quote_response: QuoteResponse,
    pub user_public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap_and_unwrap_sol: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prioritization_fee_lamports: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    pub token_ledger_instruction: Option<SerializedInstruction>,
    pub compute_budget_instructions: Vec<SerializedInstruction>,
    pub setup_instructions: Vec<SerializedInstruction>,
    pub swap_instruction: SerializedInstruction,
    pub cleanup_instruction: Option<SerializedInstruction>,
    pub address_lookup_table_addresses: Vec<String>,
    pub prioritization_fee_lamports: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SerializedInstruction {
    pub program_id: String,
    pub accounts: Vec<AccountMeta>,
    pub data: String, // base64-encoded
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

// ─── Client ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct JupiterClient {
    http: Client,
    base_url: String,
}

impl JupiterClient {
    pub fn new(base_url: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(500)) // tight deadline — local Metis
            .build()
            .expect("Failed to build reqwest client for Jupiter Metis");
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// GET /quote for circular WSOL→...→WSOL arbitrage.
    ///
    /// `amount_in_lamports`: how many lamports of WSOL to input.
    /// Returns the full quote response including route plan and expected output.
    pub async fn quote_circular_wsol(
        &self,
        amount_in_lamports: u64,
    ) -> Result<QuoteResponse> {
        self.quote(WSOL_MINT, WSOL_MINT, amount_in_lamports).await
    }

    /// GET /quote for one-leg WSOL → USDC.
    pub async fn quote_wsol_to_usdc(&self, amount_in_lamports: u64) -> Result<QuoteResponse> {
        self.quote(WSOL_MINT, USDC_MINT, amount_in_lamports).await
    }

    /// GET /quote for one-leg USDC → WSOL.
    pub async fn quote_usdc_to_wsol(&self, amount_in_usdc: u64) -> Result<QuoteResponse> {
        self.quote(USDC_MINT, WSOL_MINT, amount_in_usdc).await
    }

    /// Generic GET /quote.
    ///
    /// All filters are sent as query parameters per Metis API spec:
    /// - `slippageBps=0`: Zero slippage for atomic arbitrage (revert if price changed).
    /// - `swapMode=ExactIn`: Fix the input amount, let Metis find best output.
    /// - `forJitoBundle=true`: Exclude DEXes incompatible with Jito bundles (HumidiFi).
    /// - `restrictIntermediateTokens=true`: Only route through high-liquidity intermediate
    ///    tokens (stables), avoiding obscure pairs that cause failures.
    /// - `maxAccounts=40`: Limit transaction account count. Fewer accounts → fewer hops
    ///    (Metis has no native `maxHops` param; this is the indirect control).
    ///    40 accounts typically yields 2-4 hops while keeping TX size manageable.
    pub async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}\
             &slippageBps=0\
             &swapMode=ExactIn\
             &forJitoBundle=true\
             &restrictIntermediateTokens=true\
             &maxAccounts=40",
            self.base_url, input_mint, output_mint, amount
        );

        debug!("Jupiter quote: {url}");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Jupiter /quote request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Jupiter /quote HTTP {status}: {body}"));
        }

        let quote: QuoteResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("Jupiter /quote JSON parse error: {e}"))?;

        debug!(
            "Jupiter quote: {} → {} (in={} out={})",
            quote.input_mint, quote.output_mint, quote.in_amount, quote.out_amount
        );

        Ok(quote)
    }

    /// POST /swap-instructions to get raw Solana transaction instructions.
    ///
    /// Returns [SwapInstructionsResponse] which the caller uses to build TX1.
    /// Do not set computeUnitPrice here — we add our own compute budget instructions.
    pub async fn swap_instructions(
        &self,
        quote: QuoteResponse,
        user_public_key: &str,
    ) -> Result<SwapInstructionsResponse> {
        let url = format!("{}/swap-instructions", self.base_url);

        let body = SwapInstructionsRequest {
            quote_response: quote,
            user_public_key: user_public_key.to_string(),
            wrap_and_unwrap_sol: Some(true), // auto-wrap WSOL
            compute_unit_price_micro_lamports: None, // we set our own
            prioritization_fee_lamports: None,
        };

        debug!("Jupiter /swap-instructions for {user_public_key}");

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("Jupiter /swap-instructions request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Jupiter /swap-instructions HTTP {status}: {body}"));
        }

        let swap_ixs: SwapInstructionsResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("Jupiter /swap-instructions JSON parse error: {e}"))?;

        debug!(
            "Jupiter swap-instructions: {} ALTs, {} setup ixs, 1 swap ix",
            swap_ixs.address_lookup_table_addresses.len(),
            swap_ixs.setup_instructions.len()
        );

        Ok(swap_ixs)
    }

    /// Resolve DEX label from program ID using Jupiter's registry.
    pub async fn program_id_to_label(&self, program_id: &str) -> Result<String> {
        let url = format!("{}/program-id-to-label", self.base_url);
        let body = serde_json::json!({ "id": program_id });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("Jupiter /program-id-to-label failed: {e}"))?;

        if !resp.status().is_success() {
            return Ok(program_id.to_string()); // fallback to raw ID
        }

        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        Ok(json
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or(program_id)
            .to_string())
    }

    /// Health check — verify Metis is reachable.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url);
        match self.http.get(&url).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }
}

// ─── Instruction deserialization helpers ────────────────────────────────────

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use solana_sdk::{
    instruction::{AccountMeta as SolanaAccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

/// Convert a Jupiter [SerializedInstruction] to a Solana [Instruction].
pub fn deserialize_instruction(ix: &SerializedInstruction) -> Result<Instruction> {
    let program_id = Pubkey::from_str(&ix.program_id)
        .map_err(|e| anyhow!("Invalid program_id '{}': {e}", ix.program_id))?;

    let accounts: Vec<SolanaAccountMeta> = ix
        .accounts
        .iter()
        .map(|a| {
            let pubkey = Pubkey::from_str(&a.pubkey)
                .map_err(|e| anyhow!("Invalid account pubkey '{}': {e}", a.pubkey))?;
            Ok(SolanaAccountMeta {
                pubkey,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
        })
        .collect::<Result<_>>()?;

    let data = BASE64
        .decode(&ix.data)
        .map_err(|e| anyhow!("Failed to decode instruction data: {e}"))?;

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Convert all instructions in a [SwapInstructionsResponse] to Solana [Instruction]s.
/// Order: setup ixs → swap ix → cleanup ix (if present).
pub fn collect_swap_instructions(resp: &SwapInstructionsResponse) -> Result<Vec<Instruction>> {
    let mut ixs = Vec::new();
    for ix in &resp.setup_instructions {
        ixs.push(deserialize_instruction(ix)?);
    }
    ixs.push(deserialize_instruction(&resp.swap_instruction)?);
    if let Some(cleanup) = &resp.cleanup_instruction {
        ixs.push(deserialize_instruction(cleanup)?);
    }
    Ok(ixs)
}

/// Decode address lookup table addresses from Jupiter response.
pub fn decode_alt_addresses(resp: &SwapInstructionsResponse) -> Vec<Pubkey> {
    resp.address_lookup_table_addresses
        .iter()
        .filter_map(|s| Pubkey::from_str(s).ok())
        .collect()
}
