use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::Transaction;
use log::{info, warn, error, debug};

const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

#[derive(Serialize)]
struct SendBundleRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<serde_json::Value>,
}

#[derive(Serialize, Debug)]
struct BundleParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    frontrun_target: Option<String>,
}

#[derive(Deserialize, Debug)]
struct SendBundleResponse {
    result: String,
}

#[derive(Debug, Deserialize)]
struct BundleStatusResponse {
    jsonrpc: String,
    result: BundleStatusResult,
    id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleStatusResult {
    context: BundleContext,
    value: Vec<BundleStatus>,
}

#[derive(Debug, Deserialize)]
struct BundleContext {
    slot: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleStatus {
    pub bundle_id: String,
    pub transactions: Vec<String>,
    pub slot: u64,
    pub confirmation_status: String,
    pub err: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SimulateTransactionResponse {
    jsonrpc: String,
    result: SimulateTransactionResult,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct SimulateTransactionResult {
    context: SimulationContext,
    value: SimulationValue,
}

#[derive(Debug, Deserialize)]
struct SimulationContext {
    slot: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationValue {
    pub err: Option<serde_json::Value>,
    pub logs: Option<Vec<String>>,
    pub units_consumed: Option<u64>,
    pub accounts: Option<Vec<serde_json::Value>>,
}

pub struct JitoClient {
    http_client: Client,
    endpoints: Vec<String>,
    tip_account_index: std::sync::atomic::AtomicUsize,
    rpc_endpoint: String,
}

impl JitoClient {
    pub fn new(rpc_endpoint: String) -> Self {
        let http_client = Client::builder()
            .pool_max_idle_per_host(10)
            .tcp_keepalive(Some(std::time::Duration::from_secs(60)))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();

        let endpoints = vec![
            "https://frankfurt.mainnet.block-engine.jito.wtf".to_string(),
            "https://amsterdam.mainnet.block-engine.jito.wtf".to_string(),
        ];

        info!("🌐 Jito Client initialized");
        info!("   Jito Primary: Frankfurt 🇩🇪");
        info!("   Jito Fallback: Amsterdam 🇳🇱");
        info!("   RPC Endpoint: {}", rpc_endpoint);

        Self {
            http_client,
            endpoints,
            tip_account_index: std::sync::atomic::AtomicUsize::new(0),
            rpc_endpoint,
        }
    }

    pub fn get_tip_account(&self) -> &'static str {
        let index = self.tip_account_index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) % JITO_TIP_ACCOUNTS.len();
        JITO_TIP_ACCOUNTS[index]
    }

    /// ✅ شبیه‌سازی تراکنش با RPC - با replaceRecentBlockhash
    /// CRITICAL FIX: Added replaceRecentBlockhash and accounts parameters
    pub async fn simulate_transaction(&self, transaction: &Transaction) -> Result<SimulationValue> {
        debug!("🔬 Starting transaction simulation...");

        let serialized = bincode::serialize(transaction)
            .map_err(|e| anyhow!("Failed to serialize transaction: {}", e))?;
        let encoded = bs58::encode(&serialized).into_string();

        debug!("   Transaction size: {} bytes", serialized.len());
        debug!("   Base58 encoded length: {}", encoded.len());

        // ✅ اضافه شد: replaceRecentBlockhash برای جلوگیری از خطای blockhash قدیمی
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                encoded,
                {
                    "encoding": "base58",
                    "commitment": "processed",
                    "replaceRecentBlockhash": true,  // ✅ CRITICAL FIX!
                    "sigVerify": false,  // ✅ Skip signature verification for speed
                }
            ]
        });

        debug!("   📤 Sending simulation request to RPC...");

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                error!("❌ RPC simulation network error: {}", e);
                anyhow!("RPC simulation error: {}", e)
            })?;

        let status = response.status();
        debug!("   📥 Response status: {}", status);

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            error!("❌ RPC HTTP error {}: {}", status, error_text);
            return Err(anyhow!("RPC HTTP error: {}", error_text));
        }

        let response_text = response.text().await.unwrap_or_default();
        debug!("   📋 Response body length: {} bytes", response_text.len());

        let sim_response: SimulateTransactionResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                error!("❌ Failed to parse simulation response: {}", e);
                error!("   Response: {}", response_text);
                anyhow!("Failed to parse simulation response: {}", e)
            })?;

        debug!("   ✅ Simulation completed at slot: {}", sim_response.result.context.slot);

        Ok(sim_response.result.value)
    }

    /// ✅ چک کردن وضعیت تراکنش هدف - آیا قبلاً اجرا شده؟
    /// CRITICAL: Check if victim transaction already executed
    pub async fn check_target_transaction_status(&self, signature: &str) -> Result<TargetTxStatus> {
        debug!("🔍 Checking target transaction status: {}", signature);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignatureStatuses",
            "params": [
                [signature],
                {
                    "searchTransactionHistory": true
                }
            ]
        });

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to check tx status: {}", e))?;

        if !response.status().is_success() {
            warn!("   ⚠️  HTTP error checking tx status: {}", response.status());
            return Ok(TargetTxStatus::Unknown);
        }

        let response_json: serde_json::Value = response.json().await?;

        if let Some(result) = response_json.get("result") {
            if let Some(value) = result.get("value") {
                if let Some(arr) = value.as_array() {
                    if let Some(status) = arr.get(0) {
                        if !status.is_null() {
                            // تراکنش پیدا شد - آیا confirmed است؟
                            if let Some(confirmation_status) = status.get("confirmationStatus") {
                                let status_str = confirmation_status.as_str().unwrap_or("");
                                match status_str {
                                    "confirmed" | "finalized" => {
                                        info!("   ⚠️  Target tx ALREADY CONFIRMED: {}", signature);
                                        return Ok(TargetTxStatus::AlreadyConfirmed);
                                    }
                                    "processed" => {
                                        info!("   ✅ Target tx in mempool (processed)");
                                        return Ok(TargetTxStatus::InMempool);
                                    }
                                    _ => {
                                        debug!("   ℹ️  Target tx status: {}", status_str);
                                        return Ok(TargetTxStatus::InMempool);
                                    }
                                }
                            }
                            return Ok(TargetTxStatus::InMempool);
                        }
                    }
                }
            }
        }

        debug!("   ℹ️  Target tx not found on-chain yet");
        Ok(TargetTxStatus::NotFound)
    }

    /// ارسال bundle به Jito
    pub async fn send_bundle_with_victim(
        &self,
        transactions: Vec<Transaction>,
        victim_signature: Option<String>,
    ) -> Result<String> {
        info!("📤 Sending bundle with {} transactions", transactions.len());
        if let Some(sig) = &victim_signature {
            info!("   Victim target: {}", sig);
        }

        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .expect("Failed to serialize transaction");
                bs58::encode(&serialized).into_string()
            })
            .collect();

        debug!("📦 Encoded transactions:");
        for (i, tx) in encoded_txs.iter().enumerate() {
            debug!("   Tx {}: {} bytes", i, tx.len());
        }

        let mut params = vec![serde_json::to_value(&encoded_txs)?];

        if let Some(victim_sig) = victim_signature.clone() {
            let bundle_params = BundleParams {
                frontrun_target: Some(victim_sig.clone()),
            };
            params.push(serde_json::to_value(&bundle_params)?);
            debug!("   Using frontrun_target: {}", victim_sig);
        }

        let request = SendBundleRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params,
        };

        debug!("📤 Full request to Jito:");
        debug!("   Method: {}", request.method);
        debug!("   Params count: {}", request.params.len());

        // تلاش با Frankfurt
        match self.send_to_endpoint(&self.endpoints[0], &request).await {
            Ok(bundle_id) => {
                info!("✅ Bundle sent to Frankfurt 🇩🇪: {}", bundle_id);
                return Ok(bundle_id);
            }
            Err(e) => {
                warn!("⚠️  Frankfurt failed: {}", e);
            }
        }

        // Fallback به Amsterdam
        match self.send_to_endpoint(&self.endpoints[1], &request).await {
            Ok(bundle_id) => {
                info!("✅ Bundle sent to Amsterdam 🇳🇱: {}", bundle_id);
                Ok(bundle_id)
            }
            Err(e) => {
                error!("❌ Both endpoints failed!");
                Err(anyhow!("Failed to send bundle: {}", e))
            }
        }
    }

    /// چک کردن وضعیت bundle
    pub async fn check_bundle_status(&self, bundle_ids: Vec<String>) -> Result<Vec<BundleStatus>> {
        debug!("🔍 Checking bundle status for {} bundles", bundle_ids.len());
        for id in &bundle_ids {
            debug!("   Bundle ID: {}", id);
        }

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [bundle_ids]
        });

        debug!("📤 Bundle status request: {:?}", request);

        // سعی با هر دو endpoint
        for endpoint in &self.endpoints {
            let url = format!("{}/api/v1/bundles", endpoint);
            debug!("   Trying endpoint: {}", url);

            match self.http_client
                .post(&url)
                .json(&request)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    debug!("   Response status: {}", status);

                    if status.is_success() {
                        let response_text = response.text().await.unwrap_or_default();
                        debug!("   Response body: {}", response_text);

                        match serde_json::from_str::<BundleStatusResponse>(&response_text) {
                            Ok(result) => {
                                info!("✅ Got bundle status from {}", endpoint);
                                return Ok(result.result.value);
                            }
                            Err(e) => {
                                warn!("   Failed to parse response: {}", e);
                                continue;
                            }
                        }
                    } else {
                        let error_text = response.text().await.unwrap_or_default();
                        warn!("   HTTP error {}: {}", status, error_text);
                    }
                }
                Err(e) => {
                    warn!("   Request failed: {}", e);
                    continue;
                }
            }
        }

        Err(anyhow!("Failed to get bundle status from any endpoint"))
    }

    /// چک کردن اینکه آیا تراکنش روی blockchain landed شده یا نه
    pub async fn check_transaction_status(&self, signature: &str) -> Result<bool> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignatureStatuses",
            "params": [
                [signature],
                {
                    "searchTransactionHistory": true
                }
            ]
        });

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to check tx status: {}", e))?;

        if !response.status().is_success() {
            return Ok(false);
        }

        let response_json: serde_json::Value = response.json().await?;

        if let Some(result) = response_json.get("result") {
            if let Some(value) = result.get("value") {
                if let Some(arr) = value.as_array() {
                    if let Some(status) = arr.get(0) {
                        if !status.is_null() {
                            debug!("✅ Transaction {} found on-chain!", signature);
                            return Ok(true);
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    /// لاگ کردن جزئیات bundle برای debugging
    pub fn log_bundle_details(
        &self,
        bundle_id: &str,
        victim_sig: &str,
        front_tx_sig: &str,
        back_tx_sig: &str,
    ) {
        info!("📦 Bundle Details:");
        info!("   Bundle ID: {}", bundle_id);
        info!("   Victim Tx: {}", victim_sig);
        info!("   Front-run Tx: {}", front_tx_sig);
        info!("   Back-run Tx: {}", back_tx_sig);
        info!("   🔗 Jito Explorer: https://explorer.jito.wtf/bundle/{}", bundle_id);
        info!("   🔗 Front-run Explorer: https://solscan.io/tx/{}", front_tx_sig);
        info!("   🔗 Back-run Explorer: https://solscan.io/tx/{}", back_tx_sig);
    }

    async fn send_to_endpoint(
        &self,
        endpoint: &str,
        request: &SendBundleRequest,
    ) -> Result<String> {
        let url = format!("{}/api/v1/bundles", endpoint);
        debug!("📤 Sending to: {}", url);

        let response = self.http_client
            .post(&url)
            .json(request)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("HTTP error: {}", e))?;

        let status = response.status();
        debug!("   Response status: {}", status);

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            error!("   Response body: {}", error_text);
            return Err(anyhow!("Jito error {}: {}", status, error_text));
        }

        let response_text = response.text().await.unwrap_or_default();
        debug!("   Response body: {}", response_text);

        let result: SendBundleResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Parse error: {}", e))?;

        Ok(result.result)
    }
}

/// ✅ وضعیت تراکنش هدف
#[derive(Debug, PartialEq)]
pub enum TargetTxStatus {
    NotFound,           // هنوز در شبکه نیست
    InMempool,          // در mempool است
    AlreadyConfirmed,   // قبلاً confirmed شده (دیر رسیدیم!)
    Unknown,            // نامشخص
}
