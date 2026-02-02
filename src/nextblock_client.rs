use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_sdk::transaction::VersionedTransaction;
use log::{debug, info};
use std::time::Duration;

/// NextBlock API client for submitting bundles
///
/// NextBlock processes bundles as atomic Jito-compatible bundles
/// All transactions succeed or all fail together
pub struct NextBlockClient {
    http_client: Client,
    endpoint: String,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct SubmitBatchRequest {
    entries: Vec<TransactionEntry>,
}

#[derive(Debug, Serialize)]
struct TransactionEntry {
    transaction: TransactionContent,
}

#[derive(Debug, Serialize)]
struct TransactionContent {
    content: String,  // Base64-encoded transaction
}

#[derive(Debug, Deserialize)]
struct SubmitBatchResponse {
    signature: Option<String>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

impl NextBlockClient {
    /// Create a new NextBlock client
    ///
    /// # Arguments
    /// * `endpoint` - NextBlock API endpoint (e.g., "http://frankfurt.nextblock.io/api/v2/submit-batch")
    /// * `api_key` - Your NextBlock API key
    pub fn new(endpoint: &str, api_key: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(10)
            .tcp_nodelay(true)  // Disable Nagle's algorithm for lower latency
            .build()
            .expect("Failed to create NextBlock HTTP client");

        Self {
            http_client: client,
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// Submit a bundle to NextBlock
    ///
    /// # Arguments
    /// * `transactions` - Vector of VersionedTransaction to submit as atomic bundle
    ///
    /// # Returns
    /// * Bundle signature if successful
    ///
    /// # Notes
    /// - Transactions are encoded as Base64 (different from Jito's Base58!)
    /// - All transactions must include tip to NextBlock tip accounts
    /// - Bundles are processed atomically (all succeed or all fail)
    pub async fn send_bundle(&self, transactions: Vec<VersionedTransaction>) -> Result<String> {
        // Convert transactions to Base64 (NextBlock uses Base64, not Base58!)
        let entries: Vec<TransactionEntry> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .expect("Failed to serialize transaction");

                // ✅ NextBlock uses Base64 encoding
                use base64::{Engine as _, engine::general_purpose::STANDARD};
                let base64_encoded = STANDARD.encode(&serialized);

                TransactionEntry {
                    transaction: TransactionContent {
                        content: base64_encoded,
                    },
                }
            })
            .collect();

        let request_body = SubmitBatchRequest { entries };

        debug!("📤 Sending {} transactions to NextBlock: {}",
            transactions.len(),
            self.endpoint
        );

        // Send request with Authorization header
        let response = self.http_client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Authorization", &self.api_key)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| anyhow!("NextBlock connection error: {}", e))?;

        let status = response.status();
        let response_text = response.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "NextBlock rejected bundle ({}): {}",
                status,
                response_text
            ));
        }

        // Parse response
        let api_response: SubmitBatchResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Failed to parse NextBlock response: {} | Raw: {}", e, response_text))?;

        if let Some(err) = api_response.error {
            return Err(anyhow!("NextBlock API error: {:?}", err));
        }

        api_response
            .signature
            .ok_or_else(|| anyhow!("No signature in NextBlock response"))
    }
}
