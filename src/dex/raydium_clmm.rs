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

use solana_sdk::pubkey::Pubkey;

use crate::dex::whirlpool;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// Denominator for `trade_fee_rate` (parts per million).
pub const FEE_RATE_DENOMINATOR: u32 = 1_000_000;

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
