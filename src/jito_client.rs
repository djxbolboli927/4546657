use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::Transaction;
use log::{info, warn, error, debug};
use std::time::Duration;
use tokio::time::sleep;

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

// ✅ ساختار پاسخ منعطف‌تر (برای جلوگیری از خطای missing field)
#[derive(Debug, Deserialize)]
struct SimulateBundleResponse {
    jsonrpc: String,
    #[serde(default)] // اگر نبود، None بگذار
    result: Option<SimulateBundleResult>,
    #[serde(default)] // اگر ارور بود، آن را بگیر
    error: Option<serde_json::Value>,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct SimulateBundleResult {
    context: SimulationContext,
    value: SimulateBundleValue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateBundleValue {
    pub summary: serde_json::Value,
    pub transaction_results: Vec<SimulationValue>,
}

#[derive(Debug, Deserialize)]
struct SimulateTransactionResponse {
    jsonrpc: String,
    #[serde(default)]
    result: Option<SimulateTransactionResult>, // Option شد
    #[serde(default)]
    error: Option<serde_json::Value>, // اضافه شد
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

#[derive(Debug, Deserialize, Clone)]
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
            "https://ny.mainnet.block-engine.jito.wtf".to_string(),
            "https://tokyo.mainnet.block-engine.jito.wtf".to_string(),
        ];

        info!("🌐 Jito Client initialized");
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

    /// ✅ شبیه‌سازی باندل با ارسال به Jito Block Engine (نه RPC معمولی)
    pub async fn simulate_bundle(&self, transactions: Vec<Transaction>) -> Result<SimulateBundleValue> {
        // 1. سریالایز کردن تراکنش‌ها به Base58
        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .expect("Failed to serialize transaction");
                bs58::encode(&serialized).into_string()
            })
            .collect();

        // 2. فرمت صحیح Jito (طبق مستندات رسمی)
        let params_vec = vec![
            serde_json::json!({
                "encodedTransactions": encoded_txs,
                "bundleUuid": null,
                "decoding": "base58"
            })
        ];

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateBundle",
            "params": params_vec
        });

        // 3. ✅ ارسال به Jito Block Engine (Frankfurt)
        let simulation_url = format!("{}/api/v1/bundles", self.endpoints[0]);

        let response = self.http_client
            .post(&simulation_url)
            .json(&request)  // این خودش Content-Type را اضافه می‌کند
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("Bundle simulation network error: {}", e))?;

        // 4. ذخیره status قبل از text() (رفع خطای ownership)
        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito HTTP error ({}): {}", status, error_text));
        }

        let response_text = response.text().await.unwrap_or_default();

        // 5. پارس کردن پاسخ
        let sim_response: SimulateBundleResponse = match serde_json::from_str(&response_text) {
            Ok(v) => v,
            Err(e) => {
                error!("❌ Failed to parse Jito response!");
                debug!("   Raw Response: {}", response_text);
                return Err(anyhow!("Parse error: {} | Response: {}", e, response_text));
            }
        };

        if let Some(err) = sim_response.error {
            return Err(anyhow!("Jito API Error: {:?}", err));
        }

        if let Some(result) = sim_response.result {
            Ok(result.value)
        } else {
            Err(anyhow!("Empty result from Jito simulation"))
        }
    }

    /// شبیه‌سازی تکی (با اصلاح هندلینگ خطا)
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
                    "commitment": "processed",
                    "replaceRecentBlockhash": true,
                    "sigVerify": false,
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
            return Err(anyhow!("RPC HTTP error"));
        }

        let response_text = response.text().await.unwrap_or_default();

        let sim_response: SimulateTransactionResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                // error!("Failed to parse RPC response: {}", response_text);
                anyhow!("Failed to parse simulation response: {}", e)
            })?;

        if let Some(err) = sim_response.error {
            return Err(anyhow!("RPC API Error: {:?}", err));
        }

        if let Some(result) = sim_response.result {
            Ok(result.value)
        } else {
            Err(anyhow!("Empty result from RPC simulation"))
        }
    }

    // متدهای دیگر بدون تغییر
    pub async fn check_target_transaction_status(&self, signature: &str) -> Result<TargetTxStatus> {
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
            return Ok(TargetTxStatus::Unknown);
        }

        let response_json: serde_json::Value = response.json().await?;

        if let Some(result) = response_json.get("result") {
            if let Some(value) = result.get("value") {
                if let Some(arr) = value.as_array() {
                    if let Some(status) = arr.get(0) {
                        if !status.is_null() {
                            if let Some(confirmation_status) = status.get("confirmationStatus") {
                                let status_str = confirmation_status.as_str().unwrap_or("");
                                match status_str {
                                    "confirmed" | "finalized" => return Ok(TargetTxStatus::AlreadyConfirmed),
                                    "processed" => return Ok(TargetTxStatus::InMempool),
                                    _ => return Ok(TargetTxStatus::InMempool),
                                }
                            }
                            return Ok(TargetTxStatus::InMempool);
                        }
                    }
                }
            }
        }
        Ok(TargetTxStatus::NotFound)
    }

    pub fn get_tip_account_str(&self) -> String {
        self.get_tip_account().to_string()
    }

    pub async fn send_bundle_with_victim(
        &self,
        transactions: Vec<Transaction>,
        _victim_signature: Option<String>,
    ) -> Result<String> {
        info!("📤 Sending bundle to Jito Block Engine...");
        info!("   Bundle size: {} transactions", transactions.len());

        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx).unwrap();
                bs58::encode(&serialized).into_string()
            })
            .collect();

        let params_vec = vec![encoded_txs];

        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": params_vec
        });

        let url = format!("{}/api/v1/bundles", self.endpoints[0]);
        debug!("   Endpoint: {}", url);

        let response = self.http_client
            .post(&url)
            .json(&request_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                error!("❌ Jito HTTP request failed: {}", e);
                anyhow!("HTTP error: {}", e)
            })?;

        let status_code = response.status();

        if !status_code.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            error!("❌ Jito rejected bundle!");
            error!("   HTTP Status: {}", status_code);
            error!("   Response: {}", error_text);

            // پارس کردن خطاهای Jito
            if status_code == 429 {
                return Err(anyhow!("Jito Rate Limit (429): Too many requests"));
            } else if status_code == 400 {
                return Err(anyhow!("Jito Bad Request (400): {}", error_text));
            }

            return Err(anyhow!("Jito error {}: {}", status_code, error_text));
        }

        let response_text = response.text().await.unwrap_or_default();
        debug!("   Response body: {}", response_text);

        let result: SendBundleResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                error!("❌ Failed to parse Jito response: {}", e);
                error!("   Raw response: {}", response_text);
                anyhow!("Parse error: {}", e)
            })?;

        let bundle_id = result.result;
        info!("✅ Bundle accepted by Jito!");
        info!("   Bundle ID: {}", bundle_id);

        Ok(bundle_id)
    }

    /// ✅ NEW: پیگیری real-time bundle status (برای bundle های در حال پردازش)
    pub async fn get_inflight_bundle_statuses(&self, bundle_ids: Vec<String>) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getInflightBundleStatuses",
            "params": [bundle_ids]
        });

        let url = format!("{}/api/v1/bundles", self.endpoints[0]);

        let response = self.http_client
            .post(&url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to get inflight status: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito error: {}", error_text));
        }

        let response_json: serde_json::Value = response.json().await?;
        Ok(response_json)
    }

    /// ✅ NEW: پیگیری bundle status (برای bundle های landed)
    pub async fn get_bundle_statuses(&self, bundle_ids: Vec<String>) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [bundle_ids]
        });

        let url = format!("{}/api/v1/bundles", self.endpoints[0]);

        let response = self.http_client
            .post(&url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| anyhow!("Failed to get bundle status: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito error: {}", error_text));
        }

        let response_json: serde_json::Value = response.json().await?;
        Ok(response_json)
    }
}

#[derive(Debug, PartialEq)]
pub enum TargetTxStatus {
    NotFound,
    InMempool,
    AlreadyConfirmed,
    Unknown,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TokenProgramType {
    TokenProgram,
    Token2022Program,
}
