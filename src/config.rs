use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub metis: MetisConfig,
    pub trading: TradingConfig,
    pub jito: JitoConfig,
    pub rpc: RpcConfig,
    pub yellowstone_grpc: YellowstoneGrpcConfig,
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub simulation: SimulationConfig,
    #[serde(default)]
    pub jito_grpc: JitoGrpcConfig,
    #[serde(default)]
    pub template_cache: TemplateCacheConfig,
    #[serde(default)]
    pub pool_state: PoolStateConfig,
    #[serde(default)]
    pub jupiter_price: JupiterPriceConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MetisConfig {
    pub url: String,
    #[allow(dead_code)]
    pub binary_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TradingConfig {
    pub min_amount_sol: f64,
    pub max_amount_sol: f64,
    pub step_sol: f64,
    pub min_profit_lamports: u64,
    /// Standard Solana transaction fee in lamports (5000 = one signature fee).
    #[allow(dead_code)]
    pub base_fee_lamports: u64,
    pub tokens_file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JitoConfig {
    /// Multiple Jito block engine URLs -- bundles are sent to ALL concurrently.
    pub urls: Vec<String>,
    pub uuid: String,
    pub trading_keypair: String,
    #[allow(dead_code)]
    pub tip_min_lamports: u64,
    #[allow(dead_code)]
    pub tip_max_lamports: u64,
    #[allow(dead_code)]
    pub tip_profit_percent: f64,
    pub max_bundles_per_second: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RpcConfig {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct YellowstoneGrpcConfig {
    pub endpoint: String,
    pub x_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SimulationConfig {
    /// If false, bot sends every profitable tx without any local sim gate
    /// (pre-LiteSVM behaviour). Default: disabled so legacy configs keep
    /// working until the operator opts in.
    #[serde(default)]
    pub enabled: bool,
    /// Directory containing the DEX .so binaries listed in `program_registry`.
    #[serde(default = "default_so_dir")]
    pub so_dir: String,
    /// Directory containing per-pool account files (`dex/<DEX>/<pool>.toml`).
    /// These are pre-fetched at startup and the vault accounts within are
    /// subscribed for live Yellowstone updates.
    #[serde(default = "default_dex_dir")]
    pub dex_dir: String,
    /// When sim reverts or errors, `fail_closed=true` drops the send (safest);
    /// `false` logs and forwards to Jito anyway (useful during rollout).
    #[serde(default = "default_true")]
    pub fail_closed: bool,
    /// Number of INDEPENDENT Simulator instances to spin up. Each Simulator
    /// owns its own `Mutex<LiteSVM>`, so N workers = N sims in parallel.
    /// Sizing guidance: in steady state each sim takes ~2-5ms of CPU, so
    /// `workers` should roughly equal the peak number of profitable
    /// opportunities that arrive per 5ms window. In production, 8 is a
    /// sensible default (handles ~1600 sims/sec with headroom).
    #[serde(default = "default_workers")]
    pub workers: usize,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            so_dir: default_so_dir(),
            dex_dir: default_dex_dir(),
            fail_closed: true,
            workers: default_workers(),
        }
    }
}

fn default_so_dir() -> String {
    "/home/soluser/m/so".to_string()
}

fn default_dex_dir() -> String {
    "vendor/litesvm/dex".to_string()
}

fn default_true() -> bool {
    true
}

fn default_workers() -> usize {
    8
}

/// Second Jito submission path via SearcherService gRPC.
///
/// Runs alongside the REST UUID client in `jito.rs`. Each path has its
/// own rate limiter, so the effective Jito throughput is
/// `jito.max_bundles_per_second + jito_grpc.max_bundles_per_second`.
///
/// Like the REST client, gRPC fans out to every regional Block Engine
/// endpoint concurrently — first regional success wins. Per-region auth
/// is attempted using the whitelisted keypair, which gives 5 req/s per
/// region. Regions whose auth fails downgrade to no-auth mode (1 req/s).
#[derive(Debug, Deserialize, Clone)]
pub struct JitoGrpcConfig {
    /// If false, only the REST UUID path is used (pre-gRPC behaviour).
    #[serde(default)]
    pub enabled: bool,
    /// All Jito Block Engine gRPC endpoints. Bundles are broadcast to ALL
    /// of these per send call, mirroring the REST multi-region fan-out.
    #[serde(default = "default_jito_grpc_endpoints")]
    pub endpoints: Vec<String>,
    /// Path to the Solana keypair JSON whose pubkey Jito has whitelisted
    /// for gRPC auth. This wallet holds no funds — it is an identity only.
    /// If empty or auth fails, regions fall back to no-auth (1 req/s).
    #[serde(default)]
    pub auth_keypair: String,
    /// Per-second rate limit applied *before* the gRPC SendBundle call.
    /// REST and gRPC limiters operate independently.
    #[serde(default = "default_grpc_rate")]
    pub max_bundles_per_second: u32,
}

impl Default for JitoGrpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoints: default_jito_grpc_endpoints(),
            auth_keypair: String::new(),
            max_bundles_per_second: default_grpc_rate(),
        }
    }
}

fn default_jito_grpc_endpoints() -> Vec<String> {
    vec![
        "https://amsterdam.mainnet.block-engine.jito.wtf".to_string(),
        "https://dublin.mainnet.block-engine.jito.wtf".to_string(),
        "https://frankfurt.mainnet.block-engine.jito.wtf".to_string(),
        "https://london.mainnet.block-engine.jito.wtf".to_string(),
        "https://ny.mainnet.block-engine.jito.wtf".to_string(),
        "https://slc.mainnet.block-engine.jito.wtf".to_string(),
        "https://singapore.mainnet.block-engine.jito.wtf".to_string(),
        "https://tokyo.mainnet.block-engine.jito.wtf".to_string(),
    ]
}

fn default_grpc_rate() -> u32 {
    5
}

#[derive(Debug, Deserialize, Clone)]
pub struct PerformanceConfig {
    /// Number of tokio worker threads (multi-thread runtime).
    pub threads: usize,
    pub quote_timeout_ms: u64,
    /// CU limits per hop count: index 0 = 2 hops, index 1 = 3 hops, etc.
    /// If hops exceed the array, the last value is used.
    pub cu_limits: Vec<u32>,
    /// Maximum in-flight Metis quote requests per scan chunk.
    /// Keeps the HTTP connection pool from being overwhelmed.
    #[serde(default = "default_max_concurrent_quotes")]
    pub max_concurrent_quotes: usize,
    /// Maximum concurrent Stage-2 calc workers (merge quotes + fire
    /// swap_instructions). With fire-and-forget each worker holds its slot
    /// only for microseconds, so this can be set high to rule out the calc
    /// stage as a bottleneck. Default 6 (legacy value).
    #[serde(default = "default_calc_workers")]
    pub calc_workers: usize,
    /// Max time (ms) a swap_instructions result may wait in the LIFO queue
    /// before being dropped by a calc worker. Tune higher to tolerate slower
    /// Metis responses; lower to discard stale opportunities faster.
    #[serde(default = "default_queue_max_age_ms")]
    pub queue_max_age_ms: u64,
    #[serde(default)]
    pub bot_cpu_cores: Vec<usize>,
}

fn default_max_concurrent_quotes() -> usize {
    512
}

fn default_calc_workers() -> usize {
    6
}

fn default_queue_max_age_ms() -> u64 {
    5000
}

/// Template cache configuration.
///
/// Rollout order:
///   1. save_new=true       — extract and store route/hop templates from Metis
///                            responses. No behaviour change yet.
///   2. serve_route=true    — serve from RouteTemplate on hit, patching
///                            in_amount / quoted_out_amount in the Borsh data.
///                            Falls back to Metis when patching is not possible.
///   3. serve_from_metis=false — RAM-only (miss = drop, no Metis call).
#[derive(Debug, Deserialize, Clone)]
pub struct TemplateCacheConfig {
    /// Extract and save route/hop templates from every Metis response.
    #[serde(default)]
    pub save_new: bool,
    /// Serve from RouteTemplate when available (patches amounts if needed).
    #[serde(default)]
    pub serve_route: bool,
    /// Call Metis for instructions when no route template hits.
    #[serde(default = "default_true_tc")]
    pub serve_from_metis: bool,
}

impl Default for TemplateCacheConfig {
    fn default() -> Self {
        Self {
            save_new: false,
            serve_route: false,
            serve_from_metis: true,
        }
    }
}

fn default_true_tc() -> bool {
    true
}

// ── Pool state stream config ──────────────────────────────────────────────────

/// Configuration for the live pool-account state stream (Phase 2).
///
/// When `enabled = true`, the bot connects to the same Yellowstone endpoint
/// used by the account_cache, subscribes to all pool accounts listed in
/// `mix_json`, and keeps a live PoolStateStore in memory.
///
/// This store feeds the per-DEX price/slippage calculators (Phase 2 step 2).
/// In Phase A it simply receives and stores data — no calculator is wired yet.
#[derive(Debug, Deserialize, Clone)]
pub struct PoolStateConfig {
    /// Explicit data-source selector. One of:
    ///   "direct_grpc_fast" — bot subscribes directly to Yellowstone gRPC.
    ///   "relay_socket"     — bot reads decoded updates from a Unix socket.
    ///   "disabled"         — no pool-state stream.
    /// When empty, falls back to the legacy `enabled`/`socket` booleans.
    #[serde(default)]
    pub mode: String,
    /// Legacy: enable direct Yellowstone gRPC subscription (used only when
    /// `mode` is empty). Prefer `mode = "direct_grpc_fast"`.
    #[serde(default)]
    pub enabled: bool,
    /// Path to mix.json (Metis market cache).
    #[serde(default = "default_mix_json")]
    pub mix_json: String,
    /// Unix socket written by yellowstone_fanout_phase_a.
    /// Used only when `mode = "relay_socket"` (or legacy: non-empty socket).
    /// Set to the same value as BOT_SOCKET_PATH in the fanout process.
    /// Example: /tmp/yellowstone_fanout.sock
    #[serde(default)]
    pub socket: String,
    /// Cap on the number of subscribed accounts. 0 = no limit (production).
    /// A non-zero value truncates the subscription list and logs a warning —
    /// useful only for small connectivity tests.
    #[serde(default)]
    pub max_accounts: usize,
    /// Accounts per gRPC subscription stream. 0 = single stream (no sharding).
    /// Set to e.g. 1000–2000 if the provider limits accounts per request;
    /// the list is split into chunks, each on its own stream, all writing to
    /// the same PoolStateStore.
    #[serde(default)]
    pub accounts_per_stream: usize,
}

fn default_mix_json() -> String {
    "/root/c/metis/1/mix.json".to_string()
}

impl Default for PoolStateConfig {
    fn default() -> Self {
        Self {
            mode: String::new(),
            enabled: false,
            mix_json: default_mix_json(),
            socket: String::new(),
            max_accounts: 0,
            accounts_per_stream: 0,
        }
    }
}

impl PoolStateConfig {
    /// Resolve the effective data source, honouring the explicit `mode` first
    /// and falling back to the legacy boolean/socket fields.
    pub fn resolved_mode(&self) -> &str {
        match self.mode.trim() {
            "direct_grpc_fast" | "relay_socket" | "disabled" => self.mode.trim(),
            "" => {
                if !self.socket.is_empty() {
                    "relay_socket"
                } else if self.enabled {
                    "direct_grpc_fast"
                } else {
                    "disabled"
                }
            }
            // Unknown value — treat as disabled but the caller logs it.
            _ => "invalid",
        }
    }
}

// ── Jupiter Price API config ──────────────────────────────────────────────────

/// Jupiter Price API V3 settings.
/// Used by the price validator to fetch USD reference prices.
/// Free tier: 1 RPS / 60 RPM; up to 50 mint IDs per request.
#[derive(Debug, Deserialize, Clone)]
pub struct JupiterPriceConfig {
    /// Base URL, e.g. "https://api.jup.ag/price/v3"
    #[serde(default = "default_jupiter_url")]
    pub url: String,
    /// x-api-key header value. Empty string = no auth (public endpoint).
    #[serde(default)]
    pub api_key: String,
}

fn default_jupiter_url() -> String {
    "https://api.jup.ag/price/v3".to_string()
}

impl Default for JupiterPriceConfig {
    fn default() -> Self {
        Self {
            url: default_jupiter_url(),
            api_key: String::new(),
        }
    }
}

// ── Validation mode config ────────────────────────────────────────────────────

/// When `enabled = true`, the bot starts ONLY the pool-state stream and the
/// price validator — the normal Metis/Jito scan loop is not started.
/// No transactions are sent in this mode.
#[derive(Debug, Deserialize, Clone)]
pub struct ValidationConfig {
    /// Enable validation mode. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// How often to run a comparison cycle (seconds). Default: 5.
    #[serde(default = "default_val_interval")]
    pub interval_secs: u64,
    /// Maximum number of pool lines to print per cycle (sorted by diff).
    /// Use 0 for all. Default: 20.
    #[serde(default = "default_val_max_log")]
    pub max_pools_log: usize,
}

fn default_val_interval() -> u64 { 5 }
fn default_val_max_log() -> usize { 20 }

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 5,
            max_pools_log: 20,
        }
    }
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
