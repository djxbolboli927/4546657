//! Orca Whirlpool concentrated-liquidity swap calculator.
//!
//! Program: `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`
//!
//! Whirlpool is a Uniswap-v3-style CLMM. State lives in a single pool account
//! (sqrt_price + liquidity + tick_current_index), not vault reserves. Swap math
//! is cross-validated against:
//!   • orca-so/whirlpools programs/whirlpool/src/math/{swap_math,token_math,tick_math}.rs
//!   • orca-so/whirlpools legacy-sdk/whirlpool/src/math/
//!
//! Tick arrays (neighbour tick liquidity deltas) are NOT streamed. A swap that
//! would cross the current-tick's boundary is returned as `None` — conservative:
//! we can miss real edges at boundaries but we never invent phantom edges.
//!
//! All sqrt_prices are Q64.64 fixed-point (stored as u128). Fee rate is u16
//! parts-per-1_000_000 (so 3000 ≡ 0.3%).

use solana_sdk::pubkey::Pubkey;

use crate::dex::uint256::U256;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Denominator for `fee_rate` (parts per million).
pub const FEE_RATE_MUL_VALUE: u128 = 1_000_000;

/// Minimum valid Q64.64 sqrt_price (corresponds to MIN_TICK_INDEX = -443636).
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
/// Maximum valid Q64.64 sqrt_price (corresponds to MAX_TICK_INDEX = 443636).
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

pub const MIN_TICK_INDEX: i32 = -443_636;
pub const MAX_TICK_INDEX: i32 = 443_636;

// ── Account byte offsets ──────────────────────────────────────────────────────
// Whirlpool account layout (Anchor-serialised, LE integers):
//   0..8    discriminator
//   8..40   whirlpools_config (Pubkey)
//   40      whirlpool_bump ([u8;1])
//   41..43  tick_spacing (u16)
//   43..45  fee_tier_index_seed ([u8;2])
//   45..47  fee_rate (u16) — hundredths of a basis point; denominator 1_000_000
//   47..49  protocol_fee_rate (u16)
//   49..65  liquidity (u128)
//   65..81  sqrt_price (u128, Q64.64)
//   81..85  tick_current_index (i32)
//   85..93  protocol_fee_owed_a (u64)
//   93..101 protocol_fee_owed_b (u64)
//  101..133 token_mint_a (Pubkey)
//  133..165 token_vault_a (Pubkey)
//  165..181 fee_growth_global_a (u128)
//  181..213 token_mint_b (Pubkey)
//  213..245 token_vault_b (Pubkey)
//   …rest   fee_growth_global_b, reward_last_updated_timestamp, reward_infos
mod offsets {
    pub const TICK_SPACING: usize = 41;
    pub const FEE_RATE: usize = 45;
    pub const LIQUIDITY: usize = 49;
    pub const SQRT_PRICE: usize = 65;
    pub const TICK_CURRENT_INDEX: usize = 81;
    pub const TOKEN_MINT_A: usize = 101;
    pub const TOKEN_VAULT_A: usize = 133;
    pub const TOKEN_MINT_B: usize = 181;
    pub const TOKEN_VAULT_B: usize = 213;
    /// Minimum account data length to read all fields we use.
    pub const MIN_LEN: usize = 245;
}

// ── Pool state ────────────────────────────────────────────────────────────────

/// Snapshot of Whirlpool state used for local quote computation.
#[derive(Debug, Clone)]
pub struct WhirlpoolPool {
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    /// Current active sqrt_price in Q64.64.
    pub sqrt_price: u128,
    /// Current active liquidity.
    pub liquidity: u128,
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    /// LP fee in parts per 1_000_000.
    pub fee_rate: u16,
    /// Q64.64 sqrt_price of the lower tick boundary of the current tick range.
    pub sqrt_price_lower: u128,
    /// Q64.64 sqrt_price of the upper tick boundary of the current tick range.
    pub sqrt_price_upper: u128,
}

/// Parse a raw Whirlpool account `data` slice into a [`WhirlpoolPool`].
/// Returns `None` if the data is too short, has zero liquidity, or the tick
/// math produces out-of-range values.
pub fn parse_pool(data: &[u8]) -> Option<WhirlpoolPool> {
    use offsets::*;
    if data.len() < MIN_LEN {
        return None;
    }

    let tick_spacing =
        u16::from_le_bytes(data[TICK_SPACING..TICK_SPACING + 2].try_into().ok()?);
    let fee_rate = u16::from_le_bytes(data[FEE_RATE..FEE_RATE + 2].try_into().ok()?);
    let liquidity =
        u128::from_le_bytes(data[LIQUIDITY..LIQUIDITY + 16].try_into().ok()?);
    let sqrt_price =
        u128::from_le_bytes(data[SQRT_PRICE..SQRT_PRICE + 16].try_into().ok()?);
    let tick_current_index =
        i32::from_le_bytes(data[TICK_CURRENT_INDEX..TICK_CURRENT_INDEX + 4].try_into().ok()?);

    if liquidity == 0 || tick_spacing == 0 {
        return None;
    }
    if sqrt_price < MIN_SQRT_PRICE_X64 || sqrt_price > MAX_SQRT_PRICE_X64 {
        return None;
    }

    // Compute current-tick range boundaries.
    let lower_tick =
        tick_current_index.div_euclid(tick_spacing as i32) * tick_spacing as i32;
    let upper_tick = lower_tick + tick_spacing as i32;
    let sqrt_price_lower = sqrt_price_from_tick_index(lower_tick)?;
    let sqrt_price_upper = sqrt_price_from_tick_index(upper_tick)?;

    let token_mint_a =
        Pubkey::from(<[u8; 32]>::try_from(&data[TOKEN_MINT_A..TOKEN_MINT_A + 32]).ok()?);
    let token_vault_a =
        Pubkey::from(<[u8; 32]>::try_from(&data[TOKEN_VAULT_A..TOKEN_VAULT_A + 32]).ok()?);
    let token_mint_b =
        Pubkey::from(<[u8; 32]>::try_from(&data[TOKEN_MINT_B..TOKEN_MINT_B + 32]).ok()?);
    let token_vault_b =
        Pubkey::from(<[u8; 32]>::try_from(&data[TOKEN_VAULT_B..TOKEN_VAULT_B + 32]).ok()?);

    Some(WhirlpoolPool {
        token_mint_a,
        token_mint_b,
        token_vault_a,
        token_vault_b,
        sqrt_price,
        liquidity,
        tick_current_index,
        tick_spacing,
        fee_rate,
        sqrt_price_lower,
        sqrt_price_upper,
    })
}

// ── Tick math ─────────────────────────────────────────────────────────────────
//
// Ported verbatim from:
//   orca-so/whirlpools programs/whirlpool/src/math/tick_math.rs
//
// All sqrt_prices are Q64.64 (integer = actual_price × 2^64).
// The positive-tick path works in Q96 internally (each constant ≈ 2^96) and
// shifts right 32 at the end; intermediate ratios can reach ~2^129, so we keep
// `ratio` as U256.  The negative-tick path stays in Q64.64 throughout (all
// constants < 2^64) and fits comfortably in u128.

/// Q64.64 sqrt_price for `tick`, or `None` if `tick` is out of range.
pub fn sqrt_price_from_tick_index(tick: i32) -> Option<u128> {
    if tick < MIN_TICK_INDEX || tick > MAX_TICK_INDEX {
        return None;
    }
    if tick >= 0 {
        Some(sqrt_price_positive_tick(tick as u32))
    } else {
        Some(sqrt_price_negative_tick((-tick) as u32))
    }
}

fn sqrt_price_positive_tick(tick: u32) -> u128 {
    // Each constant represents sqrt(1.0001^(2^i)) × 2^96.
    // Initialise ratio to 2^96 (tick bit-0 not set) or to the bit-0 constant.
    let init: u128 = if tick & 1 != 0 {
        79_232_123_823_359_799_118_286_999_567
    } else {
        79_228_162_514_264_337_593_543_950_336 // 2^96
    };
    let mut ratio = U256::from_u128(init);

    macro_rules! step {
        ($bit:expr, $c:expr) => {
            if tick & (1 << $bit) != 0 {
                // ratio = (ratio × constant) >> 96.  Both ratio and constant
                // are ≤ ~2^129 and ~2^115 respectively, so the 256-bit product
                // fits without overflow.
                ratio = ratio
                    .mul_u256_u128($c)
                    .expect("positive tick mul overflow — tick out of range")
                    .shr96();
            }
        };
    }

    step!(1,  79_236_085_330_515_764_027_303_304_731_u128);
    step!(2,  79_244_008_939_048_815_603_706_035_061_u128);
    step!(3,  79_259_858_533_276_714_757_314_932_305_u128);
    step!(4,  79_291_567_232_598_584_799_939_703_904_u128);
    step!(5,  79_355_022_692_464_371_645_785_046_466_u128);
    step!(6,  79_482_085_999_252_804_386_437_311_141_u128);
    step!(7,  79_736_823_300_114_093_921_829_183_326_u128);
    step!(8,  80_248_749_790_819_932_309_965_073_892_u128);
    step!(9,  81_282_483_887_344_747_381_513_967_011_u128);
    step!(10, 83_390_072_131_320_151_908_154_831_281_u128);
    step!(11, 87_770_609_709_833_776_024_991_924_138_u128);
    step!(12, 97_234_110_755_111_693_312_479_820_773_u128);
    step!(13, 119_332_217_159_966_728_226_237_229_890_u128);
    step!(14, 179_736_315_981_702_064_433_883_588_727_u128);
    step!(15, 407_748_233_172_238_350_107_850_275_304_u128);
    step!(16, 2_098_478_828_474_011_932_436_660_412_517_u128);
    step!(17, 55_581_415_166_113_811_149_459_800_483_533_u128);
    step!(18, 38_992_368_544_603_139_932_233_054_999_993_551_u128);

    // Convert Q96 → Q64.64 by discarding the low 32 bits.
    ratio
        .shr32_as_u128()
        .expect("positive tick shr32 overflow — tick out of range")
}

fn sqrt_price_negative_tick(abs_tick: u32) -> u128 {
    // Each constant represents sqrt(1/1.0001^(2^i)) × 2^64 ≤ 2^64.
    // All intermediate products fit in u128 (ratio × const < 2^128).
    let mut ratio: u128 = if abs_tick & 1 != 0 {
        18_445_821_805_675_392_311
    } else {
        18_446_744_073_709_551_616 // 2^64 = 1.0 in Q64.64
    };

    macro_rules! step {
        ($bit:expr, $c:expr) => {
            if abs_tick & (1 << $bit) != 0 {
                // ratio = (ratio × constant) >> 64.
                ratio = U256::mul_u128(ratio, $c).shr64().as_u128()
                    .expect("negative tick shr64 overflow — tick out of range");
            }
        };
    }

    step!(1,  18_444_899_583_751_176_498_u128);
    step!(2,  18_443_055_278_223_354_162_u128);
    step!(3,  18_439_367_220_385_604_838_u128);
    step!(4,  18_431_993_317_065_449_817_u128);
    step!(5,  18_417_254_355_718_160_513_u128);
    step!(6,  18_387_811_781_193_591_352_u128);
    step!(7,  18_329_067_761_203_520_168_u128);
    step!(8,  18_212_142_134_806_087_854_u128);
    step!(9,  17_980_523_815_641_551_639_u128);
    step!(10, 17_526_086_738_831_147_013_u128);
    step!(11, 16_651_378_430_235_024_244_u128);
    step!(12, 15_030_750_278_693_429_944_u128);
    step!(13, 12_247_334_978_882_834_399_u128);
    step!(14,  8_131_365_268_884_726_200_u128);
    step!(15,  3_584_323_654_723_342_297_u128);
    step!(16,    696_457_651_847_595_233_u128);
    step!(17,     26_294_789_957_452_057_u128);
    step!(18,         37_481_735_321_082_u128);

    ratio
}

// ── Swap math ─────────────────────────────────────────────────────────────────
//
// Exact-in swap for a single tick (no tick crossing).  All formulas ported from
// orca-so/whirlpools programs/whirlpool/src/math/token_math.rs.
//
// Fee is deducted from input BEFORE the price calculation (unlike Meteora DAMM
// v2 mode-0 where fee is on output).
//
// Rounding convention for exact-in:
//   • next_sqrt_price rounds in the direction that minimises output (protects
//     the pool): UP for A→B (A in), DOWN for B→A (B in).
//   • output amount rounds DOWN (unfixed side in exact-in).

/// Compute an exact-in quote for one Whirlpool pool.
///
/// Returns the output token amount in atomic units, or `None` on:
/// - zero liquidity / zero input
/// - arithmetic overflow (extremely large pools or amounts)
/// - tick boundary crossing (requires tick-array state we don't have)
pub fn quote_exact_in(
    amount_in: u64,
    a_for_b: bool,
    sqrt_price: u128,
    liquidity: u128,
    sqrt_price_lower: u128,
    sqrt_price_upper: u128,
    fee_rate: u16,
) -> Option<u64> {
    if liquidity == 0 || amount_in == 0 {
        return None;
    }

    // Deduct LP fee from input before computing price movement.
    // Formula: amount_after_fee = floor(amount_in × (1_000_000 - fee_rate) / 1_000_000)
    let amount_after_fee = ((amount_in as u128)
        .checked_mul(FEE_RATE_MUL_VALUE - fee_rate as u128)?
        / FEE_RATE_MUL_VALUE) as u64;
    if amount_after_fee == 0 {
        return None;
    }

    if a_for_b {
        // Token A in → token B out.  Price (sqrt_price) decreases.
        let next_sqrt = next_sqrt_from_a_round_up(sqrt_price, liquidity, amount_after_fee)?;
        // Tick-boundary guard: can't cross below current-tick's lower bound.
        if next_sqrt < sqrt_price_lower {
            return None;
        }
        amount_delta_b(sqrt_price, next_sqrt, liquidity)
    } else {
        // Token B in → token A out.  Price increases.
        let next_sqrt = next_sqrt_from_b_round_down(sqrt_price, liquidity, amount_after_fee)?;
        // Tick-boundary guard: can't cross above current-tick's upper bound.
        if next_sqrt > sqrt_price_upper {
            return None;
        }
        amount_delta_a(next_sqrt, sqrt_price, liquidity)
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// next_sqrt_price when swapping token A in (price decreases), rounded UP.
///
/// Formula (from token_math.rs):
///   numerator   = L × sqrt_P × 2^64      (Q192-ish, via U256)
///   denominator = L × 2^64 + sqrt_P × Δa (Q192-ish, via U256)
///   next_sqrt   = ⌈ numerator / denominator ⌉
fn next_sqrt_from_a_round_up(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    let product = U256::mul_u128(sqrt_price, amount as u128);

    // numerator = (L * sqrt_P) << 64
    let lp = U256::mul_u128(liquidity, sqrt_price);
    let numerator = lp.checked_shl64()?;

    // denominator = L << 64 + product
    let l_shl = U256::from_u128(liquidity).checked_shl64()?;
    let denominator = l_shl.checked_add(product)?;

    numerator.div_ceil_u128(denominator)
}

/// next_sqrt_price when swapping token B in (price increases), rounded DOWN.
///
/// Formula (from token_math.rs):
///   delta   = ⌊ amount × 2^64 / L ⌋
///   next    = sqrt_P + delta
fn next_sqrt_from_b_round_down(sqrt_price: u128, liquidity: u128, amount: u64) -> Option<u128> {
    // amount is u64 so amount << 64 fits in u128: max = (2^64-1) * 2^64 < 2^128.
    let amount_x64 = (amount as u128) << 64;
    let delta = amount_x64 / liquidity; // floor
    sqrt_price.checked_add(delta)
}

/// Token-B output for A→B swap (price decreased from `p_old` to `p_new`).
///
/// Formula: Δb = ⌊ L × (p_old − p_new) / 2^64 ⌋  (Q64.64 product >> 64)
fn amount_delta_b(p_old: u128, p_new: u128, liquidity: u128) -> Option<u64> {
    debug_assert!(p_old >= p_new, "amount_delta_b: price did not decrease");
    let delta = p_old - p_new;
    // Try exact u128 multiply first (fast path for most realistic pools).
    if let Some(product) = liquidity.checked_mul(delta) {
        let out = (product >> 64) as u64;
        Some(out)
    } else {
        // Overflow: use U256.  Result > u64::MAX means the output exceeds what
        // fits in a token account — treat as None.
        let p = U256::mul_u128(liquidity, delta);
        let out = p.shr64().as_u128()?;
        if out > u64::MAX as u128 {
            return None;
        }
        Some(out as u64)
    }
}

/// Token-A output for B→A swap (price increased from `p_old` to `p_new`).
///
/// Formula: Δa = ⌊ (L × (p_new − p_old) × 2^64) / (p_new × p_old) ⌋
///
/// Requires U256: numerator = (L × Δsqrt) << 64 reaches ~2^289 in extreme
/// cases but is None-guarded by `checked_shl64`.
fn amount_delta_a(p_new: u128, p_old: u128, liquidity: u128) -> Option<u64> {
    debug_assert!(p_new >= p_old, "amount_delta_a: price did not increase");
    let delta = p_new - p_old;
    // numerator = L * delta << 64
    let ld = U256::mul_u128(liquidity, delta);
    let numerator = ld.checked_shl64()?;
    // denominator = p_new * p_old
    let denominator = U256::mul_u128(p_new, p_old);
    let out = numerator.div_floor_u128(denominator)?;
    if out > u64::MAX as u128 {
        return None;
    }
    Some(out as u64)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── tick math ─────────────────────────────────────────────────────────────

    #[test]
    fn tick0_gives_2_pow_64() {
        // At tick 0 the real price is 1.0 ⇒ sqrt_price = 2^64.
        assert_eq!(
            sqrt_price_from_tick_index(0),
            Some(18_446_744_073_709_551_616)
        );
    }

    #[test]
    fn tick_min_gives_min_sqrt_price() {
        // MIN_TICK_INDEX ↔ MIN_SQRT_PRICE_X64.
        assert_eq!(
            sqrt_price_from_tick_index(MIN_TICK_INDEX),
            Some(MIN_SQRT_PRICE_X64)
        );
    }

    #[test]
    fn tick_max_gives_max_sqrt_price() {
        // MAX_TICK_INDEX ↔ MAX_SQRT_PRICE_X64.
        assert_eq!(
            sqrt_price_from_tick_index(MAX_TICK_INDEX),
            Some(MAX_SQRT_PRICE_X64)
        );
    }

    #[test]
    fn tick_out_of_range_returns_none() {
        assert_eq!(sqrt_price_from_tick_index(MIN_TICK_INDEX - 1), None);
        assert_eq!(sqrt_price_from_tick_index(MAX_TICK_INDEX + 1), None);
    }

    #[test]
    fn tick_math_monotone() {
        // Spot-check a handful of ticks for monotone ordering.
        let ticks = [-10_000i32, -1, 0, 1, 10_000];
        let prices: Vec<u128> = ticks
            .iter()
            .map(|&t| sqrt_price_from_tick_index(t).unwrap())
            .collect();
        for w in prices.windows(2) {
            assert!(w[0] < w[1], "tick math not monotone: {} ≥ {}", w[0], w[1]);
        }
    }

    // ── swap math helpers ─────────────────────────────────────────────────────

    const Q64: u128 = 1u128 << 64; // sqrt_price at tick 0 (price = 1.0)
    const L: u128 = 10_000; // small pool for easy hand-calculation

    // Hand-computed reference values for (L=10000, P=2^64, no fee):
    //   A→B: next_sqrt = ceil(10000 * 2^64 / 10100) ≈ 2^64 * 100/101
    //   out_b = floor(L * (P - next_sqrt) / 2^64) = floor(L * 2^64/101 / 2^64) = floor(L/101) = 99
    //   B→A: symmetrically out_a = 99 (tick 0 ⇒ balanced)

    #[test]
    fn golden_a_to_b_no_fee() {
        // Swap 100 A at price 1.0, zero fee, L=10000 → 99 B.
        let next = next_sqrt_from_a_round_up(Q64, L, 100).unwrap();
        assert!(next < Q64, "price must decrease for A→B");
        let out = amount_delta_b(Q64, next, L).unwrap();
        assert_eq!(out, 99);
    }

    #[test]
    fn golden_b_to_a_no_fee() {
        // Swap 100 B at price 1.0, zero fee, L=10000 → 99 A.
        let next = next_sqrt_from_b_round_down(Q64, L, 100).unwrap();
        assert!(next > Q64, "price must increase for B→A");
        let out = amount_delta_a(next, Q64, L).unwrap();
        assert_eq!(out, 99);
    }

    #[test]
    fn golden_a_to_b_with_fee() {
        // fee_rate=3000 → amount_after_fee = floor(100 * 997000/1e6) = 99.
        // Then 99 A → out_b = floor(L/101) with denom=10099 → 98.
        let amt_after = 100u64 * 997_000 / 1_000_000; // = 99 (floor)
        assert_eq!(amt_after, 99);
        let next = next_sqrt_from_a_round_up(Q64, L, amt_after).unwrap();
        let out = amount_delta_b(Q64, next, L).unwrap();
        // With 99 A in at L=10000: out = floor(L * delta/2^64)
        // delta = Q64 - ceil(10000*Q64/10099) = Q64 * 99/10099 + rounding
        // out ≈ floor(10000 * 99/10099) = floor(990000/10099) = 98
        assert_eq!(out, 98);
    }

    #[test]
    fn quote_exact_in_wraps_fee_and_boundary_check() {
        // Use a wide tick window so 100-unit swaps at L=10000 stay within
        // the current tick.  A single-tick window for spacing=1 only holds
        // ~0.5 atomic units of each token at L=10000, so any non-trivial
        // swap would cross it — use ±10_000 ticks instead.
        let lower = sqrt_price_from_tick_index(-10_000).unwrap();
        let upper = sqrt_price_from_tick_index(10_000).unwrap();

        // A→B: price decreases from Q64; must stay above lower bound.
        let out_ab = quote_exact_in(100, true, Q64, L, lower, upper, 0).unwrap();
        assert_eq!(out_ab, 99);

        // B→A: price increases from Q64; must stay below upper bound.
        let out_ba = quote_exact_in(100, false, Q64, L, lower, upper, 0).unwrap();
        assert_eq!(out_ba, 99);
    }

    #[test]
    fn tick_boundary_returns_none_a_to_b() {
        // An enormous A→B swap would push price below the tick lower bound.
        // Use a tight window: price already AT tick 1 (upper of the 0..1 range).
        let p1 = sqrt_price_from_tick_index(1).unwrap();
        let lower = sqrt_price_from_tick_index(1).unwrap();
        let upper = sqrt_price_from_tick_index(2).unwrap();
        // A very large input drives next_sqrt far below lower_tick=1 boundary.
        // With L=10000 and huge amount, next_sqrt → 0, well below lower.
        let result = quote_exact_in(1_000_000_000, true, p1, L, lower, upper, 0);
        assert!(result.is_none(), "expected None for tick-crossing A→B swap");
    }

    #[test]
    fn tick_boundary_returns_none_b_to_a() {
        // A large B→A swap at price=tick-0 lower bound should cross the upper.
        let lower = sqrt_price_from_tick_index(0).unwrap();
        let upper = sqrt_price_from_tick_index(1).unwrap();
        // tick spacing 1 is very narrow; large B amount crosses into tick 1.
        let result = quote_exact_in(1_000_000_000, false, Q64, L, lower, upper, 0);
        assert!(result.is_none(), "expected None for tick-crossing B→A swap");
    }

    #[test]
    fn zero_liquidity_returns_none() {
        let lower = Q64;
        let upper = sqrt_price_from_tick_index(1).unwrap();
        assert!(quote_exact_in(100, true, Q64, 0, lower, upper, 0).is_none());
    }

    #[test]
    fn zero_amount_returns_none() {
        let lower = Q64;
        let upper = sqrt_price_from_tick_index(1).unwrap();
        assert!(quote_exact_in(0, true, Q64, L, lower, upper, 0).is_none());
    }

    #[test]
    fn parse_pool_rejects_short_data() {
        assert!(parse_pool(&[0u8; 10]).is_none());
        assert!(parse_pool(&[0u8; 244]).is_none());
    }

    #[test]
    fn parse_pool_roundtrip() {
        let mut data = vec![0u8; 300];
        // tick_spacing = 64
        data[41..43].copy_from_slice(&64u16.to_le_bytes());
        // fee_rate = 3000
        data[45..47].copy_from_slice(&3000u16.to_le_bytes());
        // liquidity = 1_000_000_000_000
        data[49..65].copy_from_slice(&1_000_000_000_000u128.to_le_bytes());
        // sqrt_price = 2^64 (tick 0)
        data[65..81].copy_from_slice(&Q64.to_le_bytes());
        // tick_current_index = 0
        data[81..85].copy_from_slice(&0i32.to_le_bytes());
        // mint_a: all-zero (Pubkey::default)
        // vault_a, mint_b, vault_b: all-zero
        let pool = parse_pool(&data).unwrap();
        assert_eq!(pool.fee_rate, 3000);
        assert_eq!(pool.tick_spacing, 64);
        assert_eq!(pool.liquidity, 1_000_000_000_000);
        assert_eq!(pool.sqrt_price, Q64);
        assert_eq!(pool.tick_current_index, 0);
        // lower tick = floor(0/64)*64 = 0, upper = 64
        assert_eq!(pool.sqrt_price_lower, sqrt_price_from_tick_index(0).unwrap());
        assert_eq!(pool.sqrt_price_upper, sqrt_price_from_tick_index(64).unwrap());
    }
}
