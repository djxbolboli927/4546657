use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestPing,
};

use crate::pool_state_store::PoolStateStore;

// ── mix.json loading ─────────────────────────────────────────────────────────

/// Account fields harvested from each pool entry in mix.json.
/// Per-DEX vaults/reserves are also collected when present so the store is
/// as complete as possible from the start. Tick/bin arrays are absent from
/// mix.json and are deferred to a later phase.
static EXTRA_PARAM_KEYS: &[&str] = &[
    "vault",
    "vaultA",
    "vaultB",
    "reserve",
    "reserveA",
    "reserveB",
    "oracle",
    "observation",
    "config",
    "market",
    "baseVault",
    "quoteVault",
    "eventQueue",
    "bids",
    "asks",
];

/// One pool's two token-vault pubkeys, extracted from mix.json.
/// Used by the price validator to read live reserves from PoolStateStore.
#[derive(Debug, Clone)]
pub struct PoolVaultPair {
    pub pool: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
}

/// All data produced by a single mix.json parse.
pub struct MixJsonResult {
    pub account_to_pools: HashMap<Pubkey, Vec<Pubkey>>,
    pub pool_to_accounts: HashMap<Pubkey, Vec<Pubkey>>,
    pub subscribe_list: Vec<String>,
    /// Pools that have both tokenAccountA and tokenAccountB in params.
    /// Used by the price validator.
    pub vault_pairs: Vec<PoolVaultPair>,
}

/// Parse mix.json and build two indexes:
///   `account_to_pools`  — account pubkey → pool pubkeys that depend on it
///   `pool_to_accounts`  — pool pubkey → all accounts for that pool
///
/// Also returns the deduplicated flat list of accounts to subscribe to, plus
/// the per-pool vault pairs used by the price validator.
pub fn load_mix_json(path: &str) -> Result<MixJsonResult> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading mix.json at {path}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).context("parsing mix.json")?;
    let pools = json
        .as_array()
        .context("mix.json top-level must be a JSON array")?;

    let mut account_to_pools: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
    let mut pool_to_accounts: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut subscribe_list: Vec<String> = Vec::new();
    let mut vault_pairs: Vec<PoolVaultPair> = Vec::new();

    for pool in pools {
        let pool_pk_str = match pool.get("pubkey").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let pool_pk: Pubkey = match pool_pk_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let mut pool_accounts: Vec<Pubkey> = Vec::new();

        // Always include the pool address itself.
        let mut candidates = vec![pool_pk_str.to_string()];

        // Extract vault pair for the price validator.
        let mut vault_a_str: Option<String> = None;
        let mut vault_b_str: Option<String> = None;

        if let Some(params) = pool.get("params") {
            // Mandatory token accounts.
            for key in &["tokenAccountA", "tokenAccountB"] {
                if let Some(s) = params.get(key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        candidates.push(s.to_string());
                        if *key == "tokenAccountA" {
                            vault_a_str = Some(s.to_string());
                        } else {
                            vault_b_str = Some(s.to_string());
                        }
                    }
                }
            }
            // Optional DEX-specific accounts.
            for key in EXTRA_PARAM_KEYS {
                if let Some(s) = params.get(*key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        candidates.push(s.to_string());
                    }
                }
            }
        }

        // Store vault pair if both vaults found.
        if let (Some(a_str), Some(b_str)) = (&vault_a_str, &vault_b_str) {
            if let (Ok(va), Ok(vb)) = (a_str.parse::<Pubkey>(), b_str.parse::<Pubkey>()) {
                vault_pairs.push(PoolVaultPair {
                    pool: pool_pk,
                    vault_a: va,
                    vault_b: vb,
                });
            }
        }

        for addr in candidates {
            if addr.is_empty() {
                continue;
            }
            let pk: Pubkey = match addr.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if seen.insert(addr.clone()) {
                subscribe_list.push(addr);
            }
            pool_accounts.push(pk);
            account_to_pools.entry(pk).or_default().push(pool_pk);
        }

        pool_to_accounts.entry(pool_pk).or_insert(pool_accounts);
    }

    Ok(MixJsonResult {
        account_to_pools,
        pool_to_accounts,
        subscribe_list,
        vault_pairs,
    })
}

// ── gRPC stream ──────────────────────────────────────────────────────────────

/// Connect once, subscribe, stream updates into the store.
/// Returns Ok(()) on clean end-of-stream; returns Err on any failure.
/// The caller wraps this in a reconnect loop.
async fn run_once(
    endpoint: &str,
    x_token: &str,
    subscribe_accounts: &[String],
    store: &Arc<PoolStateStore>,
) -> Result<()> {
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())?;
    if !x_token.is_empty() {
        builder = builder.x_token(Some(x_token.to_string()))?;
    }
    let mut client = builder
        .tls_config(yellowstone_grpc_client::ClientTlsConfig::new().with_native_roots())?
        .max_decoding_message_size(64 * 1024 * 1024)
        .connect()
        .await
        .context("pool_state_stream gRPC connect failed")?;

    let mut accounts_filter: HashMap<String, SubscribeRequestFilterAccounts> =
        HashMap::new();
    accounts_filter.insert(
        "pools".to_string(),
        SubscribeRequestFilterAccounts {
            account: subscribe_accounts.to_vec(),
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

    let (mut tx, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .context("pool_state_stream gRPC subscribe failed")?;

    info!(
        endpoint,
        accounts = subscribe_accounts.len(),
        pools = store.pool_count,
        "pool_state_stream subscription active"
    );

    while let Some(msg) = stream.next().await {
        let msg = msg.context("pool_state_stream stream error")?;
        match msg.update_oneof {
            Some(UpdateOneof::Account(a)) => {
                if let Some(info) = a.account {
                    let pk = match Pubkey::try_from(info.pubkey.as_slice()) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let owner_bytes: [u8; 32] = info
                        .owner
                        .as_slice()
                        .try_into()
                        .unwrap_or([0u8; 32]);
                    let owner = Pubkey::from(owner_bytes);

                    let relevant = store.apply_update(
                        pk,
                        owner,
                        info.lamports,
                        info.data,
                        a.slot,
                        info.write_version,
                    );
                    if relevant {
                        debug!(
                            slot = a.slot,
                            pubkey = %pk,
                            data_len = store.accounts.get(&pk).map(|u| u.data.len()).unwrap_or(0),
                            "pool account updated"
                        );
                    }
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                let _ = tx
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

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawn the pool-state subscription task.
///
/// Connects to the same Yellowstone endpoint the bot already uses, subscribes
/// to all accounts from mix.json, and keeps the PoolStateStore live.
/// Reconnects automatically with exponential backoff.
/// This task never exits — it runs for the lifetime of the process.
pub fn spawn_pool_state_stream(
    endpoint: String,
    x_token: String,
    subscribe_accounts: Vec<String>,
    store: Arc<PoolStateStore>,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            match run_once(&endpoint, &x_token, &subscribe_accounts, &store).await {
                Ok(()) => warn!("pool_state_stream ended cleanly, reconnecting"),
                Err(e) => warn!(error = %e, "pool_state_stream error, reconnecting"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
}
