//! Multi-MEV Client - Parallel Bundle Submission to Multiple Services
//!
//! این ماژول مسئول ارسال همزمان bundle ها به ۶ سرویس MEV مختلف است:
//! 1. Jito (Frankfurt)
//! 2. NextBlock (Frankfurt) - نیاز به API Key
//! 3. BloXroute (Germany) - نیاز به Authorization Header
//! 4. Bloom (EU)
//! 5. Nozomi (Temporal)
//! 6. 0slot
//!
//! استراتژی: Shotgun Broadcasting - اولین سرویسی که bundle را confirm کند برنده است

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use log::{info, warn, error, debug};
use std::time::Duration;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════
// Request/Response Structures
// ═══════════════════════════════════════════════════════════════

#[derive(Serialize)]
struct SendBundleRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct SendBundleResponse {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

// ═══════════════════════════════════════════════════════════════
// MEV Service Configuration
// ═══════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
pub enum MEVService {
    Jito,
    NextBlock,
    BloXroute,
    Bloom,
    Nozomi,
    ZeroSlot,
}

impl MEVService {
    fn name(&self) -> &str {
        match self {
            MEVService::Jito => "Jito",
            MEVService::NextBlock => "NextBlock",
            MEVService::BloXroute => "BloXroute",
            MEVService::Bloom => "Bloom",
            MEVService::Nozomi => "Nozomi",
            MEVService::ZeroSlot => "0slot",
        }
    }
}

pub struct ServiceConfig {
    pub endpoint: String,
    pub auth_header: Option<String>,
    pub api_key: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// MultiMEVClient - Main Structure
// ═══════════════════════════════════════════════════════════════

pub struct MultiMEVClient {
    http_client: Client,

    // Service endpoints
    jito_endpoint: String,
    nextblock_endpoint: String,
    nextblock_api_key: String,
    bloxroute_endpoint: String,
    bloxroute_auth: String,
    bloom_endpoint: String,
    nozomi_endpoint: String,
    zeroslot_endpoint: String,
}

impl MultiMEVClient {
    pub fn new(
        jito_endpoint: String,
        nextblock_endpoint: String,
        nextblock_api_key: String,
        bloxroute_endpoint: String,
        bloxroute_auth: String,
        bloom_endpoint: String,
        nozomi_endpoint: String,
        zeroslot_endpoint: String,
    ) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            http_client,
            jito_endpoint,
            nextblock_endpoint,
            nextblock_api_key,
            bloxroute_endpoint,
            bloxroute_auth,
            bloom_endpoint,
            nozomi_endpoint,
            zeroslot_endpoint,
        }
    }

    /// ارسال bundle به یک سرویس خاص
    async fn submit_to_service(
        &self,
        service: MEVService,
        bundle: &[VersionedTransaction],
    ) -> Result<String> {
        let (endpoint, auth_header, api_key) = match service {
            MEVService::Jito => (self.jito_endpoint.clone(), None, None),
            MEVService::NextBlock => (
                self.nextblock_endpoint.clone(),
                None,
                Some(self.nextblock_api_key.clone()),
            ),
            MEVService::BloXroute => (
                self.bloxroute_endpoint.clone(),
                Some(self.bloxroute_auth.clone()),
                None,
            ),
            MEVService::Bloom => (self.bloom_endpoint.clone(), None, None),
            MEVService::Nozomi => (self.nozomi_endpoint.clone(), None, None),
            MEVService::ZeroSlot => (self.zeroslot_endpoint.clone(), None, None),
        };

        // تبدیل bundle به base64
        let encoded_txs: Vec<String> = bundle
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx).unwrap();
                BASE64.encode(&serialized)
            })
            .collect();

        // ساخت JSON-RPC request (استاندارد Jito)
        let request = SendBundleRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params: vec![serde_json::json!(encoded_txs)],
        };

        // ساخت HTTP request
        let mut req = self.http_client.post(&endpoint).json(&request);

        // اضافه کردن headers بر اساس سرویس
        if let Some(auth) = auth_header {
            req = req.header("Authorization", auth);
        }
        if let Some(key) = api_key {
            req = req.header("X-API-KEY", key);
        }

        // ارسال
        let response = req
            .send()
            .await
            .map_err(|e| anyhow!("{} connection failed: {}", service.name(), e))?;

        // بررسی HTTP status
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "{} HTTP error {}: {}",
                service.name(),
                status,
                error_text
            ));
        }

        // پارس response
        let response_text = response.text().await?;
        let parsed: SendBundleResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("{} JSON parse error: {}", service.name(), e))?;

        // بررسی خطا در response
        if let Some(error) = parsed.error {
            return Err(anyhow!("{} API error: {:?}", service.name(), error));
        }

        // استخراج bundle UUID
        let bundle_uuid = parsed
            .result
            .ok_or_else(|| anyhow!("{} empty result", service.name()))?;

        debug!("✅ {} accepted bundle: {}", service.name(), bundle_uuid);
        Ok(bundle_uuid)
    }

    /// 🎯 SHOTGUN BROADCASTING - ارسال موازی به همه سرویس‌ها
    ///
    /// این متد bundle را همزمان به تمام ۶ سرویس ارسال می‌کند.
    /// اولین سرویسی که موفق شود، UUID را برمی‌گرداند.
    ///
    /// # Returns
    /// - Ok((service_name, bundle_uuid)) - نام سرویس موفق و UUID bundle
    /// - Err - اگر همه سرویس‌ها شکست خوردند
    pub async fn submit_bundle_parallel(
        &self,
        bundle: Vec<VersionedTransaction>,
    ) -> Result<(String, String)> {
        info!("🚀 Shotgun Broadcasting: Sending bundle to 6 MEV services...");

        // ارسال موازی به همه سرویس‌ها
        let bundle_arc = Arc::new(bundle);

        let (r1, r2, r3, r4, r5, r6) = tokio::join!(
            self.submit_to_service(MEVService::Jito, &bundle_arc),
            self.submit_to_service(MEVService::NextBlock, &bundle_arc),
            self.submit_to_service(MEVService::BloXroute, &bundle_arc),
            self.submit_to_service(MEVService::Bloom, &bundle_arc),
            self.submit_to_service(MEVService::Nozomi, &bundle_arc),
            self.submit_to_service(MEVService::ZeroSlot, &bundle_arc),
        );

        // بررسی نتایج - اولین موفقیت را برمی‌گردانیم
        let results = vec![
            ("Jito", r1),
            ("NextBlock", r2),
            ("BloXroute", r3),
            ("Bloom", r4),
            ("Nozomi", r5),
            ("0slot", r6),
        ];

        let mut success_count = 0;
        let mut first_success: Option<(String, String)> = None;
        let mut errors = Vec::new();

        for (service_name, result) in results {
            match result {
                Ok(uuid) => {
                    success_count += 1;
                    if first_success.is_none() {
                        first_success = Some((service_name.to_string(), uuid.clone()));
                    }
                    info!("  ✅ {}: {}", service_name, uuid);
                }
                Err(e) => {
                    errors.push(format!("{}: {}", service_name, e));
                    debug!("  ❌ {}: {}", service_name, e);
                }
            }
        }

        info!(
            "📊 Broadcasting result: {}/6 services accepted bundle",
            success_count
        );

        // اگر حداقل یک سرویس موفق شد
        if let Some((service, uuid)) = first_success {
            info!("🏆 First acceptance: {} (UUID: {})", service, uuid);
            return Ok((service, uuid));
        }

        // اگر همه شکست خوردند
        error!("❌ ALL 6 services rejected bundle!");
        for err in &errors {
            error!("   • {}", err);
        }

        Err(anyhow!("All MEV services rejected bundle: {:?}", errors))
    }
}
