use anyhow::{Context, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Stdout,
    None,
}

#[derive(Clone)]
pub struct Config {
    pub upstream_endpoint: String,
    pub x_token: String,
    pub mix_json: String,
    pub max_accounts: usize,
    pub output_mode: OutputMode,
    /// When true, emit full base64 account data (heavy). Off by default.
    pub dump_json: bool,
    /// Unix socket path to serve decoded updates to the bot.
    /// Set BOT_SOCKET_PATH to enable. Empty = disabled.
    pub socket_path: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let upstream_endpoint = std::env::var("UPSTREAM_YELLOWSTONE_ENDPOINT")
            .context("UPSTREAM_YELLOWSTONE_ENDPOINT not set")?;
        let x_token = std::env::var("UPSTREAM_YELLOWSTONE_X_TOKEN").unwrap_or_default();
        let mix_json = std::env::var("MIX_JSON")
            .unwrap_or_else(|_| "/root/c/metis/1/mix.json".to_string());
        let max_accounts = std::env::var("MAX_ACCOUNTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);

        let output_mode = match std::env::var("BOT_OUTPUT_MODE").as_deref() {
            Ok("none") => OutputMode::None,
            _ => OutputMode::Stdout,
        };

        let dump_json = std::env::args().any(|a| a == "--dump-json")
            || matches!(std::env::var("DUMP_JSON").as_deref(), Ok("1") | Ok("true"));

        let socket_path = std::env::var("BOT_SOCKET_PATH").unwrap_or_default();

        Ok(Self {
            upstream_endpoint,
            x_token,
            mix_json,
            max_accounts,
            output_mode,
            dump_json,
            socket_path,
        })
    }
}
