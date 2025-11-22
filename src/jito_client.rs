use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::Transaction;
use log::{info, warn, error};

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

#[derive(Serialize)]
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
        info!("   RPC Simulation: {}", rpc_endpoint);

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

    /// شبیه‌سازی تراکنش با RPC
    pub async fn simulate_transaction(&self, transaction: &Transaction) -> Result<SimulationValue> {
        let serialized = bincode::serialize(transaction)
            .map_err(|e| anyhow!("Failed to serialize transaction: {}", e))?;
        let encoded = bs58::encode(&serialized).into_string();

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                encoded,
                {
                    "encoding": "base58",
                    "commitment": "processed"
                }
            ]
        });

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("RPC simulation error: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("RPC HTTP error: {}", error_text));
        }

        let sim_response: SimulateTransactionResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse simulation response: {}", e))?;

        Ok(sim_response.result.value)
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

        let mut params = vec![serde_json::to_value(&encoded_txs)?];

        if let Some(victim_sig) = victim_signature {
            let bundle_params = BundleParams {
                frontrun_target: Some(victim_sig),
            };
            params.push(serde_json::to_value(&bundle_params)?);
        }

        let request = SendBundleRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params,
        };

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
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [bundle_ids]
        });

        // سعی با Frankfurt
        for endpoint in &self.endpoints {
            let url = format!("{}/api/v1/bundles", endpoint);

            match self.http_client
                .post(&url)
                .json(&request)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        if let Ok(result) = response.json::<BundleStatusResponse>().await {
                            return Ok(result.result.value);
                        }
                    }
                }
                Err(_) => continue,
            }
        }

        Err(anyhow!("Failed to get bundle status from any endpoint"))
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
        info!("   Check status: https://explorer.jito.wtf/bundle/{}", bundle_id);
    }

    async fn send_to_endpoint(
        &self,
        endpoint: &str,
        request: &SendBundleRequest,
    ) -> Result<String> {
        let url = format!("{}/api/v1/bundles", endpoint);

        let response = self.http_client
            .post(&url)
            .json(request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| anyhow!("HTTP error: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito error: {}", error_text));
        }

        let result: SendBundleResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Parse error: {}", e))?;

        Ok(result.result)
    }
}
