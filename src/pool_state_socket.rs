/// Reads NDJSON account updates from the yellowstone_fanout Unix socket and
/// feeds them into PoolStateStore — so the bot gets live pool state without
/// making its own Yellowstone gRPC connection.
///
/// Protocol: one JSON line per account update, as written by fanout/bot_sink.rs:
///   {"slot":N,"pubkey":"BASE58","owner":"BASE58","lamports":N,"write_version":N,"data":"BASE64"}
///
/// Reconnects automatically with exponential backoff if the socket is not yet
/// available (fanout not started) or if the connection drops.
use anyhow::Result;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

use crate::pool_state_store::PoolStateStore;

#[derive(Deserialize)]
struct FanoutMsg {
    slot: u64,
    pubkey: String,
    owner: String,
    lamports: u64,
    write_version: u64,
    data: String, // base64-encoded
}

async fn run_once(path: &str, store: &Arc<PoolStateStore>) -> Result<()> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| anyhow::anyhow!("connect to fanout socket {path}: {e}"))?;
    info!(socket = path, "connected to fanout socket");

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();
    let mut updates = 0u64;

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let msg: FanoutMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "fanout: invalid JSON line, skipping");
                continue;
            }
        };

        let pubkey: Pubkey = match msg.pubkey.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let owner_bytes: [u8; 32] = match bs58::decode(&msg.owner).into_vec() {
            Ok(v) if v.len() == 32 => v.try_into().unwrap(),
            _ => [0u8; 32],
        };
        let owner = Pubkey::from(owner_bytes);

        let data: Vec<u8> = match base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &msg.data,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "fanout: invalid base64 data, skipping");
                continue;
            }
        };

        store.apply_update(pubkey, owner, msg.lamports, data, msg.slot, msg.write_version);
        updates += 1;

        if updates % 10_000 == 0 {
            info!(updates, slot = msg.slot, "fanout socket: {} updates applied", updates);
        }
    }

    Ok(())
}

/// Spawn the fanout socket reader task.
///
/// Connects to the Unix socket written by yellowstone_fanout_phase_a, reads
/// NDJSON lines, and applies each update to the PoolStateStore.
/// Reconnects with exponential backoff if the socket is unavailable.
pub fn spawn_socket_reader(path: String, store: Arc<PoolStateStore>) {
    info!(socket = %path, "pool_state_socket: starting fanout socket reader");
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            match run_once(&path, &store).await {
                Ok(()) => warn!(socket = %path, "fanout socket closed cleanly, reconnecting"),
                Err(e) => warn!(socket = %path, error = %e, "fanout socket error, reconnecting"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
}
