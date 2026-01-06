use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::Transaction;
use log::{info, warn, error, debug};
use std::time::Duration;

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

#[derive(Deserialize, Debug)]
struct SendBundleResponse {
    result: String,
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
