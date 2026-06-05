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

use crate::dex::{
    meteora_damm_v2, meteora_dlmm, pumpswap, raydium_amm_v4, raydium_clmm, raydium_cpmm, whirlpool,
};
use crate::pool_state_store::PoolStateStore;
use crate::pool_state_stream::PoolVaultPair;

// ── DEX kind ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum DexKind {
    RaydiumAmmV4,
    RaydiumCpmm {
        trade_fee_rate: u64,
    },
    /// Meteora DAMM v2 (cp-amm): Uniswap-v3-style, priced from a pool-account
    /// snapshot (sqrt_price + liquidity), NOT from vault reserves. The snapshot
    /// is taken at `build_edges` time so each per-amount `quote_edge` call is a
    /// pure function. `a_for_b` is the swap direction for *this* edge.
    MeteoraDammV2 {
        sqrt_price: u128,
        liquidity: u128,
        sqrt_min_price: u128,
        sqrt_max_price: u128,
        fee_numerator: u64,
        collect_fee_mode: u8,
        a_for_b: bool,
    },
    /// Orca Whirlpool: Uniswap-v3-style CLMM.  Single-tick quote from a
    /// pool-account snapshot.  Returns None when the swap would cross the
    /// current-tick's boundary (tick-array state not available).
    OrcaWhirlpoolV1 {
        sqrt_price: u128,
        liquidity: u128,
        /// sqrt_price of the lower tick boundary (Q64.64).
        sqrt_price_lower: u128,
        /// sqrt_price of the upper tick boundary (Q64.64).
        sqrt_price_upper: u128,
        fee_rate: u16,
        a_for_b: bool,
    },
    /// Raydium CLMM: Uniswap-v3-style CLMM.  Multi-tick quote when TickArray
    /// accounts are in the store; single-tick fallback otherwise.
    /// `zero_for_one` = token0 → token1 direction.
    RaydiumClmm {
        sqrt_price: u128,
        liquidity: u128,
        sqrt_price_lower: u128,
        sqrt_price_upper: u128,
        trade_fee_rate: u32,
        zero_for_one: bool,
        /// Current tick index — needed for multi-tick boundary calculation.
        tick_current: i32,
        /// Tick spacing for this pool tier.
        tick_spacing: u16,
    },
    /// PumpSwap: pump.fun's constant-product AMM (x*y=k, 30 bps total fee).
    /// Priced from live vault reserves like Raydium AMM v4.
    PumpSwap,
    /// Meteora DLMM: bin-based Liquidity Book. Priced from BinArray accounts
    /// (constant-sum per bin), looked up by deriving their PDAs from the live
    /// store at quote time. `swap_for_y` = token X in / token Y out.
    MeteoraDlmm {
        pool: Box<meteora_dlmm::LbPair>,
        swap_for_y: bool,
    },
}

impl DexKind {
    pub fn name(&self) -> &'static str {
        match self {
            DexKind::RaydiumAmmV4 => "RaydiumAmmV4",
            DexKind::RaydiumCpmm { .. } => "RaydiumCpmm",
            DexKind::MeteoraDammV2 { .. } => "MeteoraDammV2",
            DexKind::OrcaWhirlpoolV1 { .. } => "OrcaWhirlpoolV1",
            DexKind::RaydiumClmm { .. } => "RaydiumClmm",
            DexKind::PumpSwap => "PumpSwap",
            DexKind::MeteoraDlmm { .. } => "MeteoraDlmm",
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

/// Build the two directed edges for a Meteora DAMM v2 pool from its pool
/// account. Skips silently if the pool account isn't live yet, can't be parsed,
/// or is an unsupported (compounding / empty) pool — in which case it simply
/// won't contribute edges this scan.
fn push_meteora_edges(pair: &PoolVaultPair, store: &PoolStateStore, out: &mut Vec<Edge>) {
    let Some(acc) = store.accounts.get(&pair.pool) else {
        return; // pool account not streamed yet
    };
    let Some(pool) = meteora_damm_v2::parse_pool(&acc.data) else {
        return;
    };
    if !pool.is_supported() {
        return; // compounding curve or empty liquidity — never priced here
    }
    let fee_numerator = pool.total_fee_numerator();

    let make = |a_for_b: bool| DexKind::MeteoraDammV2 {
        sqrt_price: pool.sqrt_price,
        liquidity: pool.liquidity,
        sqrt_min_price: pool.sqrt_min_price,
        sqrt_max_price: pool.sqrt_max_price,
        fee_numerator,
        collect_fee_mode: pool.collect_fee_mode,
        a_for_b,
    };

    // Edge A → B (a_for_b = true).
    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(true),
        mint_in: pool.token_a_mint,
        mint_out: pool.token_b_mint,
        vault_in: pool.token_a_vault,
        vault_out: pool.token_b_vault,
    });
    // Edge B → A (a_for_b = false).
    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(false),
        mint_in: pool.token_b_mint,
        mint_out: pool.token_a_mint,
        vault_in: pool.token_b_vault,
        vault_out: pool.token_a_vault,
    });
}

/// Build two directed edges for an Orca Whirlpool pool from its pool account.
/// Silently skips if the pool account isn't live, can't be parsed, or has zero
/// liquidity — in all cases it simply won't contribute edges this scan.
fn push_whirlpool_edges(pair: &PoolVaultPair, store: &PoolStateStore, out: &mut Vec<Edge>) {
    let Some(acc) = store.accounts.get(&pair.pool) else {
        return;
    };
    let Some(pool) = whirlpool::parse_pool(&acc.data) else {
        return;
    };

    let make = |a_for_b: bool| DexKind::OrcaWhirlpoolV1 {
        sqrt_price: pool.sqrt_price,
        liquidity: pool.liquidity,
        sqrt_price_lower: pool.sqrt_price_lower,
        sqrt_price_upper: pool.sqrt_price_upper,
        fee_rate: pool.fee_rate,
        a_for_b,
    };

    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(true),
        mint_in: pool.token_mint_a,
        mint_out: pool.token_mint_b,
        vault_in: pool.token_vault_a,
        vault_out: pool.token_vault_b,
    });
    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(false),
        mint_in: pool.token_mint_b,
        mint_out: pool.token_mint_a,
        vault_in: pool.token_vault_b,
        vault_out: pool.token_vault_a,
    });
}

/// Build two directed edges for a Raydium CLMM pool from its pool account.
/// Silently skips if the pool is missing, has zero liquidity, or tick boundaries
/// can't be computed.  Fee is read from the AmmConfig account; falls back to
/// tick-spacing inference when the AmmConfig isn't in the store.
fn push_raydium_clmm_edges(pair: &PoolVaultPair, store: &PoolStateStore, out: &mut Vec<Edge>) {
    let Some(acc) = store.accounts.get(&pair.pool) else {
        return;
    };
    let Some(pool) = raydium_clmm::parse_pool(&acc.data) else {
        return;
    };

    // Prefer pair.amm_config hint (from mix.json), then the pubkey embedded in
    // the pool account, then fall back to tick-spacing tier inference.
    let trade_fee_rate = if let Some(cfg_pk) = pair.amm_config {
        store
            .accounts
            .get(&cfg_pk)
            .and_then(|r| raydium_clmm::parse_trade_fee_rate(&r.data))
    } else {
        store
            .accounts
            .get(&pool.amm_config)
            .and_then(|r| raydium_clmm::parse_trade_fee_rate(&r.data))
    }
    .unwrap_or_else(|| raydium_clmm::fee_rate_from_tick_spacing(pool.tick_spacing));

    let make = |zero_for_one: bool| DexKind::RaydiumClmm {
        sqrt_price: pool.sqrt_price_x64,
        liquidity: pool.liquidity,
        sqrt_price_lower: pool.sqrt_price_lower,
        sqrt_price_upper: pool.sqrt_price_upper,
        trade_fee_rate,
        zero_for_one,
        tick_current: pool.tick_current,
        tick_spacing: pool.tick_spacing,
    };

    // Token0 → Token1 (zero_for_one = true)
    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(true),
        mint_in: pool.token_mint_0,
        mint_out: pool.token_mint_1,
        vault_in: pool.token_vault_0,
        vault_out: pool.token_vault_1,
    });
    // Token1 → Token0 (zero_for_one = false)
    out.push(Edge {
        pool: pair.pool,
        dex_kind: make(false),
        mint_in: pool.token_mint_1,
        mint_out: pool.token_mint_0,
        vault_in: pool.token_vault_1,
        vault_out: pool.token_vault_0,
    });
}

/// Build two directed edges for a PumpSwap pool from its pool account.
/// Vault addresses come from the pool account itself (not pair.vault_a/b).
/// Silently skips if the pool account is missing or too short to parse.
fn push_pumpswap_edges(pair: &PoolVaultPair, store: &PoolStateStore, out: &mut Vec<Edge>) {
    let Some(acc) = store.accounts.get(&pair.pool) else {
        return;
    };
    let Some(pool) = pumpswap::parse_pool(&acc.data) else {
        return;
    };

    // Base → Quote
    out.push(Edge {
        pool: pair.pool,
        dex_kind: DexKind::PumpSwap,
        mint_in: pool.base_mint,
        mint_out: pool.quote_mint,
        vault_in: pool.base_vault,
        vault_out: pool.quote_vault,
    });
    // Quote → Base
    out.push(Edge {
        pool: pair.pool,
        dex_kind: DexKind::PumpSwap,
        mint_in: pool.quote_mint,
        mint_out: pool.base_mint,
        vault_in: pool.quote_vault,
        vault_out: pool.base_vault,
    });
}

/// Build two directed edges for a Meteora DLMM pool from its LbPair account.
/// The per-bin reserves live in separate BinArray accounts read at quote time;
/// here we only snapshot the pool parameters. Silently skips if the pool
/// account is missing, too short, or the pool is disabled.
fn push_dlmm_edges(pair: &PoolVaultPair, store: &PoolStateStore, out: &mut Vec<Edge>) {
    let Some(acc) = store.accounts.get(&pair.pool) else {
        return;
    };
    let Some(pool) = meteora_dlmm::parse_pool(&acc.data) else {
        return;
    };
    if !pool.is_supported() {
        return;
    }

    // X → Y (swap_for_y = true): active_id decreases.
    out.push(Edge {
        pool: pair.pool,
        dex_kind: DexKind::MeteoraDlmm {
            pool: Box::new(pool.clone()),
            swap_for_y: true,
        },
        mint_in: pool.token_x_mint,
        mint_out: pool.token_y_mint,
        vault_in: pool.reserve_x,
        vault_out: pool.reserve_y,
    });
    // Y → X (swap_for_y = false): active_id increases.
    out.push(Edge {
        pool: pair.pool,
        dex_kind: DexKind::MeteoraDlmm {
            pool: Box::new(pool.clone()),
            swap_for_y: false,
        },
        mint_in: pool.token_y_mint,
        mint_out: pool.token_x_mint,
        vault_in: pool.reserve_y,
        vault_out: pool.reserve_x,
    });
}

/// Build the full edge list from live vault pairs.
///
/// Each pair produces two edges (both directions). Edges whose vault data or
/// DEX kind can't be resolved are silently dropped — they will appear once the
/// store receives the relevant account updates.
pub fn build_edges(pairs: &[PoolVaultPair], store: &PoolStateStore) -> Vec<Edge> {
    let mut out = Vec::with_capacity(pairs.len() * 2);

    for pair in pairs {
        // CLMM pools (Meteora DAMM v2, Orca Whirlpool) price from the pool
        // account, not vault reserves — they need their own edge-building paths.
        if pair.owner == meteora_damm_v2::PROGRAM_ID {
            push_meteora_edges(pair, store, &mut out);
            continue;
        }
        if pair.owner == whirlpool::PROGRAM_ID {
            push_whirlpool_edges(pair, store, &mut out);
            continue;
        }
        if pair.owner == raydium_clmm::PROGRAM_ID {
            push_raydium_clmm_edges(pair, store, &mut out);
            continue;
        }
        if pair.owner == pumpswap::PROGRAM_ID {
            push_pumpswap_edges(pair, store, &mut out);
            continue;
        }
        if pair.owner == meteora_dlmm::PROGRAM_ID {
            push_dlmm_edges(pair, store, &mut out);
            continue;
        }

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
    // CLMM engines price from a pool-account snapshot captured at build time —
    // no vault reads required.
    if let DexKind::MeteoraDammV2 {
        sqrt_price,
        liquidity,
        sqrt_min_price,
        sqrt_max_price,
        fee_numerator,
        collect_fee_mode,
        a_for_b,
    } = &edge.dex_kind
    {
        return meteora_damm_v2::quote_exact_in(
            amount_in,
            *a_for_b,
            *sqrt_price,
            *liquidity,
            *sqrt_min_price,
            *sqrt_max_price,
            *fee_numerator,
            *collect_fee_mode,
        );
    }
    if let DexKind::OrcaWhirlpoolV1 {
        sqrt_price,
        liquidity,
        sqrt_price_lower,
        sqrt_price_upper,
        fee_rate,
        a_for_b,
    } = &edge.dex_kind
    {
        return whirlpool::quote_exact_in(
            amount_in,
            *a_for_b,
            *sqrt_price,
            *liquidity,
            *sqrt_price_lower,
            *sqrt_price_upper,
            *fee_rate,
        );
    }
    if let DexKind::RaydiumClmm {
        sqrt_price,
        liquidity,
        sqrt_price_lower,
        sqrt_price_upper,
        trade_fee_rate,
        zero_for_one,
        tick_current,
        tick_spacing,
    } = &edge.dex_kind
    {
        let pool_pk = edge.pool;
        // Attempt multi-tick traversal if tick arrays are present in the store.
        // Falls back gracefully to single-tick when arrays are missing.
        let multi = raydium_clmm::quote_exact_in_multi_tick(
            amount_in,
            *zero_for_one,
            *sqrt_price,
            *liquidity,
            *tick_current,
            *tick_spacing,
            *trade_fee_rate,
            &pool_pk,
            |pda| store.accounts.get(&pda).map(|r| r.data.clone()),
        );
        if multi.is_some() {
            return multi;
        }
        // Tick arrays missing — single-tick quote (returns None on crossing).
        return raydium_clmm::quote_exact_in(
            amount_in,
            *zero_for_one,
            *sqrt_price,
            *liquidity,
            *sqrt_price_lower,
            *sqrt_price_upper,
            *trade_fee_rate,
        );
    }

    if let DexKind::MeteoraDlmm { pool, swap_for_y } = &edge.dex_kind {
        // Bin reserves live in BinArray accounts; derive each array's PDA and
        // read it from the live store. Missing arrays ⇒ conservative None.
        let lb_pair = edge.pool;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        return meteora_dlmm::quote_exact_in(pool, amount_in, *swap_for_y, now, |arr_index| {
            let pda = meteora_dlmm::derive_bin_array_pda(&lb_pair, arr_index);
            store.accounts.get(&pda).map(|r| r.data.clone())
        });
    }

    // Constant-product engines read live vault reserves.
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
        DexKind::PumpSwap => {
            pumpswap::quote_exact_in(amount_in, reserve_in, reserve_out)
        }
        DexKind::MeteoraDammV2 { .. }
        | DexKind::OrcaWhirlpoolV1 { .. }
        | DexKind::RaydiumClmm { .. }
        | DexKind::MeteoraDlmm { .. } => {
            unreachable!("handled above")
        }
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
    /// Token amounts at each intermediate hop output.
    /// 2-hop: [mid]  (output of hop 1 = input of hop 2)
    /// 3-hop: [mid1, mid2]
    pub intermediate_amounts: Vec<u64>,
    pub amount_in: u64,
    pub amount_out: u64,
    /// amount_out − amount_in (always > 0 for a CycleHit).
    pub profit_gross: u64,
    /// Gross profit minus tx_cost and n_hops × error_margin (may be < 0).
    pub profit_net: i64,
    /// True if this hit was refined by ternary search (not just a coarse-scan point).
    pub optimized: bool,
    /// The coarse-scan `amount_in` that seeded the ternary search, if any.
    pub coarse_seed_amount: Option<u64>,
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
                    intermediate_amounts: vec![mid],
                    amount_in,
                    amount_out: out,
                    profit_gross: gross,
                    profit_net: net,
                    optimized: false,
                    coarse_seed_amount: None,
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
                        intermediate_amounts: vec![mid1, mid2],
                        amount_in,
                        amount_out: out,
                        profit_gross: gross,
                        profit_net: net,
                        optimized: false,
                        coarse_seed_amount: None,
                    });
                }
            }
        }
    }

    // Dedup: keep only the best hit per unique (pools, intermediate_mints) key —
    // drop redundant coarse-grid hits before the optimisation pass.
    hits.sort_unstable_by(|a, b| b.profit_gross.cmp(&a.profit_gross));

    // ── Optimisation pass ──────────────────────────────────────────────────────
    // For the top unique cycles found by the coarse scan, run a ternary search
    // to find the exact amount_in that maximises gross profit.  This naturally
    // handles DLMM bin crossings (the quote function already traverses bins) and
    // CLMM tick crossings (when tick arrays are in the store).
    //
    // Key includes intermediate_mints to avoid deduping two routes that share
    // pool addresses but trade different token pairs in different directions.
    {
        type CycleKey = (Vec<Pubkey>, Vec<Pubkey>); // (pools, intermediate_mints)

        let find_edge_idx = |pool: Pubkey, mint_in: Pubkey| -> Option<usize> {
            by_mint_in.get(&mint_in).and_then(|idxs| {
                idxs.iter().find(|&&i| edges[i].pool == pool).copied()
            })
        };

        let cycle_key = |h: &CycleHit| -> CycleKey {
            (h.pools.clone(), h.intermediate_mints.clone())
        };

        let mut seen_paths: std::collections::HashSet<CycleKey> = std::collections::HashSet::new();
        let mut opt_hits: Vec<CycleHit> = Vec::new();
        let coarse_hits_snap: Vec<CycleHit> = hits.iter().take(50).cloned().collect();

        for hit in &coarse_hits_snap {
            let key = cycle_key(hit);
            if !seen_paths.insert(key) {
                continue;
            }
            if opt_hits.len() >= 20 {
                break;
            }
            let hops = hit.hops();
            // Search bounds: ±10× the coarse best amount to stay near the
            // known profitable region, capped to global limits.
            let seed = hit.amount_in;
            let lo = (seed / 10).clamp(SEARCH_MIN_LAMPORTS, SEARCH_MAX_LAMPORTS);
            let hi = (seed.saturating_mul(10)).clamp(lo, SEARCH_MAX_LAMPORTS);

            let coarse_seed = hit.amount_in;

            if hops == 2 {
                let x = hit.intermediate_mints[0];
                let i1 = match find_edge_idx(hit.pools[0], wsol_mint) { Some(i) => i, None => continue };
                let i2 = match find_edge_idx(hit.pools[1], x) { Some(i) => i, None => continue };
                if let Some((amt, gross)) = optimise_2hop(
                    &edges[i1], &edges[i2], store, lo, hi,
                ) {
                    // Keep whichever is better — ternary or the coarse seed point.
                    let (final_amt, final_gross) = if gross > hit.profit_gross {
                        (amt, gross)
                    } else {
                        (hit.amount_in, hit.profit_gross)
                    };
                    let out = final_amt + final_gross;
                    let net = (out as i64) - (final_amt as i64) - (tx_cost as i64)
                        - (error_margin_per_hop as i64 * 2);
                    let mid_amt = quote_edge(&edges[i1], final_amt, store).unwrap_or(0);
                    opt_hits.push(CycleHit {
                        pools: hit.pools.clone(),
                        dex_names: hit.dex_names.clone(),
                        intermediate_mints: hit.intermediate_mints.clone(),
                        intermediate_amounts: vec![mid_amt],
                        amount_in: final_amt,
                        amount_out: out,
                        profit_gross: final_gross,
                        profit_net: net,
                        optimized: true,
                        coarse_seed_amount: Some(coarse_seed),
                    });
                }
            } else if hops == 3 {
                let x = hit.intermediate_mints[0];
                let y = hit.intermediate_mints[1];
                let i1 = match find_edge_idx(hit.pools[0], wsol_mint) { Some(i) => i, None => continue };
                let i2 = match find_edge_idx(hit.pools[1], x) { Some(i) => i, None => continue };
                let i3 = match find_edge_idx(hit.pools[2], y) { Some(i) => i, None => continue };
                if let Some((amt, gross)) = optimise_3hop(
                    &edges[i1], &edges[i2], &edges[i3], store, lo, hi,
                ) {
                    let (final_amt, final_gross) = if gross > hit.profit_gross {
                        (amt, gross)
                    } else {
                        (hit.amount_in, hit.profit_gross)
                    };
                    let out = final_amt + final_gross;
                    let net = (out as i64) - (final_amt as i64) - (tx_cost as i64)
                        - (error_margin_per_hop as i64 * 3);
                    let mid1_amt = quote_edge(&edges[i1], final_amt, store).unwrap_or(0);
                    let mid2_amt = quote_edge(&edges[i2], mid1_amt, store).unwrap_or(0);
                    opt_hits.push(CycleHit {
                        pools: hit.pools.clone(),
                        dex_names: hit.dex_names.clone(),
                        intermediate_mints: hit.intermediate_mints.clone(),
                        intermediate_amounts: vec![mid1_amt, mid2_amt],
                        amount_in: final_amt,
                        amount_out: out,
                        profit_gross: final_gross,
                        profit_net: net,
                        optimized: true,
                        coarse_seed_amount: Some(coarse_seed),
                    });
                }
            }
        }

        // Replace coarse-scan entries with optimised ones (which already keep the
        // best of coarse vs ternary). Key on (pools, intermediate_mints) to avoid
        // false deduplication of same-pool paths with different token directions.
        let opt_keys: std::collections::HashSet<CycleKey> =
            opt_hits.iter().map(cycle_key).collect();
        hits.retain(|h| !opt_keys.contains(&cycle_key(h)));
        hits.extend(opt_hits);
    }

    // Sort by net profit descending — execution candidates should be ranked by
    // what actually lands in the wallet after fees, not raw gross.
    hits.sort_unstable_by(|a, b| b.profit_net.cmp(&a.profit_net));
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
/// Covers 0.001 → 0.5 SOL with key points for a fast overview.
/// After the initial scan, `find_cycles_in_store` refines the top hits with
/// ternary search — these coarse points just seed the search range.
pub const SCAN_AMOUNTS: &[u64] = &[
    1_000_000,    // 0.001 SOL
    5_000_000,    // 0.005 SOL
    10_000_000,   // 0.010 SOL
    25_000_000,   // 0.025 SOL
    50_000_000,   // 0.050 SOL
    100_000_000,  // 0.100 SOL
    250_000_000,  // 0.250 SOL
    500_000_000,  // 0.500 SOL
];

/// Minimum input amount for ternary search (1 SOL = 1 lamport here is too small).
const SEARCH_MIN_LAMPORTS: u64 = 100_000; // 0.0001 SOL
/// Maximum input amount for ternary search.
const SEARCH_MAX_LAMPORTS: u64 = 1_000_000_000; // 1.0 SOL

/// Gross profit (amount_out − amount_in) for a 2-hop cycle, or 0 if no profit.
/// Returns `None` if the quote fails.
fn gross_2hop(
    e1: &Edge,
    e2: &Edge,
    amount_in: u64,
    store: &Arc<PoolStateStore>,
) -> Option<u64> {
    let mid = quote_edge(e1, amount_in, store)?;
    let out = quote_edge(e2, mid, store)?;
    out.checked_sub(amount_in)
}

/// Gross profit for a 3-hop cycle.
fn gross_3hop(
    e1: &Edge,
    e2: &Edge,
    e3: &Edge,
    amount_in: u64,
    store: &Arc<PoolStateStore>,
) -> Option<u64> {
    let mid1 = quote_edge(e1, amount_in, store)?;
    let mid2 = quote_edge(e2, mid1, store)?;
    let out = quote_edge(e3, mid2, store)?;
    out.checked_sub(amount_in)
}

/// Ternary search for the `amount_in` that maximises gross profit on a 2-hop
/// cycle.  The profit curve is unimodal (concave) for all CP / CLMM / DLMM
/// pools: slippage increases with size, so there is a single peak.
///
/// Returns `(best_amount, best_gross)` or `None` if no gross-positive amount
/// is found in `[lo, hi]`.
fn optimise_2hop(
    e1: &Edge,
    e2: &Edge,
    store: &Arc<PoolStateStore>,
    mut lo: u64,
    mut hi: u64,
) -> Option<(u64, u64)> {
    // 50 iterations: converges to within (hi−lo)/3^50 ≈ 0.
    for _ in 0..50 {
        if hi <= lo + 3 { break; }
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;
        let p1 = gross_2hop(e1, e2, m1, store).unwrap_or(0);
        let p2 = gross_2hop(e1, e2, m2, store).unwrap_or(0);
        if p1 < p2 { lo = m1; } else { hi = m2; }
    }
    // Sample a few points near the found optimum to handle flat plateaus.
    let candidates = [
        lo,
        lo + (hi - lo) / 4,
        (lo + hi) / 2,
        hi - (hi - lo) / 4,
        hi,
    ];
    let best = candidates
        .iter()
        .filter_map(|&a| gross_2hop(e1, e2, a, store).map(|g| (a, g)))
        .max_by_key(|&(_, g)| g)?;
    if best.1 > 0 { Some(best) } else { None }
}

/// Ternary search for optimal amount on a 3-hop cycle.
fn optimise_3hop(
    e1: &Edge,
    e2: &Edge,
    e3: &Edge,
    store: &Arc<PoolStateStore>,
    mut lo: u64,
    mut hi: u64,
) -> Option<(u64, u64)> {
    for _ in 0..50 {
        if hi <= lo + 3 { break; }
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;
        let p1 = gross_3hop(e1, e2, e3, m1, store).unwrap_or(0);
        let p2 = gross_3hop(e1, e2, e3, m2, store).unwrap_or(0);
        if p1 < p2 { lo = m1; } else { hi = m2; }
    }
    let candidates = [lo, lo + (hi - lo) / 4, (lo + hi) / 2, hi - (hi - lo) / 4, hi];
    let best = candidates
        .iter()
        .filter_map(|&a| gross_3hop(e1, e2, e3, a, store).map(|g| (a, g)))
        .max_by_key(|&(_, g)| g)?;
    if best.1 > 0 { Some(best) } else { None }
}

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
///
/// If `hit_tx` is `Some`, all net-positive hits are forwarded to the cycle
/// executor via the channel (non-blocking — hits are dropped if the channel
/// is full rather than stalling the scanner).
pub fn spawn_cycle_scanner(
    pairs: Vec<PoolVaultPair>,
    store: Arc<PoolStateStore>,
    interval_secs: u64,
    max_log: usize,
    tx_cost: u64,
    hit_tx: Option<tokio::sync::mpsc::Sender<CycleHit>>,
) {
    let metrics = Arc::new(CycleMetrics::default());
    let pairs = Arc::new(pairs);
    let hit_tx = Arc::new(hit_tx);
    // Set VERBOSE_STATS=1 to re-enable [cycle] per-scan log lines.
    let verbose = std::env::var("VERBOSE_STATS").map(|v| v == "1").unwrap_or(false);

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
                if verbose { eprintln!("[cycle] no live edges yet (live_pools={live_pools})"); }
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

            if verbose {
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
                    let opt_tag = if hit.optimized {
                        if let Some(seed) = hit.coarse_seed_amount {
                            format!(" opt=1 seed={:.4}SOL", seed as f64 / 1e9)
                        } else {
                            " opt=1".to_string()
                        }
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "[cycle]   #{rank} {}-hop{opt_tag}  in={:.4}SOL gross={:+}L net={:+}L  \
path=[{}]  via=[{}]",
                        hit.hops(),
                        hit.amount_in as f64 / 1e9,
                        hit.profit_gross as i64,
                        hit.profit_net,
                        path.join("→"),
                        if mints.is_empty() { "direct".to_string() } else { mints.join("→") },
                    );
                }
            }

            // Forward all net-positive hits to the executor or validator (non-blocking).
            if let Some(ref sender) = *hit_tx {
                for hit in hits.iter().filter(|h| h.profit_net > 0) {
                    let _ = sender.try_send(hit.clone());
                }
            }

            // Summary stats.
            if verbose {
                let total_gross = metrics.gross_positive.load(Ordering::Relaxed);
                let total_net = metrics.net_positive.load(Ordering::Relaxed);
                let best_gross_ever = metrics.best_gross_lamports.load(Ordering::Relaxed);
                let best_net_ever = metrics.best_net_lamports.load(Ordering::Relaxed);
                eprintln!(
                    "[cycle] cumulative: scans={scans} gross+={total_gross} net+={total_net} \
best_gross_ever={best_gross_ever}L best_net_ever={best_net_ever}L"
                );
            }
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
