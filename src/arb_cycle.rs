//! WSOL-based arbitrage cycle evaluator.
//!
//! Every pool has two directed edges (A→B and B→A).  A "cycle" starts and
//! ends at WSOL:
//!
//!   2-hop: WSOL ──pool1──► X ──pool2──► WSOL
//!   3-hop: WSOL ──pool1──► X ──pool2──► Y ──pool3──► WSOL
//!
//! `quote_edge` reads live vault balances from PoolStateStore and calls the
//! appropriate DEX calculator.  No RPC, no Metis, no float.
//!
//! Cost model (conservative, per Raydium AMM/CPMM validation results):
//!   tx_fee             = 5_000 lamports (one signature)
//!   jito_tip           = 1_000 lamports (minimum)
//!   calculator_margin  = 100 lamports per hop
//!   net_profit         = amount_out − amount_in − tx_fee − tip − n_hops × margin

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use tracing::warn;

use crate::dex::{raydium_amm_v4, raydium_cpmm};
use crate::pool_state_store::PoolStateStore;
use crate::pool_state_stream::PoolVaultPair;

// ── DEX kind ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DexKind {
    RaydiumAmmV4,
    RaydiumCpmm { trade_fee_rate: u64 },
}

impl DexKind {
    pub fn name(&self) -> &'static str {
        match self {
            DexKind::RaydiumAmmV4 => "RaydiumAmmV4",
            DexKind::RaydiumCpmm { .. } => "RaydiumCpmm",
        }
    }
}

// ── Edge ─────────────────────────────────────────────────────────────────────

/// One directed hop through a single pool.
#[derive(Debug, Clone)]
pub struct Edge {
    pub pool: Pubkey,
    pub dex_kind: DexKind,
    pub mint_in: Pubkey,
    pub mint_out: Pubkey,
    pub vault_in: Pubkey,
    pub vault_out: Pubkey,
}

// ── SPL token account parsing ─────────────────────────────────────────────────

/// Extract (mint, amount) from a raw SPL token account.
/// mint @ [0..32], amount @ [64..72].
fn read_spl_token_account(data: &[u8]) -> Option<(Pubkey, u64)> {
    if data.len() < 72 {
        return None;
    }
    let mint = Pubkey::from(<[u8; 32]>::try_from(&data[0..32]).ok()?);
    let amount = u64::from_le_bytes(data[64..72].try_into().ok()?);
    Some((mint, amount))
}

// ── Edge building ─────────────────────────────────────────────────────────────

/// Try to resolve the DexKind for a pool.  Reads AmmConfig from store if needed.
fn resolve_dex_kind(pair: &PoolVaultPair, store: &PoolStateStore) -> Option<DexKind> {
    if pair.owner == raydium_amm_v4::PROGRAM_ID {
        return Some(DexKind::RaydiumAmmV4);
    }
    if pair.owner == raydium_cpmm::PROGRAM_ID {
        // Try amm_config from mix.json first, then parse from pool data.
        let fee_rate = if let Some(cfg_pk) = pair.amm_config {
            store
                .accounts
                .get(&cfg_pk)
                .and_then(|r| raydium_cpmm::parse_trade_fee_rate(&r.data))
        } else {
            store
                .accounts
                .get(&pair.pool)
                .and_then(|r| raydium_cpmm::parse_amm_config(&r.data))
                .and_then(|cfg_pk| store.accounts.get(&cfg_pk))
                .and_then(|r| raydium_cpmm::parse_trade_fee_rate(&r.data))
        }?;
        return Some(DexKind::RaydiumCpmm { trade_fee_rate: fee_rate });
    }
    None // CLMM, DLMM, Whirlpool etc — skip
}

/// Build the full edge list from live vault pairs.
///
/// Each pair produces two edges (both directions). Edges whose vault data or
/// DEX kind can't be resolved are silently dropped — they will appear once the
/// store receives the relevant account updates.
pub fn build_edges(pairs: &[PoolVaultPair], store: &PoolStateStore) -> Vec<Edge> {
    let mut out = Vec::with_capacity(pairs.len() * 2);

    for pair in pairs {
        let Some(dex_kind) = resolve_dex_kind(pair, store) else {
            continue;
        };
        // Read mint from vault data to identify which token each vault holds.
        let mint_a = store
            .accounts
            .get(&pair.vault_a)
            .and_then(|r| read_spl_token_account(&r.data).map(|(m, _)| m));
        let mint_b = store
            .accounts
            .get(&pair.vault_b)
            .and_then(|r| read_spl_token_account(&r.data).map(|(m, _)| m));

        let (mint_a, mint_b) = match (mint_a, mint_b) {
            (Some(a), Some(b)) => (a, b),
            _ => continue, // vaults not yet live
        };

        // Edge A → B
        out.push(Edge {
            pool: pair.pool,
            dex_kind: dex_kind.clone(),
            mint_in: mint_a,
            mint_out: mint_b,
            vault_in: pair.vault_a,
            vault_out: pair.vault_b,
        });
        // Edge B → A
        out.push(Edge {
            pool: pair.pool,
            dex_kind,
            mint_in: mint_b,
            mint_out: mint_a,
            vault_in: pair.vault_b,
            vault_out: pair.vault_a,
        });
    }

    out
}

// ── Quote ─────────────────────────────────────────────────────────────────────

/// Compute exact-in amount_out for one edge from live store data.
/// Returns None if any required account is missing or reserves are zero.
pub fn quote_edge(edge: &Edge, amount_in: u64, store: &PoolStateStore) -> Option<u64> {
    let in_data = store.accounts.get(&edge.vault_in)?;
    let out_data = store.accounts.get(&edge.vault_out)?;
    let (_, reserve_in) = read_spl_token_account(&in_data.data)?;
    let (_, reserve_out) = read_spl_token_account(&out_data.data)?;
    if reserve_in == 0 || reserve_out == 0 {
        return None;
    }
    drop(in_data);
    drop(out_data);

    match &edge.dex_kind {
        DexKind::RaydiumAmmV4 => raydium_amm_v4::RaydiumAmmV4::amount_out(
            amount_in,
            reserve_in,
            reserve_out,
        )
        .map(|q| q.amount_out),
        DexKind::RaydiumCpmm { trade_fee_rate } => raydium_cpmm::RaydiumCpmm::amount_out(
            amount_in,
            reserve_in,
            reserve_out,
            *trade_fee_rate,
        )
        .map(|q| q.amount_out),
    }
}

// ── Cycle opportunity ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CycleHit {
    /// Pool pubkeys in hop order.
    pub pools: Vec<Pubkey>,
    /// DEX names in hop order.
    pub dex_names: Vec<&'static str>,
    /// Intermediate mint symbols (as base58; caller may resolve to ticker later).
    pub intermediate_mints: Vec<Pubkey>,
    pub amount_in: u64,
    pub amount_out: u64,
    /// amount_out − amount_in (always > 0 for a CycleHit).
    pub profit_gross: u64,
    /// Gross profit minus tx_cost and n_hops × error_margin (may be < 0).
    pub profit_net: i64,
}

impl CycleHit {
    pub fn hops(&self) -> usize {
        self.pools.len()
    }
    pub fn is_net_positive(&self) -> bool {
        self.profit_net > 0
    }
}

// ── Cycle search ─────────────────────────────────────────────────────────────

/// Find all gross-positive WSOL cycles (2-hop and 3-hop) at the given amounts.
///
/// `tx_cost`              = tx_fee + jito_tip in lamports (6_000 for staging).
/// `error_margin_per_hop` = calculator uncertainty per hop (100 L for Raydium AMM/CPMM).
///
/// Returns hits sorted by gross profit descending. The caller inspects
/// `profit_net` to decide whether to act.
pub fn find_cycles_in_store(
    edges: &[Edge],
    wsol_mint: Pubkey,
    amounts: &[u64],
    tx_cost: u64,
    error_margin_per_hop: u64,
    store: &Arc<PoolStateStore>,
) -> Vec<CycleHit> {
    let mut by_mint_in: HashMap<Pubkey, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        by_mint_in.entry(e.mint_in).or_default().push(i);
    }

    let wsol_out_edges = by_mint_in.get(&wsol_mint).cloned().unwrap_or_default();
    let empty: Vec<usize> = vec![];
    let mut hits: Vec<CycleHit> = Vec::new();

    // ── 2-hop: WSOL → X → WSOL ───────────────────────────────────────────
    for &i in &wsol_out_edges {
        let e1 = &edges[i];
        let x = e1.mint_out;
        if x == wsol_mint { continue; }

        let x_to_wsol = by_mint_in.get(&x).unwrap_or(&empty);
        for &j in x_to_wsol {
            let e2 = &edges[j];
            if e2.mint_out != wsol_mint { continue; }
            if e2.pool == e1.pool { continue; }

            for &amount_in in amounts {
                let Some(mid) = quote_edge(e1, amount_in, store) else { continue };
                if mid == 0 { continue; }
                let Some(out) = quote_edge(e2, mid, store) else { continue };
                if out <= amount_in { continue; } // no gross profit

                let gross = out - amount_in;
                let margin = error_margin_per_hop * 2;
                let net = (out as i64) - (amount_in as i64) - (tx_cost as i64) - (margin as i64);

                hits.push(CycleHit {
                    pools: vec![e1.pool, e2.pool],
                    dex_names: vec![e1.dex_kind.name(), e2.dex_kind.name()],
                    intermediate_mints: vec![x],
                    amount_in,
                    amount_out: out,
                    profit_gross: gross,
                    profit_net: net,
                });
            }
        }
    }

    // ── 3-hop: WSOL → X → Y → WSOL ───────────────────────────────────────
    // wsol→X edges × X→Y edges × Y→wsol edges.
    // Guard: skip if the edge list is huge (>300 edges → O(300^2) = 90k inner iters,
    // fine; full 3-hop adds one more multiplier so cap intermediate at 200).
    let x_count = wsol_out_edges.len().min(200);
    for &i in wsol_out_edges.iter().take(x_count) {
        let e1 = &edges[i];
        let x = e1.mint_out;
        if x == wsol_mint { continue; }

        // All edges starting at X (going to any Y ≠ WSOL).
        let x_edges = by_mint_in.get(&x).unwrap_or(&empty);
        for &j in x_edges {
            let e2 = &edges[j];
            let y = e2.mint_out;
            if y == wsol_mint { continue; } // that's a 2-hop, already covered
            if y == x { continue; } // degenerate
            if e2.pool == e1.pool { continue; }

            // Y → WSOL edges.
            let y_to_wsol = by_mint_in.get(&y).unwrap_or(&empty);
            for &k in y_to_wsol {
                let e3 = &edges[k];
                if e3.mint_out != wsol_mint { continue; }
                if e3.pool == e1.pool || e3.pool == e2.pool { continue; }

                for &amount_in in amounts {
                    let Some(mid1) = quote_edge(e1, amount_in, store) else { continue };
                    if mid1 == 0 { continue; }
                    let Some(mid2) = quote_edge(e2, mid1, store) else { continue };
                    if mid2 == 0 { continue; }
                    let Some(out) = quote_edge(e3, mid2, store) else { continue };
                    if out <= amount_in { continue; }

                    let gross = out - amount_in;
                    let margin = error_margin_per_hop * 3;
                    let net = (out as i64) - (amount_in as i64) - (tx_cost as i64) - (margin as i64);

                    hits.push(CycleHit {
                        pools: vec![e1.pool, e2.pool, e3.pool],
                        dex_names: vec![e1.dex_kind.name(), e2.dex_kind.name(), e3.dex_kind.name()],
                        intermediate_mints: vec![x, y],
                        amount_in,
                        amount_out: out,
                        profit_gross: gross,
                        profit_net: net,
                    });
                }
            }
        }
    }

    // Sort by gross profit descending.
    hits.sort_unstable_by(|a, b| b.profit_gross.cmp(&a.profit_gross));
    hits
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct CycleMetrics {
    pub scans_total: AtomicU64,
    pub cycles_evaluated: AtomicU64,
    pub gross_positive: AtomicU64,
    pub net_positive: AtomicU64,
    pub best_gross_lamports: AtomicU64,
    pub best_net_lamports: AtomicU64,
}

// ── Periodic scanner ──────────────────────────────────────────────────────────

/// Amounts to test (lamports of WSOL).
/// Covers 0.001 → 0.05 SOL with a few key points for a fast overview.
pub const SCAN_AMOUNTS: &[u64] = &[
    1_000_000,   // 0.001 SOL
    5_000_000,   // 0.005 SOL
    10_000_000,  // 0.010 SOL
    25_000_000,  // 0.025 SOL
    50_000_000,  // 0.050 SOL
];

/// Standard costs assumed for the Raydium AMM/CPMM staging env:
///   tx_fee     = 5_000 lamports
///   jito_tip   = 1_000 lamports (minimum)
pub const DEFAULT_TX_COST: u64 = 6_000;
/// Calculator error margin per hop (Raydium AMM/CPMM, p95 = 40 lamports).
/// Using 100 lamports as conservative safety buffer.
pub const ERROR_MARGIN_PER_HOP: u64 = 100;

/// Spawn the cycle scanner task. Runs every `interval_secs` seconds, logging
/// the best opportunities found without sending any transactions.
///
/// The CPU-bound edge build + cycle search runs inside `spawn_blocking`, so it
/// never blocks the async runtime: the gRPC stream ingestion and the price
/// validator keep running on the worker threads while the search executes on a
/// blocking-pool thread. On a 4-core box this keeps reserve data fresh.
pub fn spawn_cycle_scanner(
    pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    interval_secs: u64,
    max_log: usize,
    tx_cost: u64,
) {
    let metrics = Arc::new(CycleMetrics::default());
    let pairs = Arc::new(pairs);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // skip immediate first tick (store may be empty)

        loop {
            ticker.tick().await;

            // Heavy, CPU-bound work off the async runtime.
            let pairs_c = pairs.clone();
            let store_c = store.clone();
            let tx_cost_c = tx_cost;
            let result = tokio::task::spawn_blocking(move || {
                let edges = build_edges(&pairs_c, &store_c);
                let wsol =
                    solana_sdk::pubkey::Pubkey::from_str_const(crate::tokens::WSOL_MINT);
                let live_pools = store_c.live_pool_count();
                let edge_count = edges.len();
                if edge_count == 0 {
                    return (live_pools, 0usize, Vec::new());
                }
                let hits = find_cycles_in_store(
                    &edges,
                    wsol,
                    SCAN_AMOUNTS,
                    tx_cost_c,
                    ERROR_MARGIN_PER_HOP,
                    &store_c,
                );
                (live_pools, edge_count, hits)
            })
            .await;

            let (live_pools, edge_count, hits) = match result {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "[cycle] search task panicked");
                    continue;
                }
            };

            if edge_count == 0 {
                eprintln!("[cycle] no live edges yet (live_pools={live_pools})");
                continue;
            }

            let scans = metrics.scans_total.fetch_add(1, Ordering::Relaxed) + 1;
            metrics
                .cycles_evaluated
                .fetch_add(hits.len() as u64, Ordering::Relaxed);

            let gross_pos = hits.iter().filter(|h| h.profit_gross > 0).count();
            let net_pos = hits.iter().filter(|h| h.profit_net > 0).count();

            metrics
                .gross_positive
                .fetch_add(gross_pos as u64, Ordering::Relaxed);
            metrics
                .net_positive
                .fetch_add(net_pos as u64, Ordering::Relaxed);

            if let Some(best) = hits.first() {
                metrics
                    .best_gross_lamports
                    .fetch_max(best.profit_gross, Ordering::Relaxed);
                if best.profit_net > 0 {
                    metrics
                        .best_net_lamports
                        .fetch_max(best.profit_net as u64, Ordering::Relaxed);
                }
            }

            eprintln!(
                "[cycle] scan={scans} live_pools={live_pools} edges={edge_count} \
hits(gross+)={gross_pos} hits(net+)={net_pos} best_gross={} best_net={}",
                hits.first().map(|h| h.profit_gross).unwrap_or(0),
                hits.first().map(|h| h.profit_net).unwrap_or(0),
            );

            // Print top hits.
            for (rank, hit) in hits.iter().take(max_log).enumerate() {
                let path: Vec<String> = hit
                    .pools
                    .iter()
                    .zip(hit.dex_names.iter())
                    .map(|(p, d)| format!("{d}:{}", &p.to_string()[..8]))
                    .collect();
                let mints: Vec<String> = hit
                    .intermediate_mints
                    .iter()
                    .map(|m| m.to_string()[..8].to_string())
                    .collect();
                eprintln!(
                    "[cycle]   #{rank} {}-hop  in={:.4}SOL gross={:+}L net={:+}L  \
path=[{}]  via=[{}]",
                    hit.hops(),
                    hit.amount_in as f64 / 1e9,
                    hit.profit_gross as i64,
                    hit.profit_net,
                    path.join("→"),
                    if mints.is_empty() { "direct".to_string() } else { mints.join("→") },
                );
            }

            // Summary stats.
            let total_gross = metrics.gross_positive.load(Ordering::Relaxed);
            let total_net = metrics.net_positive.load(Ordering::Relaxed);
            let best_gross_ever = metrics.best_gross_lamports.load(Ordering::Relaxed);
            let best_net_ever = metrics.best_net_lamports.load(Ordering::Relaxed);
            eprintln!(
                "[cycle] cumulative: scans={scans} gross+={total_gross} net+={total_net} \
best_gross_ever={best_gross_ever}L best_net_ever={best_net_ever}L"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_state_store::PoolStateStore;

    /// Build a 165-byte SPL token account: mint @ [0..32], amount @ [64..72].
    fn spl_account(mint: &Pubkey, amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[0..32].copy_from_slice(mint.as_ref());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data
    }

    fn insert_vault(store: &PoolStateStore, vault: Pubkey, mint: &Pubkey, amount: u64) {
        store.apply_update(
            vault,
            raydium_amm_v4::PROGRAM_ID, // owner irrelevant for vaults
            0,
            spl_account(mint, amount),
            1,
            1,
        );
    }

    /// Two WSOL/X Raydium AMM v4 pools at different prices must yield a
    /// gross-positive 2-hop WSOL→X→WSOL cycle. Proves the search works, so an
    /// empty result in production is a genuine market condition, not a bug.
    #[test]
    fn finds_two_hop_arbitrage() {
        let wsol = Pubkey::from_str_const(crate::tokens::WSOL_MINT);
        let x = Pubkey::new_unique();

        let pool1 = Pubkey::new_unique();
        let p1_wsol_vault = Pubkey::new_unique();
        let p1_x_vault = Pubkey::new_unique();

        let pool2 = Pubkey::new_unique();
        let p2_wsol_vault = Pubkey::new_unique();
        let p2_x_vault = Pubkey::new_unique();

        // Build the store indexes.
        let mut a2p: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        let mut p2a: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        for (pool, wv, xv) in [
            (pool1, p1_wsol_vault, p1_x_vault),
            (pool2, p2_wsol_vault, p2_x_vault),
        ] {
            p2a.insert(pool, vec![pool, wv, xv]);
            for acc in [pool, wv, xv] {
                a2p.entry(acc).or_default().push(pool);
            }
        }
        let store = PoolStateStore::new(a2p, p2a);

        // Pool1: 1000 WSOL : 1000 X  (price 1:1)
        insert_vault(&store, p1_wsol_vault, &wsol, 1_000_000_000_000);
        insert_vault(&store, p1_x_vault, &x, 1_000_000_000_000);
        // Pool2: 1000 WSOL : 900 X  (X scarcer → worth more WSOL)
        insert_vault(&store, p2_wsol_vault, &wsol, 1_000_000_000_000);
        insert_vault(&store, p2_x_vault, &x, 900_000_000_000);

        let pairs = vec![
            PoolVaultPair {
                pool: pool1,
                vault_a: p1_wsol_vault,
                vault_b: p1_x_vault,
                owner: raydium_amm_v4::PROGRAM_ID,
                amm_config: None,
            },
            PoolVaultPair {
                pool: pool2,
                vault_a: p2_wsol_vault,
                vault_b: p2_x_vault,
                owner: raydium_amm_v4::PROGRAM_ID,
                amm_config: None,
            },
        ];

        let edges = build_edges(&pairs, &store);
        assert_eq!(edges.len(), 4, "two pools → four directed edges");

        let hits = find_cycles_in_store(&edges, wsol, &[1_000_000], 6_000, 100, &store);
        assert!(!hits.is_empty(), "an ~11% price gap must produce a cycle");
        let best = &hits[0];
        assert_eq!(best.hops(), 2);
        assert!(best.profit_gross > 0);
        assert!(best.is_net_positive(), "11% gap easily covers costs");
    }

    /// Two pools at the SAME price must NOT produce a profitable cycle
    /// (fees make the round-trip a loss). Confirms we don't emit false hits.
    #[test]
    fn equal_price_pools_no_arbitrage() {
        let wsol = Pubkey::from_str_const(crate::tokens::WSOL_MINT);
        let x = Pubkey::new_unique();
        let pool1 = Pubkey::new_unique();
        let p1w = Pubkey::new_unique();
        let p1x = Pubkey::new_unique();
        let pool2 = Pubkey::new_unique();
        let p2w = Pubkey::new_unique();
        let p2x = Pubkey::new_unique();

        let mut a2p: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        let mut p2a: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        for (pool, wv, xv) in [(pool1, p1w, p1x), (pool2, p2w, p2x)] {
            p2a.insert(pool, vec![pool, wv, xv]);
            for acc in [pool, wv, xv] {
                a2p.entry(acc).or_default().push(pool);
            }
        }
        let store = PoolStateStore::new(a2p, p2a);
        for (wv, xv) in [(p1w, p1x), (p2w, p2x)] {
            insert_vault(&store, wv, &wsol, 1_000_000_000_000);
            insert_vault(&store, xv, &x, 1_000_000_000_000);
        }

        let pairs = vec![
            PoolVaultPair { pool: pool1, vault_a: p1w, vault_b: p1x, owner: raydium_amm_v4::PROGRAM_ID, amm_config: None },
            PoolVaultPair { pool: pool2, vault_a: p2w, vault_b: p2x, owner: raydium_amm_v4::PROGRAM_ID, amm_config: None },
        ];
        let edges = build_edges(&pairs, &store);
        let hits = find_cycles_in_store(&edges, wsol, &[1_000_000], 6_000, 100, &store);
        assert!(hits.is_empty(), "equal prices → fees make every cycle a loss");
    }
}
