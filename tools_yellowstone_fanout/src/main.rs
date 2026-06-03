//! Yellowstone Fanout — Phase A
//!
//! Standalone test tool. Connects directly to the upstream Yellowstone
//! (PublicNode) gRPC endpoint, subscribes to a small set of accounts derived
//! from mix.json, decodes the account updates, and emits a compact summary to
//! the bot sink (stdout). It does NOT touch Metis in any way.
//!
//! Goal: prove the bot can receive and correctly decode the same raw account
//! state that Metis receives. Phase B (relay in front of Metis) comes later.

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
        "yellowstone_fanout Phase A starting"
    );

    let accounts = mix::load_subscription_accounts(&cfg.mix_json, cfg.max_accounts)?;
    if accounts.is_empty() {
        anyhow::bail!("no accounts extracted from mix.json — nothing to subscribe");
    }
    info!(count = accounts.len(), "loaded subscription accounts from mix.json");

    let metrics = Arc::new(metrics::Metrics::new());
    metrics.spawn_reporter(Duration::from_secs(10));

    // Reconnect loop with exponential backoff. In Phase A the bot path is the
    // only consumer — there is no Metis path to protect yet.
    let mut backoff = Duration::from_millis(500);
    loop {
        match upstream_client::run_stream(&cfg, &accounts, &metrics).await {
            Ok(()) => warn!("upstream stream ended cleanly, reconnecting"),
            Err(e) => warn!(error = %e, "upstream stream error, reconnecting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}
