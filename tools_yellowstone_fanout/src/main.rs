//! Yellowstone Fanout — Phase A
//!
//! Connects to upstream Yellowstone, subscribes to pool accounts from mix.json,
//! and broadcasts each update to:
//!   1. stdout   (human inspection / pipe to file)
//!   2. Unix socket (bot reads via pool_state_socket — no gRPC needed in bot)
//!
//! Usage (server):
//!   UPSTREAM_YELLOWSTONE_ENDPOINT=https://...
//!   UPSTREAM_YELLOWSTONE_X_TOKEN=...
//!   MIX_JSON=/root/c/metis/1/mix.json
//!   BOT_SOCKET_PATH=/tmp/yellowstone_fanout.sock   ← bot reads from here
//!   MAX_ACCOUNTS=2000
//!   BOT_OUTPUT_MODE=none                           ← suppress stdout on prod
//!   cargo run --release --bin yellowstone_fanout_phase_a

mod bot_sink;
mod config;
mod metrics;
mod mix;
mod upstream_client;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::from_env()?;
    info!(
        endpoint = %cfg.upstream_endpoint,
        mix = %cfg.mix_json,
        max_accounts = cfg.max_accounts,
        socket = %if cfg.socket_path.is_empty() { "disabled" } else { &cfg.socket_path },
        "yellowstone_fanout Phase A starting"
    );

    let accounts = mix::load_subscription_accounts(&cfg.mix_json, cfg.max_accounts)?;
    if accounts.is_empty() {
        anyhow::bail!("no accounts extracted from mix.json — nothing to subscribe");
    }
    info!(count = accounts.len(), "loaded subscription accounts from mix.json");

    let metrics = Arc::new(metrics::Metrics::new());
    metrics.spawn_reporter(Duration::from_secs(10));

    // Start socket server (if BOT_SOCKET_PATH is set) and get the broadcast sender.
    let tx = bot_sink::start(&cfg);

    let mut backoff = Duration::from_millis(500);
    loop {
        match upstream_client::run_stream(&cfg, &accounts, &metrics, &tx).await {
            Ok(()) => warn!("upstream stream ended cleanly, reconnecting"),
            Err(e) => warn!(error = %e, "upstream stream error, reconnecting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}
