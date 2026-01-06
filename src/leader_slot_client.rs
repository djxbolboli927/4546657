use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use log::{info, debug, error};
use std::time::Duration;

#[derive(Debug, Deserialize)]
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
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        info!("📍 Leader Slot API Client initialized");
        info!("   Endpoint: {}", rpc_endpoint);

        Self {
            http_client,
            rpc_endpoint,
        }
    }

    /// دریافت لیست leader slots آینده (برای شناسایی بهترین timing)
    pub async fn get_upcoming_leader_slots(&self, limit: usize) -> Result<Vec<LeaderSlot>> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLeaderSlots",
            "params": [limit]
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
            .map_err(|e| anyhow!("Failed to parse leader slots response: {}", e))?;

        if let Some(err) = leader_response.error {
            return Err(anyhow!("Leader Slot API Error: {:?}", err));
        }

        leader_response.result.ok_or_else(|| anyhow!("Empty result from Leader Slot API"))
    }

    /// بررسی کنید که آیا slot فعلی در منطقه ما leader دارد یا نه
    pub async fn is_optimal_slot(&self, current_slot: u64, target_region: &str) -> Result<bool> {
        let upcoming_slots = self.get_upcoming_leader_slots(20).await?;

        for leader_slot in upcoming_slots.iter() {
            if leader_slot.slot >= current_slot && leader_slot.slot <= current_slot + 5 {
                if let Some(region) = &leader_slot.region {
                    if region.to_lowercase().contains(&target_region.to_lowercase()) {
                        debug!("✅ Optimal slot found! Slot {} has leader in {}", leader_slot.slot, region);
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }

    /// دریافت بهترین slot بعدی با کمترین latency
    pub async fn get_next_optimal_slot(&self, target_region: &str) -> Result<Option<LeaderSlot>> {
        let upcoming_slots = self.get_upcoming_leader_slots(50).await?;

        let mut best_slot: Option<LeaderSlot> = None;
        let mut best_ping = f64::MAX;

        for slot in upcoming_slots {
            if let Some(region) = &slot.region {
                if region.to_lowercase().contains(&target_region.to_lowercase()) {
                    let ping = slot.ping_ms.unwrap_or(999.0);
                    if ping < best_ping {
                        best_ping = ping;
                        best_slot = Some(slot);
                    }
                }
            }
        }

        Ok(best_slot)
    }
}
