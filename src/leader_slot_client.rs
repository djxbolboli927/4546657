use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use log::{info, debug, error};
use std::time::Duration;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LeaderSlot {
    pub slot: u64,
    pub leader: String,
    pub region: Option<String>,
    pub ping_ms: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LeaderSlotsResponse {
    jsonrpc: String,
    #[serde(default)]
    result: Option<Vec<LeaderSlot>>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    id: u64,
}

pub struct LeaderSlotClient {
    http_client: Client,
    rpc_endpoint: String,
}

impl LeaderSlotClient {
    pub fn new(rpc_endpoint: String) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(5)) // کاهش timeout برای سرعت بیشتر
            .build()
            .unwrap();

        info!("📍 Leader Slot API Client initialized (ERPC specific)");
        info!("   Endpoint: {}", rpc_endpoint);

        Self {
            http_client,
            rpc_endpoint,
        }
    }

    /// ✅ اصلاح شده: دریافت لیدرهای آینده از یک اسلات مشخص
    /// طبق داکیومنت ERPC، پارامتر ورودی start_slot است، نه limit.
    pub async fn get_leader_slots_from(&self, start_slot: u64) -> Result<Vec<LeaderSlot>> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLeaderSlots",
            "params": [start_slot] // ✅ پارامتر صحیح: اسلات شروع (مثلا 245000000)
        });

        let response = self.http_client
            .post(&self.rpc_endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| anyhow!("Leader Slot API request failed: {}", e))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Leader Slot API HTTP error: {}", error_text));
        }

        let response_text = response.text().await.unwrap_or_default();

        let leader_response: LeaderSlotsResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Failed to parse leader slots response: {} | Response: {}", e, response_text))?;

        if let Some(err) = leader_response.error {
            return Err(anyhow!("Leader Slot API Error: {:?}", err));
        }

        leader_response.result.ok_or_else(|| anyhow!("Empty result from Leader Slot API"))
    }

    /// ✅ بررسی اینکه آیا لیدر اسلات هدف در منطقه مناسب (اروپا) است؟
    /// target_slot: اسلاتی که تراکنش قرار است در آن ماین شود (معمولا current + 2 تا 5)
    pub async fn is_leader_in_region(&self, target_slot: u64, allowed_regions: &[&str]) -> bool {
        // درخواست دیتای لیدرها از اسلات هدف
        match self.get_leader_slots_from(target_slot).await {
            Ok(slots) => {
                // بررسی اولین چند اسلات (تا 10 تا بعدی)
                for leader_info in slots.iter().take(10) {
                    if leader_info.slot >= target_slot && leader_info.slot <= target_slot + 10 {
                        if let Some(region) = &leader_info.region {
                            let region_lower = region.to_lowercase();
                            for allowed in allowed_regions {
                                if region_lower.contains(&allowed.to_lowercase()) {
                                    debug!("✅ Leader for slot {} is in {} (Allowed)", leader_info.slot, region);
                                    return true;
                                }
                            }
                        }
                    }
                }
                debug!("⚠️ No European leader found in next 10 slots from {}", target_slot);
            }
            Err(e) => {
                error!("❌ Failed to fetch leader info: {}", e);
            }
        }
        false
    }
}
