//! Leader Oracle Module - Geographic-Aware Block Leader Detection
//!
//! این ماژول با استفاده از ERPC Leader Slot Information API
//! بلاک لیدرهای آینده را شناسایی کرده و تشخیص می‌دهد که آیا در اروپا هستند یا خیر

use anyhow::{Result, anyhow};
use log::{info, warn, error, debug};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ═══════════════════════════════════════════════════════════════
// تنظیمات فیلترینگ جغرافیایی
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct GeoConfig {
    /// لیست مناطق مجاز (مثلاً ["Europe", "EU"])
    pub allowed_regions: Vec<String>,
    /// لیست کدهای کشور مجاز (مثلاً ["DE", "NL", "FR", "GB"])
    pub allowed_countries: Vec<String>,
    /// حداکثر تأخیر مجاز از فرانکفورت (میلی‌ثانیه)
    pub max_latency_ms: u64,
}

impl Default for GeoConfig {
    fn default() -> Self {
        Self {
            // مناطق اروپایی
            allowed_regions: vec!["Europe".to_string(), "EU".to_string()],
            // کشورهای اصلی اروپا که Solana validators دارند
            allowed_countries: vec![
                "DE".to_string(), // آلمان
                "NL".to_string(), // هلند
                "FR".to_string(), // فرانسه
                "GB".to_string(), // انگلستان
                "CH".to_string(), // سوئیس
                "BE".to_string(), // بلژیک
                "PL".to_string(), // لهستان
                "SE".to_string(), // سوئد
                "FI".to_string(), // فنلاند
            ],
            // حداکثر 30 میلی‌ثانیه تأخیر
            max_latency_ms: 30,
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// ساختارهای داده برای ERPC API Response
// ═══════════════════════════════════════════════════════════════

/// اطلاعات یک اسلات و لیدر آن
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderSlotInfo {
    /// شماره اسلات
    pub slot: u64,

    /// کلید عمومی لیدر (validator identity)
    #[serde(default)]
    pub leader: Option<String>,

    /// نام منطقه یا دیتاسنتر (مثلاً "Europe", "US-East", "Asia-Pacific")
    #[serde(default)]
    pub region: Option<String>,

    /// کد کشور ISO (مثلاً "DE", "JP", "US")
    #[serde(default)]
    pub country: Option<String>,

    /// شهر
    #[serde(default)]
    pub city: Option<String>,

    /// پینگ اندازه‌گیری شده از فرانکفورت (میلی‌ثانیه)
    #[serde(default)]
    pub ping: Option<u64>,

    /// آدرس IP (اختیاری)
    #[serde(default)]
    pub ip: Option<String>,

    /// زمان دریافت داده (برای کش)
    #[serde(skip)]
    pub fetched_at: Option<Instant>,
}

/// پاسخ JSON-RPC از ERPC
#[derive(Debug, Deserialize)]
pub struct ErpcResponse {
    pub jsonrpc: String,
    #[serde(default)]
    pub result: Option<Vec<LeaderSlotInfo>>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    pub id: u64,
}

// ═══════════════════════════════════════════════════════════════
// LeaderOracle - هسته اصلی ماژول
// ═══════════════════════════════════════════════════════════════

pub struct LeaderOracle {
    /// HTTP client برای ارتباط با API
    client: Client,

    /// آدرس endpoint ERPC
    api_url: String,

    /// توکن احراز هویت
    api_key: String,

    /// کش اطلاعات اسلات‌های آینده (slot -> info)
    cache: Arc<RwLock<HashMap<u64, LeaderSlotInfo>>>,

    /// تنظیمات فیلتر جغرافیایی
    config: GeoConfig,

    /// آخرین slot که برای آن درخواست زده‌ایم
    last_fetched_slot: Arc<RwLock<Option<u64>>>,

    /// تعداد اسلات‌هایی که در هر درخواست fetch می‌کنیم
    fetch_ahead_count: u64,
}

impl LeaderOracle {
    /// ساخت instance جدید
    ///
    /// # Arguments
    /// * `api_url` - آدرس ERPC endpoint (مثلاً "https://edge.erpc.global")
    /// * `api_key` - کلید API
    /// * `config` - تنظیمات فیلترینگ جغرافیایی
    pub fn new(api_url: &str, api_key: &str, config: GeoConfig) -> Self {
        info!("🌍 Leader Oracle initializing...");
        info!("   API Endpoint: {}", api_url);
        info!("   Allowed Regions: {:?}", config.allowed_regions);
        info!("   Allowed Countries: {:?}", config.allowed_countries);
        info!("   Max Latency: {} ms", config.max_latency_ms);

        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(5)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_url: api_url.to_string(),
            api_key: api_key.to_string(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            config,
            last_fetched_slot: Arc::new(RwLock::new(None)),
            fetch_ahead_count: 100, // fetch 100 slots جلوتر
        }
    }

    /// به‌روزرسانی schedule لیدرها از API
    ///
    /// این متد باید به‌صورت دوره‌ای (مثلاً هر چند ثانیه) فراخوانی شود
    /// تا کش همیشه پر از اطلاعات اسلات‌های آینده باشد
    pub async fn update_schedule(&self, start_slot: u64) -> Result<usize> {
        debug!("🔄 Updating leader schedule from slot {}", start_slot);

        // ساخت JSON-RPC request
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLeaderSlots",
            "params": [start_slot, self.fetch_ahead_count]
        });

        // ارسال درخواست به ERPC
        let response = self.client
            .post(&self.api_url)
            .header("Content-Type", "application/json")
            .query(&[("api-key", &self.api_key)]) // کلید API در query string
            .json(&request_body)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to connect to ERPC API: {}", e))?;

        // بررسی HTTP status
        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("ERPC API HTTP error {}: {}", status, error_text));
        }

        // پارس کردن پاسخ
        let response_text = response.text().await?;
        let parsed: ErpcResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                error!("❌ Failed to parse ERPC response: {}", e);
                error!("   Raw response: {}", response_text);
                anyhow!("JSON parse error: {}", e)
            })?;

        // بررسی خطای API
        if let Some(err) = parsed.error {
            return Err(anyhow!("ERPC API Error: {:?}", err));
        }

        // استخراج نتیجه
        let leader_slots = parsed.result.ok_or_else(|| anyhow!("Empty result from ERPC"))?;

        // به‌روزرسانی کش
        let mut cache = self.cache.write().await;

        // پاکسازی اسلات‌های قدیمی برای مدیریت حافظه
        cache.retain(|&slot, _| slot >= start_slot);

        let mut added_count = 0;
        let now = Instant::now();

        for mut info in leader_slots {
            // افزودن timestamp برای کش
            info.fetched_at = Some(now);
            cache.insert(info.slot, info);
            added_count += 1;
        }

        // به‌روزرسانی آخرین slot
        *self.last_fetched_slot.write().await = Some(start_slot);

        info!("✅ Leader schedule updated: {} slots cached (from slot {})", added_count, start_slot);

        Ok(added_count)
    }

    /// بررسی اینکه آیا می‌توانیم در این slot معامله کنیم یا نه
    ///
    /// این متد اصلی‌ترین تابع برای ربات شماست.
    /// قبل از هر معامله باید این را صدا بزنید.
    ///
    /// # Returns
    /// * `true` - لیدر در اروپاست یا latency کم دارد → می‌توان trade کرد
    /// * `false` - لیدر خارج از اروپاست و latency بالا دارد → نباید trade کرد
    pub async fn can_trade(&self, current_slot: u64) -> bool {
        let cache = self.cache.read().await;

        // بررسی اینکه اطلاعات این slot در کش هست یا نه
        if let Some(info) = cache.get(&current_slot) {

            // ============================================================
            // استراتژی تصمیم‌گیری (به ترتیب اولویت):
            // ============================================================

            // 1️⃣ بررسی PING (بالاترین اولویت)
            //    اگر ping کمتر از حد تعیین شده باشد، موقعیت مکانی مهم نیست
            if let Some(ping) = info.ping {
                if ping <= self.config.max_latency_ms {
                    debug!("✅ Slot {}: TRADE ALLOWED (Low Ping: {} ms)", current_slot, ping);
                    return true;
                }
            }

            // 2️⃣ بررسی کد کشور
            if let Some(ref country) = info.country {
                if self.config.allowed_countries.contains(country) {
                    debug!("✅ Slot {}: TRADE ALLOWED (Country: {})", current_slot, country);
                    return true;
                }
            }

            // 3️⃣ بررسی منطقه (Region)
            if let Some(ref region) = info.region {
                // تطبیق جزئی رشته (مثلاً "Europe-West" شامل "Europe" می‌شود)
                for allowed in &self.config.allowed_regions {
                    if region.to_lowercase().contains(&allowed.to_lowercase()) {
                        debug!("✅ Slot {}: TRADE ALLOWED (Region: {})", current_slot, region);
                        return true;
                    }
                }
            }

            // ❌ هیچ شرطی برقرار نشد → لیدر خارج از اروپا
            warn!("⛔ Slot {}: TRADE BLOCKED (Outside Europe)", current_slot);
            if let Some(country) = &info.country {
                warn!("   Country: {}", country);
            }
            if let Some(region) = &info.region {
                warn!("   Region: {}", region);
            }
            if let Some(ping) = info.ping {
                warn!("   Ping: {} ms (max: {} ms)", ping, self.config.max_latency_ms);
            }

            return false;
        }

        // اگر اطلاعات در کش نیست، سیاست محافظه‌کارانه: عدم معامله
        warn!("⚠️  Slot {}: NO DATA IN CACHE - Trade blocked (safety)", current_slot);
        false
    }

    /// انتخاب بهترین Jito Block Engine endpoint بر اساس موقعیت لیدر
    ///
    /// استفاده:
    /// ```rust
    /// let endpoint = oracle.get_optimal_jito_endpoint(current_slot).await;
    /// // ارسال bundle به این endpoint
    /// ```
    pub async fn get_optimal_jito_endpoint(&self, slot: u64) -> String {
        let cache = self.cache.read().await;

        // Default: فرانکفورت (چون سرور ما آنجاست)
        let default = "https://frankfurt.mainnet.block-engine.jito.wtf".to_string();

        if let Some(info) = cache.get(&slot) {
            // انتخاب بر اساس کشور
            if let Some(country) = &info.country {
                match country.as_str() {
                    "NL" => {
                        debug!("🇳🇱 Slot {}: Using Amsterdam Jito endpoint", slot);
                        return "https://amsterdam.mainnet.block-engine.jito.wtf".to_string();
                    }
                    "DE" => {
                        debug!("🇩🇪 Slot {}: Using Frankfurt Jito endpoint", slot);
                        return "https://frankfurt.mainnet.block-engine.jito.wtf".to_string();
                    }
                    "GB" | "FR" | "BE" | "CH" => {
                        // کشورهای غرب اروپا → فرانکفورت یا آمستردام
                        debug!("🇪🇺 Slot {}: Using Frankfurt Jito endpoint (West Europe)", slot);
                        return default;
                    }
                    "US" => {
                        debug!("🇺🇸 Slot {}: Using New York Jito endpoint", slot);
                        return "https://ny.mainnet.block-engine.jito.wtf".to_string();
                    }
                    "JP" | "SG" | "KR" => {
                        debug!("🇯🇵 Slot {}: Using Tokyo Jito endpoint", slot);
                        return "https://tokyo.mainnet.block-engine.jito.wtf".to_string();
                    }
                    _ => {}
                }
            }

            // انتخاب بر اساس region
            if let Some(region) = &info.region {
                if region.to_lowercase().contains("europe") {
                    return default;
                } else if region.to_lowercase().contains("asia") {
                    return "https://tokyo.mainnet.block-engine.jito.wtf".to_string();
                } else if region.to_lowercase().contains("us") || region.to_lowercase().contains("america") {
                    return "https://ny.mainnet.block-engine.jito.wtf".to_string();
                }
            }
        }

        // Default fallback
        debug!("ℹ️  Slot {}: Using default Frankfurt endpoint", slot);
        default
    }

    /// دریافت اطلاعات کامل یک اسلات (برای debugging)
    pub async fn get_slot_info(&self, slot: u64) -> Option<LeaderSlotInfo> {
        self.cache.read().await.get(&slot).cloned()
    }

    /// بررسی اینکه آیا باید schedule را update کنیم یا نه
    ///
    /// این متد بررسی می‌کند که آیا نزدیک به انتهای buffer هستیم
    pub async fn should_update_schedule(&self, current_slot: u64) -> bool {
        let last_slot = self.last_fetched_slot.read().await;

        match *last_slot {
            None => true, // هنوز هیچ fetch نکرده‌ایم
            Some(last) => {
                // اگر به 20 اسلات آخر buffer رسیدیم، باید update کنیم
                let buffer_remaining = (last + self.fetch_ahead_count).saturating_sub(current_slot);
                buffer_remaining < 20
            }
        }
    }

    /// تعداد اسلات‌های موجود در کش
    pub async fn cache_size(&self) -> usize {
        self.cache.read().await.len()
    }

    /// آمار کش (برای monitoring)
    pub async fn get_cache_stats(&self) -> CacheStats {
        let cache = self.cache.read().await;
        let slots: Vec<u64> = cache.keys().copied().collect();

        CacheStats {
            total_slots: slots.len(),
            min_slot: slots.iter().min().copied(),
            max_slot: slots.iter().max().copied(),
            europe_count: cache.values()
                .filter(|info| {
                    info.country.as_ref()
                        .map(|c| self.config.allowed_countries.contains(c))
                        .unwrap_or(false)
                })
                .count(),
        }
    }
}

/// آمار کش
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_slots: usize,
    pub min_slot: Option<u64>,
    pub max_slot: Option<u64>,
    pub europe_count: usize,
}

// ═══════════════════════════════════════════════════════════════
// Background Updater Task
// ═══════════════════════════════════════════════════════════════

/// تسک پس‌زمینه برای به‌روزرسانی خودکار leader schedule
///
/// این تابع را در یک tokio::spawn اجرا کنید تا به‌صورت خودکار
/// اطلاعات لیدرها را fetch کند
pub async fn start_leader_schedule_updater(
    oracle: Arc<LeaderOracle>,
    mut current_slot_rx: tokio::sync::watch::Receiver<u64>,
) {
    info!("🔄 Leader Schedule Updater started");

    let mut last_update = Instant::now();
    let update_interval = Duration::from_secs(10); // هر 10 ثانیه چک می‌کنیم

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        // دریافت آخرین slot
        let current_slot = *current_slot_rx.borrow_and_update();

        // بررسی اینکه آیا نیاز به update هست
        if oracle.should_update_schedule(current_slot).await
            || last_update.elapsed() > update_interval
        {
            match oracle.update_schedule(current_slot).await {
                Ok(count) => {
                    last_update = Instant::now();

                    // لاگ آمار
                    let stats = oracle.get_cache_stats().await;
                    info!("📊 Cache Stats: {} total slots, {} in Europe ({:.1}%)",
                        stats.total_slots,
                        stats.europe_count,
                        (stats.europe_count as f64 / stats.total_slots as f64) * 100.0
                    );
                }
                Err(e) => {
                    error!("❌ Failed to update leader schedule: {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geo_config_default() {
        let config = GeoConfig::default();
        assert!(config.allowed_countries.contains(&"DE".to_string()));
        assert_eq!(config.max_latency_ms, 30);
    }

    #[tokio::test]
    async fn test_leader_oracle_creation() {
        let config = GeoConfig::default();
        let oracle = LeaderOracle::new(
            "https://test.erpc.global",
            "test-key",
            config
        );

        assert_eq!(oracle.cache_size().await, 0);
    }
}
