use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::info;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestPing,
};

use crate::bot_sink::{self, BroadcastTx};
use crate::config::Config;
use crate::metrics::Metrics;

pub async fn run_stream(
    cfg: &Config,
    accounts: &[String],
    metrics: &Arc<Metrics>,
    tx: &BroadcastTx,
) -> Result<()> {
    let mut builder = GeyserGrpcClient::build_from_shared(cfg.upstream_endpoint.clone())?;
    if !cfg.x_token.is_empty() {
        builder = builder.x_token(Some(cfg.x_token.clone()))?;
    }
    let mut client = builder
        .tls_config(yellowstone_grpc_client::ClientTlsConfig::new().with_native_roots())?
        .max_decoding_message_size(64 * 1024 * 1024)
        .connect()
        .await
        .context("gRPC connect failed")?;

    info!(endpoint = %cfg.upstream_endpoint, "gRPC connected");

    let mut accounts_filter: HashMap<String, SubscribeRequestFilterAccounts> = HashMap::new();
    accounts_filter.insert(
        "bot".to_string(),
        SubscribeRequestFilterAccounts {
            account: accounts.to_vec(),
            owner: vec![],
            filters: vec![],
            nonempty_txn_signature: None,
        },
    );

    let request = SubscribeRequest {
        slots: HashMap::new(),
        accounts: accounts_filter,
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        entry: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot: None,
    };

    let (mut ping_tx, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .context("gRPC subscribe failed")?;

    info!(accounts = accounts.len(), "subscription active; waiting for account updates");

    while let Some(msg) = stream.next().await {
        let msg = msg.context("stream yielded error")?;
        match msg.update_oneof {
            Some(UpdateOneof::Account(a)) => {
                metrics.from_upstream.fetch_add(1, Ordering::Relaxed);
                metrics.last_slot.store(a.slot, Ordering::Relaxed);

                if let Some(info) = a.account {
                    metrics.to_bot.fetch_add(1, Ordering::Relaxed);
                    let pubkey = bs58::encode(&info.pubkey).into_string();
                    let owner = bs58::encode(&info.owner).into_string();
                    bot_sink::emit_account(
                        cfg,
                        tx,
                        a.slot,
                        &pubkey,
                        &owner,
                        info.lamports,
                        &info.data,
                        info.write_version,
                        a.is_startup,
                    );
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                let _ = ping_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    })
                    .await;
            }
            _ => {}
        }
    }

    Ok(())
}
