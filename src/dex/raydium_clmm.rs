//! Raydium CLMM — concentrated-liquidity AMM.
//!
//! Program: `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`
//!
//! Raydium CLMM is a Uniswap-v3-style CLMM using the same Q64.64 sqrt-price
//! representation as Orca Whirlpool. Swap math and tick math are shared verbatim
//! (`whirlpool::quote_exact_in` / `whirlpool::sqrt_price_from_tick_index`).
//!
//! Source cross-validation:
//!   • raydium-io/raydium-clmm programs/amm/src/ (PoolState layout, AmmConfig)
//!   • raydium-io/raydium-clmm programs/amm/src/states/tick_array.rs (TickArray)
//!   • Raydium CLMM on-chain program: tick math identical to Whirlpool / Uniswap v3
//!
//! Fee is in the pool's AmmConfig account (separate from the pool account):
//!   • AmmConfig.trade_fee_rate is u32 parts-per-1_000_000
//!   • If AmmConfig is unavailable, we fall back to inferring from tick_spacing
//!     using the standard Raydium CLMM tier mapping.
//!
//! Pool account (PoolState) layout (Anchor, LE integers):
//!   0..8    discriminator
//!   8       bump (u8)
//!   9..41   amm_config (Pubkey)
//!   41..73  owner (Pubkey)
//!   73..105 token_mint_0 (Pubkey)
//!   105..137 token_mint_1 (Pubkey)
//!   137..169 token_vault_0 (Pubkey)
//!   169..201 token_vault_1 (Pubkey)
//!   201..233 observation_key (Pubkey)
//!   233     mint_decimals_0 (u8)
//!   234     mint_decimals_1 (u8)
//!   235..237 tick_spacing (u16)
//!   237..253 liquidity (u128)
//!   253..269 sqrt_price_x64 (u128, Q64.64)
//!   269..273 tick_current (i32)
//!
//! AmmConfig layout (Anchor, LE integers):
//!   0..8   discriminator
//!   8      bump (u8)
//!   9..11  index (u16)
//!   11..43 owner (Pubkey)
//!   43..47 protocol_fee_rate (u32)
//!   47..51 trade_fee_rate (u32)  ← fee used for swap math
//!
//! TickArrayState layout (Anchor zero_copy, LE integers, repr(packed)):
//!   0..8    discriminator
//!   8..40   pool_id (Pubkey)
//!   40..44  start_tick_index (i32)
//!   44..14924  ticks: [TickState; TICK_ARRAY_SIZE=60]  (each 248 bytes, repr(packed))
//!     Each TickState (248 bytes, repr(packed)):
//!       0..4    tick (i32)
//!       4..20   liquidity_net (i128)  ← used for crossing
//!       20..36  liquidity_gross (u128)
//!       36..52  fee_growth_outside_0_x64 (u128)
//!       52..68  fee_growth_outside_1_x64 (u128)
//!       68..76  tick_cumulative_outside (i64)
//!       76..92  seconds_per_liquidity_outside_x64 (u128)
//!       92..96  seconds_outside (u32)
//!       96..144 rewards_growth_outside ([u128; 3])
//!       144..248 padding ([u64; 13])
//!   14924..14925 initialized_tick_count (u8)
//!   14932..14940 recent_epoch (u64)  [+7 align padding after initialized_tick_count]
//!   14940..15684 padding ([u64; 93])
//!
//! TickArray PDA seeds: [b"tick_array", pool_id, start_tick_index_i32_be_bytes]

use solana_sdk::pubkey::Pubkey;

use crate::dex::whirlpool;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// Denominator for `trade_fee_rate` (parts per million).
pub const FEE_RATE_DENOMINATOR: u32 = 1_000_000;

/// Ticks per TickArray (from raydium-clmm constants.rs).
pub const TICK_ARRAY_SIZE: i32 = 60;

/// Max ticks the multi-tick quote will cross before giving up (prevents pathological loops).
const MAX_TICKS_CROSSED: u32 = 20;

mod pool_offsets {
    pub const AMM_CONFIG: usize = 9;
    pub const TOKEN_MINT_0: usize = 73;
    pub const TOKEN_MINT_1: usize = 105;
    pub const TOKEN_VAULT_0: usize = 137;
    pub const TOKEN_VAULT_1: usize = 169;
    pub const TICK_SPACING: usize = 235;
    pub const LIQUIDITY: usize = 237;
    pub const SQRT_PRICE_X64: usize = 253;
    pub const TICK_CURRENT: usize = 269;
    pub const MIN_LEN: usize = 273;
}

mod amm_config_offsets {
    pub const TRADE_FEE_RATE: usize = 47;
    pub const MIN_LEN: usize = 51;
}

mod tick_array_offsets {
    /// Byte offset of start_tick_index within TickArrayState.
    pub const START_TICK_INDEX: usize = 40;
    /// Byte offset of the first TickState within TickArrayState.
    pub const TICKS_START: usize = 44;
    /// Size of one TickState (repr(packed), no internal padding).
    pub const TICK_STATE_SIZE: usize = 248;
    /// Byte offset of `tick` within a TickState.
    pub const TICK_OFFSET: usize = 0;
    /// Byte offset of `liquidity_net` (i128) within a TickState.
    pub const LIQUIDITY_NET_OFFSET: usize = 4;
    /// Minimum TickArrayState length to be valid.
    pub const MIN_LEN: usize = 44 + 60 * 248; // 14924 bytes
}

/// Snapshot of Raydium CLMM pool state for local quote computation.
#[derive(Debug, Clone)]
pub struct ClmmPool {
    /// Pubkey of the associated AmmConfig account.
    pub amm_config: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub tick_spacing: u16,
    pub liquidity: u128,
    /// Current sqrt_price in Q64.64.
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    /// Q64.64 sqrt_price of the lower tick boundary of the current tick range.
    pub sqrt_price_lower: u128,
    /// Q64.64 sqrt_price of the upper tick boundary of the current tick range.
    pub sqrt_price_upper: u128,
}

/// Parse a Raydium CLMM PoolState account.
/// Returns `None` if data is too short, liquidity is zero, or tick boundaries
/// can't be computed (tick index out of supported range).
pub fn parse_pool(data: &[u8]) -> Option<ClmmPool> {
    use pool_offsets::*;
    if data.len() < MIN_LEN {
        return None;
    }

    let amm_config = Pubkey::from(
        <[u8; 32]>::try_from(&data[AMM_CONFIG..AMM_CONFIG + 32]).ok()?,
    );
    let token_mint_0 = Pubkey::from(
        <[u8; 32]>::try_from(&data[TOKEN_MINT_0..TOKEN_MINT_0 + 32]).ok()?,
    );
    let token_mint_1 = Pubkey::from(
        <[u8; 32]>::try_from(&data[TOKEN_MINT_1..TOKEN_MINT_1 + 32]).ok()?,
    );
    let token_vault_0 = Pubkey::from(
        <[u8; 32]>::try_from(&data[TOKEN_VAULT_0..TOKEN_VAULT_0 + 32]).ok()?,
    );
    let token_vault_1 = Pubkey::from(
        <[u8; 32]>::try_from(&data[TOKEN_VAULT_1..TOKEN_VAULT_1 + 32]).ok()?,
    );
    let tick_spacing =
        u16::from_le_bytes(data[TICK_SPACING..TICK_SPACING + 2].try_into().ok()?);
    let liquidity =
        u128::from_le_bytes(data[LIQUIDITY..LIQUIDITY + 16].try_into().ok()?);
    let sqrt_price_x64 =
        u128::from_le_bytes(data[SQRT_PRICE_X64..SQRT_PRICE_X64 + 16].try_into().ok()?);
    let tick_current =
        i32::from_le_bytes(data[TICK_CURRENT..TICK_CURRENT + 4].try_into().ok()?);

    if liquidity == 0 || tick_spacing == 0 {
        return None;
    }
    if sqrt_price_x64 < whirlpool::MIN_SQRT_PRICE_X64
        || sqrt_price_x64 > whirlpool::MAX_SQRT_PRICE_X64
    {
        return None;
    }

    // Current active tick range: [lower, lower + tick_spacing) where lower is
    // the largest multiple of tick_spacing that is ≤ tick_current.
    let lower_tick =
        tick_current.div_euclid(tick_spacing as i32) * tick_spacing as i32;
    let upper_tick = lower_tick + tick_spacing as i32;

    let sqrt_price_lower = whirlpool::sqrt_price_from_tick_index(lower_tick)?;
    let sqrt_price_upper = whirlpool::sqrt_price_from_tick_index(upper_tick)?;

    Some(ClmmPool {
        amm_config,
        token_mint_0,
        token_mint_1,
        token_vault_0,
        token_vault_1,
        tick_spacing,
        liquidity,
        sqrt_price_x64,
        tick_current,
        sqrt_price_lower,
        sqrt_price_upper,
    })
}

/// Read `trade_fee_rate` (u32, parts-per-1_000_000) from a raw AmmConfig account.
pub fn parse_trade_fee_rate(data: &[u8]) -> Option<u32> {
    use amm_config_offsets::*;
    if data.len() < MIN_LEN {
        return None;
    }
    Some(u32::from_le_bytes(
        data[TRADE_FEE_RATE..TRADE_FEE_RATE + 4].try_into().ok()?,
    ))
}

/// Infer `trade_fee_rate` from tick_spacing using standard Raydium CLMM tier
/// mapping (matches deployed AmmConfig accounts on mainnet).
pub fn fee_rate_from_tick_spacing(tick_spacing: u16) -> u32 {
    match tick_spacing {
        1 => 100,     // 0.01%
        10 => 500,    // 0.05%
        60 => 2_500,  // 0.25%
        200 => 10_000, // 1%
        400 => 20_000, // 2%
        _ => 2_500,   // default to 0.25%
    }
}

/// Compute the start tick index of the TickArray that contains `tick`.
///
/// From raydium-clmm source (math/mod.rs):
///   start = floor(tick / (tick_spacing * TICK_ARRAY_SIZE)) × (tick_spacing * TICK_ARRAY_SIZE)
/// Special case for negative ticks: floor towards −∞, not 0 (same as div_euclid).
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_per_array = tick_spacing as i32 * TICK_ARRAY_SIZE;
    // Rust div rounds toward zero; we need floor (towards -∞).
    tick.div_euclid(ticks_per_array) * ticks_per_array
}

/// Derive the PDA for a Raydium CLMM TickArray account.
///
/// Seeds: `[b"tick_array", pool_id, start_tick_index.to_be_bytes()]`
/// (big-endian i32 — confirmed from raydium SDK source)
pub fn derive_tick_array_pda(pool_id: &Pubkey, start_tick_index: i32) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[
            b"tick_array",
            pool_id.as_ref(),
            &start_tick_index.to_be_bytes(),
        ],
        &PROGRAM_ID,
    );
    pda
}

/// Read the `liquidity_net` (i128, LE) for the tick at `tick_index` from a
/// raw TickArrayState account.  Returns `None` if the array doesn't cover
/// that tick or the data is too short.
fn read_liquidity_net(
    data: &[u8],
    tick_index: i32,
    start_tick_index: i32,
    tick_spacing: u16,
) -> Option<i128> {
    use tick_array_offsets::*;
    if data.len() < MIN_LEN {
        return None;
    }
    // Verify stored start_tick_index matches.
    let stored_start = i32::from_le_bytes(
        data[START_TICK_INDEX..START_TICK_INDEX + 4].try_into().ok()?,
    );
    if stored_start != start_tick_index {
        return None;
    }
    // Which slot in the array?
    let slot = (tick_index - start_tick_index) / tick_spacing as i32;
    if slot < 0 || slot >= TICK_ARRAY_SIZE {
        return None;
    }
    let base = TICKS_START + slot as usize * TICK_STATE_SIZE;
    if data.len() < base + TICK_STATE_SIZE {
        return None;
    }
    let liq_net = i128::from_le_bytes(
        data[base + LIQUIDITY_NET_OFFSET..base + LIQUIDITY_NET_OFFSET + 16]
            .try_into()
            .ok()?,
    );
    Some(liq_net)
}

/// Exact-in swap quote for one Raydium CLMM pool (single-tick, no tick-array).
///
/// Delegates to `whirlpool::quote_exact_in` — tick math and swap math are
/// identical between the two programs (both use Q64.64 sqrt-price, same
/// fee-on-input formula with 1_000_000 denominator).
///
/// Returns `None` on overflow, zero liquidity, or tick-boundary crossing.
pub fn quote_exact_in(
    amount_in: u64,
    zero_for_one: bool,
    sqrt_price: u128,
    liquidity: u128,
    sqrt_price_lower: u128,
    sqrt_price_upper: u128,
    trade_fee_rate: u32,
) -> Option<u64> {
    // Valid Raydium CLMM fee tiers (100/500/2500/10000/20000) all fit in u16.
    let fee_rate = u16::try_from(trade_fee_rate).ok()?;
    whirlpool::quote_exact_in(
        amount_in,
        zero_for_one,
        sqrt_price,
        liquidity,
        sqrt_price_lower,
        sqrt_price_upper,
        fee_rate,
    )
}

/// Exact-in swap quote for Raydium CLMM with multi-tick traversal.
///
/// Simulates the full Uniswap-v3-style swap loop: cross tick boundaries one at
/// a time, adjusting liquidity via `liquidity_net`, until `amount_in` is
/// exhausted. Each tick boundary's TickArray account is loaded lazily via the
/// supplied `get_tick_array` closure.
///
/// Returns `None` if:
/// - a required TickArray is missing from the store (conservative)
/// - liquidity drops to zero mid-swap
/// - arithmetic overflow
///
/// Falls back automatically to the single-tick quote when `amount_in` stays
/// within the current tick (tick array not needed).
/// `get_tick_array` receives the already-derived PDA of the requested TickArray.
/// The caller looks it up in the store: `|pda| store.accounts.get(&pda).map(|r| r.data.clone())`.
pub fn quote_exact_in_multi_tick<F>(
    amount_in: u64,
    zero_for_one: bool,
    mut sqrt_price: u128,
    mut liquidity: u128,
    tick_current: i32,
    tick_spacing: u16,
    trade_fee_rate: u32,
    pool_id: &Pubkey,
    mut get_tick_array: F,
) -> Option<u64>
where
    F: FnMut(Pubkey) -> Option<Vec<u8>>,
{
    if amount_in == 0 || liquidity == 0 {
        return None;
    }
    let fee_rate_u128 = trade_fee_rate as u128;
    let fee_denom = 1_000_000u128;
    let fee_comp = fee_denom.checked_sub(fee_rate_u128)?;

    let mut amount_left = amount_in;
    let mut total_out: u64 = 0;
    let mut tick = tick_current;

    // Cache: (arr_start, raw_data) — avoid redundant PDA lookups.
    let mut cached_arr_start: Option<i32> = None;
    let mut cached_arr_data: Vec<u8> = Vec::new();

    for _ in 0..MAX_TICKS_CROSSED {
        if amount_left == 0 {
            break;
        }

        // Tick boundary we'll hit next in the swap direction.
        let boundary_tick = if zero_for_one {
            // Price decreases (token0 in): crosses lower boundary of current interval.
            let lower = tick.div_euclid(tick_spacing as i32) * tick_spacing as i32;
            // If we're already exactly on the lower boundary, go one more step down.
            if tick == lower { lower - tick_spacing as i32 } else { lower }
        } else {
            // Price increases (token1 in): crosses upper boundary of current interval.
            let lower = tick.div_euclid(tick_spacing as i32) * tick_spacing as i32;
            lower + tick_spacing as i32
        };

        let sqrt_boundary = whirlpool::sqrt_price_from_tick_index(boundary_tick)?;
        // Clamp to global price limits.
        let sqrt_target = if zero_for_one {
            sqrt_boundary.max(whirlpool::MIN_SQRT_PRICE_X64 + 1)
        } else {
            sqrt_boundary.min(whirlpool::MAX_SQRT_PRICE_X64 - 1)
        };

        // How much input (after fee) is needed to reach sqrt_target?
        let amount_after_fee_to_target = if zero_for_one {
            // Token A in: compute A needed to move price from sqrt_price → sqrt_target.
            // Δa = L × (1/sqrt_target − 1/sqrt_price) = L × (sqrt_price − sqrt_target) /
            //      (sqrt_price × sqrt_target) × 2^64
            compute_amount_a_in(sqrt_price, sqrt_target, liquidity)?
        } else {
            // Token B in: Δb = L × (sqrt_target − sqrt_price) / 2^64
            compute_amount_b_in(sqrt_price, sqrt_target, liquidity)?
        };

        // Gross input (including fee) to reach target: gross = ceil(net / fee_comp * fee_denom).
        let gross_to_target: u64 = {
            let g = (amount_after_fee_to_target as u128)
                .checked_mul(fee_denom)?
                .checked_add(fee_comp - 1)? // ceil
                / fee_comp;
            u64::try_from(g).unwrap_or(u64::MAX)
        };

        let (step_out, consumed) = if amount_left >= gross_to_target {
            // Cross the boundary: swap all the way to sqrt_target.
            let out = if zero_for_one {
                whirlpool::amount_delta_b(sqrt_price, sqrt_target, liquidity)?
            } else {
                whirlpool::amount_delta_a(sqrt_target, sqrt_price, liquidity)?
            };
            (out, gross_to_target)
        } else {
            // Stays within this tick range: partial swap.
            let after_fee = (amount_left as u128 * fee_comp / fee_denom) as u64;
            if after_fee == 0 {
                break;
            }
            let next_sqrt = if zero_for_one {
                whirlpool::next_sqrt_from_a_round_up(sqrt_price, liquidity, after_fee)?
            } else {
                whirlpool::next_sqrt_from_b_round_down(sqrt_price, liquidity, after_fee)?
            };
            let out = if zero_for_one {
                whirlpool::amount_delta_b(sqrt_price, next_sqrt, liquidity)?
            } else {
                whirlpool::amount_delta_a(next_sqrt, sqrt_price, liquidity)?
            };
            sqrt_price = next_sqrt;
            (out, amount_left)
        };

        total_out = total_out.checked_add(step_out)?;
        amount_left = amount_left.saturating_sub(consumed);

        if amount_left == 0 {
            break;
        }

        // We crossed boundary_tick — load the TickArray and apply liquidity_net.
        sqrt_price = sqrt_target;

        let arr_start = tick_array_start_index(boundary_tick, tick_spacing);
        if cached_arr_start != Some(arr_start) {
            let pda = derive_tick_array_pda(pool_id, arr_start);
            let data = get_tick_array(pda).unwrap_or_default();
            if data.len() < tick_array_offsets::MIN_LEN {
                // Missing or undersized tick array — fall back to single-tick mode.
                // Return whatever output we've accumulated so far (conservative).
                return if total_out > 0 { Some(total_out) } else { None };
            }
            cached_arr_data = data;
            cached_arr_start = Some(arr_start);
        }

        let liq_net = read_liquidity_net(&cached_arr_data, boundary_tick, arr_start, tick_spacing)
            .unwrap_or(0i128);

        // Uniswap v3 convention: liq_net is added when crossing LEFT→RIGHT (price up).
        // Crossing RIGHT→LEFT (zero_for_one): subtract. Crossing LEFT→RIGHT: add.
        if zero_for_one {
            liquidity = (liquidity as i128).checked_sub(liq_net)? as u128;
            tick = boundary_tick - 1; // now below the crossed tick
        } else {
            liquidity = (liquidity as i128).checked_add(liq_net)? as u128;
            tick = boundary_tick;
        }

        if liquidity == 0 {
            return None; // no liquidity in this range
        }
    }

    if total_out == 0 { None } else { Some(total_out) }
}

// ── Amount helpers for multi-tick ─────────────────────────────────────────────

/// Token-A input to move sqrt_price from `p_cur` to `p_next` (p_next < p_cur).
/// From Uniswap v3: Δa = L × (p_cur − p_next) / (p_cur × p_next / 2^64)
///                     = L × (p_cur − p_next) × 2^64 / (p_cur × p_next)
fn compute_amount_a_in(p_cur: u128, p_next: u128, liquidity: u128) -> Option<u64> {
    debug_assert!(p_cur >= p_next);
    use crate::dex::uint256::U256;
    let delta = p_cur.checked_sub(p_next)?;
    // numerator = liquidity × delta << 64
    let ld = U256::mul_u128(liquidity, delta);
    let numerator = ld.checked_shl64()?;
    let denominator = U256::mul_u128(p_cur, p_next);
    // ceil for exact-in (protects pool)
    let out = numerator.div_ceil_u128(denominator)?;
    u64::try_from(out).ok()
}

/// Token-B input to move sqrt_price from `p_cur` to `p_next` (p_next > p_cur).
/// From Uniswap v3: Δb = ceil(L × (p_next − p_cur) / 2^64)
fn compute_amount_b_in(p_cur: u128, p_next: u128, liquidity: u128) -> Option<u64> {
    debug_assert!(p_next >= p_cur);
    use crate::dex::uint256::U256;
    let delta = p_next.checked_sub(p_cur)?;
    let prod = U256::mul_u128(liquidity, delta);
    // ceil(prod / 2^64) — reuse div_ceil_u128 with divisor = 2^64
    let two_pow_64 = U256::from_u128(1u128 << 64);
    let out = prod.div_ceil_u128(two_pow_64)?;
    u64::try_from(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use whirlpool::sqrt_price_from_tick_index;

    const Q64: u128 = 1 << 64; // sqrt_price at tick=0 (price=1.0)
    const L: u128 = 10_000;

    fn wide_bounds() -> (u128, u128) {
        let lower = sqrt_price_from_tick_index(-10_000).unwrap();
        let upper = sqrt_price_from_tick_index(10_000).unwrap();
        (lower, upper)
    }

    #[test]
    fn zero_inputs_return_none() {
        let (lo, hi) = wide_bounds();
        assert!(quote_exact_in(0, true, Q64, L, lo, hi, 500).is_none());
        assert!(quote_exact_in(100, true, Q64, 0, lo, hi, 500).is_none());
    }

    #[test]
    fn fee_rate_too_large_returns_none() {
        let (lo, hi) = wide_bounds();
        // Fee rate > u16::MAX should return None safely.
        assert!(quote_exact_in(100, true, Q64, L, lo, hi, 70_000).is_none());
    }

    #[test]
    fn golden_zero_for_one_no_fee() {
        // token0→token1 at tick=0, fee=0, balanced pool: out ≈ in
        let (lo, hi) = wide_bounds();
        let out = quote_exact_in(100, true, Q64, L, lo, hi, 0).unwrap();
        assert_eq!(out, 99); // same as whirlpool golden test
    }

    #[test]
    fn golden_one_for_zero_no_fee() {
        let (lo, hi) = wide_bounds();
        let out = quote_exact_in(100, false, Q64, L, lo, hi, 0).unwrap();
        assert_eq!(out, 99);
    }

    #[test]
    fn golden_with_fee_2500() {
        // 0.25% fee: amount_after_fee = floor(100 * 997500 / 1000000) = 99
        // Then swap gives ~99 out (small price impact at L=10000)
        let (lo, hi) = wide_bounds();
        let out = quote_exact_in(100, true, Q64, L, lo, hi, 2_500).unwrap();
        assert_eq!(out, 98); // same as whirlpool golden test with fee_rate=2500
    }

    #[test]
    fn tick_boundary_crossing_returns_none() {
        // Tiny tick range [0, 1] — any appreciable swap crosses the boundary.
        let lower = sqrt_price_from_tick_index(0).unwrap();
        let upper = sqrt_price_from_tick_index(1).unwrap();
        // Large swap well beyond the tick range → None
        assert!(quote_exact_in(10_000_000, true, Q64, L, lower, upper, 0).is_none());
    }

    #[test]
    fn fee_rate_from_tick_spacing_coverage() {
        assert_eq!(fee_rate_from_tick_spacing(1), 100);
        assert_eq!(fee_rate_from_tick_spacing(10), 500);
        assert_eq!(fee_rate_from_tick_spacing(60), 2_500);
        assert_eq!(fee_rate_from_tick_spacing(200), 10_000);
        assert_eq!(fee_rate_from_tick_spacing(400), 20_000);
        assert_eq!(fee_rate_from_tick_spacing(99), 2_500); // default
    }

    #[test]
    fn parse_trade_fee_rate_too_short_returns_none() {
        assert!(parse_trade_fee_rate(&[0u8; 10]).is_none());
    }
}
