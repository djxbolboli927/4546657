/// Fanout sinks: stdout for human inspection, Unix socket for the bot.
///
/// Socket protocol: newline-delimited JSON (one line per account update).
/// Each line:
///   {"slot":N,"pubkey":"BASE58","owner":"BASE58","lamports":N,"write_version":N,"data":"BASE64"}
///
/// The bot connects, reads lines, parses each as FanoutMsg, and calls
/// PoolStateStore::apply_update — no Yellowstone gRPC connection needed.
use crate::config::{Config, OutputMode};

use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Shared broadcast sender. Every account update encoded as a JSON+newline
/// is sent to all connected socket clients.
pub type BroadcastTx = broadcast::Sender<Arc<str>>;

/// Create the broadcast channel and, if socket_path is non-empty, start the
/// Unix socket server task.  Returns the sender for use in the upstream client.
pub fn start(cfg: &Config) -> BroadcastTx {
    // Channel capacity: if a client is slow and falls > 4096 messages behind,
    // it is disconnected (lagged error). This prevents the hot path from blocking.
    let (tx, _rx) = broadcast::channel::<Arc<str>>(4096);

    if !cfg.socket_path.is_empty() {
        let path = cfg.socket_path.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            run_socket_server(path, tx2).await;
        });
    }

    tx
}

/// Encode one account update to the JSON line format and broadcast it.
/// Also prints to stdout when output_mode = Stdout.
#[allow(clippy::too_many_arguments)]
pub fn emit_account(
    cfg: &Config,
    tx: &BroadcastTx,
    slot: u64,
    pubkey: &str,
    owner: &str,
    lamports: u64,
    data: &[u8],
    write_version: u64,
    is_startup: bool,
) {
    // Build the JSON line.  We always encode data as base64 so the binary
    // payload survives NDJSON framing without escaping issues.
    let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);

    let line: Arc<str> = if cfg.dump_json {
        Arc::from(format!(
            "{{\"slot\":{slot},\"pubkey\":\"{pubkey}\",\"owner\":\"{owner}\",\
\"lamports\":{lamports},\"write_version\":{write_version},\
\"is_startup\":{is_startup},\"data_len\":{},\"data\":\"{data_b64}\"}}\n",
            data.len()
        ))
    } else {
        Arc::from(format!(
            "{{\"slot\":{slot},\"pubkey\":\"{pubkey}\",\"owner\":\"{owner}\",\
\"lamports\":{lamports},\"write_version\":{write_version},\"data\":\"{data_b64}\"}}\n"
        ))
    };

    // Stdout sink (for eyeballing / pipe to file).
    if cfg.output_mode == OutputMode::Stdout {
        // Print compact summary, not the heavy JSON, unless --dump-json.
        if cfg.dump_json {
            print!("{line}");
        } else {
            println!(
                "slot={slot} pubkey={pubkey} owner={owner} lamports={lamports} \
data_len={} write_version={write_version} startup={is_startup}",
                data.len()
            );
        }
    }

    // Socket sink (for the bot).  send() returns Err only when there are no
    // receivers — ignore that case silently.
    let _ = tx.send(line);
}

/// Accept connections on the Unix socket and broadcast account updates to
/// every connected client as newline-delimited JSON.
async fn run_socket_server(path: String, tx: BroadcastTx) {
    // Remove a stale socket file if it exists.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => {
            info!(socket = %path, "fanout Unix socket listening");
            l
        }
        Err(e) => {
            warn!(socket = %path, error = %e, "failed to bind fanout socket");
            return;
        }
    };

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "socket accept error");
                continue;
            }
        };
        info!("fanout: bot connected via socket");

        let mut rx = tx.subscribe();
        tokio::spawn(async move {
            let mut stream = stream;
            loop {
                match rx.recv().await {
                    Ok(line) => {
                        if stream.write_all(line.as_bytes()).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(dropped = n, "fanout socket client lagged, dropped messages");
                        // Continue — don't disconnect on lag; just note it.
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            info!("fanout: bot socket client disconnected");
        });
    }
}
