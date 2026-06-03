use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Raw account state as delivered by Yellowstone, stored per pubkey.
/// The bot uses this to compute pool prices (slippage) locally without
/// asking Metis for every quote — Phase 2 of the arb pipeline.
#[derive(Clone, Debug)]
pub struct RawAccountUpdate {
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
    pub slot: u64,
    pub write_version: u64,
    pub updated_at_unix_ns: u64,
}

/// In-memory store of live pool account states.
///
/// Indexed two ways:
///   `accounts`       — pubkey → latest raw account state
///   `account_to_pools` — account pubkey → pool pubkeys that depend on it
///   `pool_to_accounts` — pool pubkey → all account pubkeys for that pool
///
/// Both indexes are built once at startup from mix.json and never change.
/// Only `accounts` is written on every Yellowstone update.
#[derive(Clone)]
pub struct PoolStateStore {
    /// Live account states — updated on every Yellowstone account update.
    pub accounts: Arc<DashMap<Pubkey, RawAccountUpdate>>,
    /// Which pools depend on a given account (built from mix.json at startup).
    pub account_to_pools: Arc<HashMap<Pubkey, Vec<Pubkey>>>,
    /// Which accounts belong to each pool (built from mix.json at startup).
    pub pool_to_accounts: Arc<HashMap<Pubkey, Vec<Pubkey>>>,
    /// Count of pool-account mappings loaded from mix.json.
    pub pool_count: usize,
}

impl PoolStateStore {
    pub fn new(
        account_to_pools: HashMap<Pubkey, Vec<Pubkey>>,
        pool_to_accounts: HashMap<Pubkey, Vec<Pubkey>>,
    ) -> Arc<Self> {
        let pool_count = pool_to_accounts.len();
        Arc::new(Self {
            accounts: Arc::new(DashMap::new()),
            account_to_pools: Arc::new(account_to_pools),
            pool_to_accounts: Arc::new(pool_to_accounts),
            pool_count,
        })
    }

    /// Apply one Yellowstone account update. Returns true if this account is
    /// relevant (present in the pool index), false if it is unknown.
    pub fn apply_update(
        &self,
        pubkey: Pubkey,
        owner: Pubkey,
        lamports: u64,
        data: Vec<u8>,
        slot: u64,
        write_version: u64,
    ) -> bool {
        let relevant = self.account_to_pools.contains_key(&pubkey);
        let updated_at_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        self.accounts.insert(
            pubkey,
            RawAccountUpdate {
                pubkey,
                owner,
                lamports,
                data,
                slot,
                write_version,
                updated_at_unix_ns,
            },
        );
        relevant
    }

    /// How many accounts are currently live in the store.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// A pool is "live" once its pool-state account and both token vaults have
    /// been received. We approximate this as: at least 3 of the pool's mapped
    /// accounts are present in the store (pool + vaultA + vaultB). Pools that
    /// map fewer than 3 accounts are live once all of them are present.
    pub fn is_pool_live(&self, pool: &Pubkey) -> bool {
        let Some(accts) = self.pool_to_accounts.get(pool) else {
            return false;
        };
        let need = accts.len().min(3);
        let have = accts
            .iter()
            .filter(|pk| self.accounts.contains_key(*pk))
            .take(need)
            .count();
        have >= need
    }

    /// Count of pools that currently satisfy `is_pool_live`.
    pub fn live_pool_count(&self) -> usize {
        self.pool_to_accounts
            .keys()
            .filter(|p| self.is_pool_live(p))
            .count()
    }
}
