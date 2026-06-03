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

/// Recursively collect every valid Solana pubkey found as a JSON string value
/// inside `value`. Numbers (e.g. routingGroup) and non-pubkey strings are
/// ignored. This makes the parser DEX-agnostic: whatever extra accounts a
/// given DEX puts in `params`, we capture them without a hard-coded key list.
fn collect_pubkeys_from_value(value: &serde_json::Value, out: &mut Vec<Pubkey>) {
    match value {
        serde_json::Value::String(s) => {
            if let Ok(pk) = s.parse::<Pubkey>() {
                out.push(pk);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_pubkeys_from_value(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map {
                collect_pubkeys_from_value(v, out);
            }
        }
        _ => {}
    }
}

/// One pool's two token-vault pubkeys, extracted from mix.json.
/// Used by the price validator to read live reserves from PoolStateStore.
#[derive(Debug, Clone)]
pub struct PoolVaultPair {
    pub pool: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    /// The pool account's owner program id (from mix.json "owner"), used to
    /// classify which DEX engine handles this pool. May be default/zero when
    /// mix.json omits it (then the validator falls back to the live account's
    /// owner from the store).
    pub owner: Pubkey,
    /// Optional CPMM AmmConfig account (from params "config"/"ammConfig"),
    /// needed to read the per-pool trade_fee_rate.
    pub amm_config: Option<Pubkey>,
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

        // Collect every account pubkey for this pool. Start with the pool
        // address itself, then recursively harvest all valid pubkeys from
        // `params` (DEX-agnostic — captures vaults, oracles, tick/bin arrays,
        // configs, ALTs, mints, and anything else a DEX puts there).
        let mut candidates: Vec<Pubkey> = vec![pool_pk];

        // Pool owner program id (classifies the DEX for the validator).
        let owner: Pubkey = pool
            .get("owner")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();

        // Extract vault pair + AmmConfig for the price validator (named keys).
        let mut vault_a: Option<Pubkey> = None;
        let mut vault_b: Option<Pubkey> = None;
        let mut amm_config: Option<Pubkey> = None;

        if let Some(params) = pool.get("params") {
            // CPMM AmmConfig account (per-pool trade_fee_rate lives here).
            for key in &["config", "ammConfig", "amm_config"] {
                if let Some(s) = params.get(*key).and_then(|v| v.as_str()) {
                    if let Ok(pk) = s.parse::<Pubkey>() {
                        amm_config = Some(pk);
                        break;
                    }
                }
            }
            // Named token accounts → vault pair for the validator.
            vault_a = params
                .get("tokenAccountA")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok());
            vault_b = params
                .get("tokenAccountB")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok());

            // Recursively harvest every other pubkey in params for the stream.
            collect_pubkeys_from_value(params, &mut candidates);
        }

        // Store vault pair if both vaults found.
        if let (Some(va), Some(vb)) = (vault_a, vault_b) {
            vault_pairs.push(PoolVaultPair {
                pool: pool_pk,
                vault_a: va,
                vault_b: vb,
                owner,
                amm_config,
            });
        }

        for pk in candidates {
            let addr = pk.to_string();
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

/// Spawn one subscription stream for a single chunk of accounts.
fn spawn_one_stream(
    shard: usize,
    endpoint: String,
    x_token: String,
    subscribe_accounts: Vec<String>,
    store: Arc<PoolStateStore>,
) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            match run_once(&endpoint, &x_token, &subscribe_accounts, &store).await {
                Ok(()) => warn!(shard, "pool_state_stream ended cleanly, reconnecting"),
                Err(e) => warn!(shard, error = %e, "pool_state_stream error, reconnecting"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
}

/// Spawn a stats reporter that logs store health every 5 seconds.
/// No per-update logging — this is the only periodic visibility into the stream.
fn spawn_stats_reporter(store: Arc<PoolStateStore>, total_accounts: usize) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            let live_accounts = store.account_count();
            let live_pools = store.live_pool_count();
            eprintln!(
                "[pool_state] stats live_accounts={live_accounts}/{total_accounts} \
live_pools={live_pools}/{}",
                store.pool_count
            );
        }
    });
}

/// Spawn the pool-state subscription task(s) — DirectGrpcFast.
///
/// Subscribes directly to Yellowstone gRPC with an exact-account filter built
/// from mix.json, and keeps the PoolStateStore live. If `accounts_per_stream`
/// is non-zero and the list is larger, the subscription is sharded across
/// multiple streams, all writing to the same store. A stats reporter logs
/// live_accounts / live_pools every 5 seconds. Tasks never exit — they
/// reconnect with exponential backoff for the lifetime of the process.
pub fn spawn_pool_state_stream(
    endpoint: String,
    x_token: String,
    subscribe_accounts: Vec<String>,
    store: Arc<PoolStateStore>,
) {
    spawn_pool_state_stream_sharded(endpoint, x_token, subscribe_accounts, store, 0);
}

/// Like `spawn_pool_state_stream` but with an explicit shard size.
/// `accounts_per_stream == 0` ⇒ a single stream (no sharding).
pub fn spawn_pool_state_stream_sharded(
    endpoint: String,
    x_token: String,
    subscribe_accounts: Vec<String>,
    store: Arc<PoolStateStore>,
    accounts_per_stream: usize,
) {
    let total = subscribe_accounts.len();
    spawn_stats_reporter(store.clone(), total);

    if accounts_per_stream == 0 || total <= accounts_per_stream {
        eprintln!(
            "[pool_state] direct gRPC: 1 stream, {total} accounts, commitment=processed"
        );
        spawn_one_stream(0, endpoint, x_token, subscribe_accounts, store);
        return;
    }

    let chunks: Vec<Vec<String>> = subscribe_accounts
        .chunks(accounts_per_stream)
        .map(|c| c.to_vec())
        .collect();
    eprintln!(
        "[pool_state] direct gRPC: {} streams × ≤{accounts_per_stream} accounts \
({total} total), commitment=processed",
        chunks.len()
    );
    for (shard, chunk) in chunks.into_iter().enumerate() {
        spawn_one_stream(shard, endpoint.clone(), x_token.clone(), chunk, store.clone());
    }
}
