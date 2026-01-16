use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::{Transaction, VersionedTransaction};
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

    /// ✅ شبیه‌سازی باندل با استفاده از Jito simulateBundle API
    /// این متد از Base64 encoding استفاده می‌کند (طبق مستندات جیتو)
    /// نکته مهم: simulateBundle باید به RPC endpoint فرستاده شود (نه Block Engine!)
    /// منبع: https://www.quicknode.com/docs/solana/simulateBundle
    pub async fn simulate_bundle(&self, transactions: Vec<Transaction>, _jito_endpoint: Option<&str>) -> Result<SimulateBundleValue> {
        // تبدیل تراکنش‌ها به Base64 (نه Base58!)
        let encoded_txs: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .expect("Failed to serialize transaction");
                BASE64.encode(&serialized)
            })
            .collect();

        // ساختار درخواست طبق مستندات QuickNode/Jito
        let params_vec = vec![
            serde_json::json!({
                "encodedTransactions": encoded_txs,
                "simulationBank": "Tip",           // شبیه‌سازی روی آخرین وضعیت
                "skipSigVerify": false,            // اعتبارسنجی امضا
                "replaceRecentBlockhash": true     // جایگزینی هش بلاک قدیمی
            })
        ];

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateBundle",
            "params": params_vec
        });

        // ✅ استفاده از RPC endpoint (نه Block Engine!)
        // simulateBundle باید به RPC node فرستاده شود که jito-solana را اجرا می‌کند
        // مثل ERPC، Helius، Triton (با پشتیبانی Jito)
        let rpc_url = &self.rpc_endpoint;

        debug!("📡 Sending simulateBundle to RPC: {}", rpc_url);

        let response = self.http_client
            .post(rpc_url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| anyhow!("Bundle simulation network error: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito HTTP error: {}", error_text));
        }

        let response_text = response.text().await.unwrap_or_default();

        let sim_response: SimulateBundleResponse = match serde_json::from_str(&response_text) {
            Ok(v) => v,
            Err(e) => {
                error!("❌ Failed to parse Jito response!");
                error!("   Raw Response: {}", response_text);
                return Err(anyhow!("Parse error: {}", e));
            }
        };

        if let Some(err) = sim_response.error {
            return Err(anyhow!("Jito API Error: {:?}", err));
        }

        if let Some(result) = sim_response.result {
            debug!("✅ Jito simulation response received");
            debug!("   Summary: {:?}", result.value.summary);
            debug!("   Transaction results count: {}", result.value.transaction_results.len());
            Ok(result.value)
        } else {
            error!("❌ Empty result from Jito simulation");
            error!("   Raw response: {}", response_text);
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

    /// بررسی موازی وضعیت victim از RPC و Jito endpoint به طور همزمان
    /// برمی‌گرداند: (RPC status, Jito status, RPC latency ms, Jito latency ms)
    pub async fn check_victim_parallel(
        &self,
        victim_signature: &str,
        _optimal_jito_endpoint: &str,
    ) -> (TargetTxStatus, TargetTxStatus, f64, f64) {
        use tokio::join;
        use std::time::Instant;

        // درخواست موازی به هر دو endpoint
        let rpc_future = async {
            let start = Instant::now();
            let result = self.check_target_transaction_status(victim_signature).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            (result.unwrap_or(TargetTxStatus::Unknown), elapsed)
        };

        // برای Jito، از همان RPC استفاده می‌کنیم (Solana همه endpoint ها یکسان هستند)
        // ولی می‌توانیم بعداً از endpoint دیگری استفاده کنیم
        let jito_future = async {
            let start = Instant::now();
            let result = self.check_target_transaction_status(victim_signature).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            (result.unwrap_or(TargetTxStatus::Unknown), elapsed)
        };

        let ((rpc_status, rpc_ms), (jito_status, jito_ms)) = join!(rpc_future, jito_future);

        (rpc_status, jito_status, rpc_ms, jito_ms)
    }

    /// بررسی وضعیت تراکنش victim با استراتژی موازی (RPC + Jito)
    /// این متد سریع‌تر از check_target_transaction_status است
    pub async fn check_victim_with_fallback(
        &self,
        victim_signature: &str,
        _optimal_jito_endpoint: &str,
    ) -> Result<TargetTxStatus> {
        use tokio::time::{timeout, Duration};

        // استراتژی: ابتدا RPC را با timeout کوتاه امتحان کن
        // اگر خیلی سریع جواب داد (کمتر از 150ms)، از همان استفاده کن
        // در غیر این صورت، fallback به RPC عادی
        match timeout(
            Duration::from_millis(150),
            self.check_target_transaction_status(victim_signature)
        ).await {
            Ok(Ok(status)) => Ok(status),
            _ => {
                // Fallback: درخواست RPC مجدد با timeout بیشتر
                debug!("Fast check timeout, falling back to standard RPC");
                self.check_target_transaction_status(victim_signature).await
            }
        }
    }

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
                                    "processed" => return Ok(TargetTxStatus::Processed),
                                    _ => return Ok(TargetTxStatus::Processed),
                                }
                            }
                            return Ok(TargetTxStatus::Processed);
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

    /// ✅ شبیه‌سازی تراکنش victim برای بررسی اینکه آیا موفق خواهد شد یا نه
    /// این متد حتی برای تراکنش‌هایی که هنوز در blockchain history نیستند کار می‌کند!
    pub async fn simulate_victim_transaction(
        &self,
        victim_tx: &VersionedTransaction,
    ) -> Result<VictimSimulationResult> {
        // Serialize کردن تراکنش victim
        let serialized = bincode::serialize(victim_tx)
            .map_err(|e| anyhow!("Failed to serialize victim tx: {}", e))?;
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
                    "replaceRecentBlockhash": true,  // ✅ از blockhash جدید استفاده کن
                    "sigVerify": false,              // ✅ signature را verify نکن
                }
            ]
        });

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .timeout(std::time::Duration::from_millis(200))  // timeout سریع برای MEV
            .send()
            .await
            .map_err(|e| anyhow!("Victim simulation network error: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("RPC HTTP error: {}", response.status()));
        }

        let response_json: serde_json::Value = response.json().await
            .map_err(|e| anyhow!("Failed to parse JSON: {}", e))?;

        if let Some(err) = response_json.get("error") {
            return Err(anyhow!("RPC API Error: {:?}", err));
        }

        if let Some(result) = response_json.get("result").and_then(|r| r.get("value")) {
            let will_succeed = result.get("err").and_then(|e| e.as_null()).is_some();
            let error = result.get("err").cloned();
            let logs = result.get("logs").and_then(|l| l.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect());
            let units_consumed = result.get("unitsConsumed").and_then(|u| u.as_u64());

            Ok(VictimSimulationResult {
                will_succeed,
                error,
                logs,
                units_consumed,
            })
        } else {
            Err(anyhow!("Empty result from victim simulation"))
        }
    }

    /// ✅ ارسال bundle تک‌تراکنشی برای تست (بدون victim)
    /// این متد برای تست اینکه Jito bundle های ما را قبول می‌کند استفاده می‌شود
    pub async fn send_single_transaction_bundle(
        &self,
        transaction: &Transaction,
        jito_endpoint: &str,
    ) -> Result<String> {
        // Serialize کردن تراکنش
        let serialized = bincode::serialize(transaction)
            .map_err(|e| anyhow!("Failed to serialize transaction: {}", e))?;
        let encoded = bs58::encode(&serialized).into_string();

        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [[encoded]]  // آرایه‌ای با یک عنصر
        });

        let url = format!("{}/api/v1/bundles", jito_endpoint);

        debug!("📦 Sending single-tx bundle to Jito: {}", jito_endpoint);

        let response = self.http_client
            .post(&url)
            .json(&request_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("Jito bundle send error: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito bundle rejected: {} - {}", status, error_text));
        }

        let response_text = response.text().await.unwrap_or_default();
        let result: SendBundleResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Failed to parse Jito response: {} - Raw: {}", e, response_text))?;

        Ok(result.result)
    }

    /// ✅ ارسال bundle به endpoint مشخص
    pub async fn send_bundle_with_victim(
        &self,
        transactions: Vec<Transaction>,
        _victim_signature: Option<String>,
        jito_endpoint: Option<&str>,
    ) -> Result<String> {
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

        // ✅ استفاده از endpoint مشخص یا default
        let endpoint = jito_endpoint.unwrap_or(&self.endpoints[0]);
        let url = format!("{}/api/v1/bundles", endpoint);

        let response = self.http_client
            .post(&url)
            .json(&request_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("HTTP error: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jito error: {}", error_text));
        }

        let response_text = response.text().await.unwrap_or_default();
        let result: SendBundleResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Parse error: {}", e))?;

        Ok(result.result)
    }
}

#[derive(Debug, PartialEq)]
pub enum TargetTxStatus {
    NotFound,          // تراکنش در تاریخچه زنجیره وجود ندارد
    Processed,         // در یک block هست ولی confirmed نشده (بهترین حالت برای sandwich!)
    AlreadyConfirmed,  // Confirmed یا Finalized شده (دیگر دیر شده)
    Unknown,           // خطا یا وضعیت نامشخص
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TokenProgramType {
    TokenProgram,
    Token2022Program,
}

/// نتیجه شبیه‌سازی تراکنش victim
#[derive(Debug)]
pub struct VictimSimulationResult {
    pub will_succeed: bool,
    pub error: Option<serde_json::Value>,
    pub logs: Option<Vec<String>>,
    pub units_consumed: Option<u64>,
}
