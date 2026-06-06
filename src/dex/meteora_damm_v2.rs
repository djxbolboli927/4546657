//! Meteora DAMM v2 — the "cp-amm" concentrated-liquidity AMM.
//!
//! On-chain program: `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG`
//!
//! Unlike Raydium AMM v4 / CPMM, DAMM v2 is **NOT** plain `x*y=k` over vault
//! balances. It is Uniswap-v3-style: the pool keeps a single global position in
//! `[sqrt_min_price, sqrt_max_price]` and prices a swap entirely from
//! `sqrt_price` (Q64.64 fixed point, `u128`) and `liquidity` (`u128`). Vault
//! balances are **never read** for the quote — exactly like a v3 active-tick.
//!
//! Math ported verbatim from the on-chain program `MeteoraAg/damm-v2`
//! (`programs/cp-amm/src/...`) and cross-checked 1:1 against the TypeScript SDK
//! `@meteora-ag/cp-amm-sdk` (`MeteoraAg/damm-v2-sdk`, `src/math/...`), the same
//! SDK katlogic's `solana-arbitrage-bot` wraps for its `dyn2` engine. Rounding
//! directions match on-chain to the lamport (price rounds against the trader,
//! output rounds down toward the pool).
//!
//! ```text
//! FEE_DENOMINATOR = 1_000_000_000   (1e9 — NOT 1e6 like CPMM!)
//!
//! A→B (token A in):
//!   next = ceil( L * sqrt_price / (L + amount_in * sqrt_price) )   // round up
//!   require next >= sqrt_min_price
//!   out_B = floor( L * (sqrt_price - next) >> 128 )                // round down
//!
//! B→A (token B in):
//!   next = sqrt_price + floor( (amount_in << 128) / L )            // round down
//!   require next <= sqrt_max_price
//!   out_A = floor( L * (next - sqrt_price) / (sqrt_price * next) ) // round down
//!
//! fee   = ceil( amount * fee_numerator / 1e9 )                     // round up
//! ```
//!
//! Fee placement (input vs output) depends on `collect_fee_mode`:
//!   • 0 = BothToken  → fee taken from **output**, both directions.
//!   • 1 = OnlyB      → A→B fee on output, B→A fee on **input**.
//!   • 2 = Compounding → uses a *constant-product* curve, not this sqrt math.
//!     We deliberately **skip** mode 2 (return `None`) rather than mis-price it,
//!     so it can never produce a fake arbitrage signal. (Rare in practice.)
//!
//! Total fee numerator = min( base(cliff) + dynamic, max_fee[fee_version] ).
//! `max_fee` = 50% (v0) or 99% (v1).
//!
//! ── Accuracy notes (read before trusting to the lamport) ──────────────────
//! • The curve math above is stable across program versions.
//! • Account byte offsets (§ `offsets`) are from the cp-amm IDL used by the
//!   current mainnet SDK (v1.3.x); the `Pool` struct field *order* is
//!   authoritative. If a future redeploy changes `PoolFeesStruct`'s size the
//!   fee offsets (only) would shift — validate against a live pool.
//! • Base fee uses the **cliff** numerator (the schedule's maximum). If a pool
//!   runs a decaying fee scheduler the live fee is ≤ cliff, so we *over*-state
//!   the fee — this can only make us miss a real edge, never invent a fake one.
//!   The dynamic-fee component IS applied exactly when enabled.

use solana_sdk::pubkey::Pubkey;

use super::mul_div_ceil;
use super::uint256::U256;

/// Meteora DAMM v2 (cp-amm) mainnet program id.
pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");

/// Fee parts-per-billion denominator (1e9).
pub const FEE_DENOMINATOR: u128 = 1_000_000_000;

/// Max total fee numerator by `fee_version`: 50% (v0) and 99% (v1).
pub const MAX_FEE_NUMERATOR_V0: u64 = 500_000_000;
pub const MAX_FEE_NUMERATOR_V1: u64 = 990_000_000;

/// Dynamic-fee constants (verbatim from the SDK `constants.ts`).
const DYNAMIC_FEE_SCALING_FACTOR: u128 = 100_000_000_000; // 1e11
const DYNAMIC_FEE_ROUNDING_OFFSET: u128 = 99_999_999_999;

// ── Pool account byte offsets ──────────────────────────────────────────────
// 8-byte Anchor discriminator + fields, all little-endian. Derived from the
// cp-amm IDL `cp_amm.json` (current mainnet SDK). Field *order* is the
// authoritative part; see the accuracy notes above.
mod offsets {
    pub const TOKEN_A_MINT: usize = 168;
    pub const TOKEN_B_MINT: usize = 200;
    pub const TOKEN_A_VAULT: usize = 232;
    pub const TOKEN_B_VAULT: usize = 264;
    pub const LIQUIDITY: usize = 360; // u128
    pub const SQRT_MIN_PRICE: usize = 424; // u128
    pub const SQRT_MAX_PRICE: usize = 440; // u128
    pub const SQRT_PRICE: usize = 456; // u128
    pub const COLLECT_FEE_MODE: usize = 484; // u8
    pub const FEE_VERSION: usize = 486; // u8

    // PoolFeesStruct begins right after the discriminator (offset 8). Its first
    // u64 is the base-fee cliff numerator.
    pub const CLIFF_FEE_NUMERATOR: usize = 8; // u64

    // DynamicFeeStruct begins at offset 56.
    pub const DYNAMIC_FEE_INITIALIZED: usize = 56; // u8
    pub const DYNAMIC_VARIABLE_FEE_CONTROL: usize = 56 + 12; // 68, u32
    pub const DYNAMIC_BIN_STEP: usize = 56 + 16; // 72, u16
    pub const DYNAMIC_VOLATILITY_ACC: usize = 56 + 64; // 120, u128

    /// Minimum data length to read every field we need (sqrt_price ends at 472,
    /// fee_version at 487, volatility_acc ends at 136). 488 covers all.
    pub const MIN_LEN: usize = 488;
}

/// Collect-fee-mode discriminants.
pub const COLLECT_FEE_BOTH: u8 = 0;
pub const COLLECT_FEE_ONLY_B: u8 = 1;
pub const COLLECT_FEE_COMPOUNDING: u8 = 2;

// ── Little-endian readers ──────────────────────────────────────────────────

#[inline]
fn read_u128(data: &[u8], off: usize) -> Option<u128> {
    data.get(off..off + 16)
        .map(|b| u128::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn read_u64(data: &[u8], off: usize) -> Option<u64> {
    data.get(off..off + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn read_u16(data: &[u8], off: usize) -> Option<u16> {
    data.get(off..off + 2)
        .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn read_pubkey(data: &[u8], off: usize) -> Option<Pubkey> {
    data.get(off..off + 32)
        .map(|b| Pubkey::from(<[u8; 32]>::try_from(b).unwrap()))
}

/// Decoded DAMM v2 pool state — everything needed to quote a swap.
#[derive(Debug, Clone)]
pub struct MeteoraPool {
    pub token_a_mint: Pubkey,
    pub token_b_mint: Pubkey,
    pub token_a_vault: Pubkey,
    pub token_b_vault: Pubkey,
    pub sqrt_price: u128,
    pub sqrt_min_price: u128,
    pub sqrt_max_price: u128,
    pub liquidity: u128,
    pub collect_fee_mode: u8,
    pub fee_version: u8,
    /// Base (cliff) fee numerator over 1e9.
    pub base_fee_numerator: u64,
    /// Dynamic fee numerator over 1e9 (0 when the dynamic fee is disabled).
    pub dynamic_fee_numerator: u64,
}

impl MeteoraPool {
    /// Total fee numerator over 1e9, capped by `fee_version`.
    pub fn total_fee_numerator(&self) -> u64 {
        let max = if self.fee_version == 0 {
            MAX_FEE_NUMERATOR_V0
        } else {
            MAX_FEE_NUMERATOR_V1
        };
        self.base_fee_numerator
            .saturating_add(self.dynamic_fee_numerator)
            .min(max)
    }

    /// Is this pool priced by the concentrated-liquidity curve we implement?
    /// Compounding pools (mode 2) use constant-product and are not handled.
    pub fn is_supported(&self) -> bool {
        self.collect_fee_mode != COLLECT_FEE_COMPOUNDING && self.liquidity > 0 && self.sqrt_price > 0
    }
}

/// Dynamic fee numerator: `(vfc * (volAcc * binStep)^2 + 99_999_999_999) / 1e11`.
///
/// Verbatim from the SDK `getDynamicFeeNumerator`. For real pools `volAcc` is
/// bounded by `max_volatility_accumulator` (a u32) so the squared term stays
/// inside `u128`; we use saturating math so a pathological value can only
/// *raise* the fee (conservative — never a fake signal).
fn dynamic_fee_numerator(volatility_accumulator: u128, bin_step: u16, variable_fee_control: u32) -> u64 {
    if variable_fee_control == 0 {
        return 0;
    }
    let vfa_bin = volatility_accumulator.saturating_mul(bin_step as u128);
    let square = vfa_bin.saturating_mul(vfa_bin);
    let v_fee = (variable_fee_control as u128).saturating_mul(square);
    let num = v_fee.saturating_add(DYNAMIC_FEE_ROUNDING_OFFSET) / DYNAMIC_FEE_SCALING_FACTOR;
    u64::try_from(num).unwrap_or(u64::MAX)
}

/// Parse a DAMM v2 pool account into [`MeteoraPool`]. Returns `None` if the
/// data is too short or a required field can't be read.
pub fn parse_pool(data: &[u8]) -> Option<MeteoraPool> {
    if data.len() < offsets::MIN_LEN {
        return None;
    }

    let base_fee_numerator = read_u64(data, offsets::CLIFF_FEE_NUMERATOR)?;

    // Dynamic fee component — only when the struct is initialized.
    let dynamic_fee_numerator = if data.get(offsets::DYNAMIC_FEE_INITIALIZED).copied() == Some(0)
        || data.get(offsets::DYNAMIC_FEE_INITIALIZED).is_none()
    {
        0
    } else {
        let vfc = read_u32(data, offsets::DYNAMIC_VARIABLE_FEE_CONTROL)?;
        let bin_step = read_u16(data, offsets::DYNAMIC_BIN_STEP)?;
        let vol_acc = read_u128(data, offsets::DYNAMIC_VOLATILITY_ACC)?;
        dynamic_fee_numerator(vol_acc, bin_step, vfc)
    };

    Some(MeteoraPool {
        token_a_mint: read_pubkey(data, offsets::TOKEN_A_MINT)?,
        token_b_mint: read_pubkey(data, offsets::TOKEN_B_MINT)?,
        token_a_vault: read_pubkey(data, offsets::TOKEN_A_VAULT)?,
        token_b_vault: read_pubkey(data, offsets::TOKEN_B_VAULT)?,
        sqrt_price: read_u128(data, offsets::SQRT_PRICE)?,
        sqrt_min_price: read_u128(data, offsets::SQRT_MIN_PRICE)?,
        sqrt_max_price: read_u128(data, offsets::SQRT_MAX_PRICE)?,
        liquidity: read_u128(data, offsets::LIQUIDITY)?,
        collect_fee_mode: *data.get(offsets::COLLECT_FEE_MODE)?,
        fee_version: *data.get(offsets::FEE_VERSION)?,
        base_fee_numerator,
        dynamic_fee_numerator,
    })
}

/// Does the fee come off the input (vs the output) for this mode + direction?
///   • BothToken(0): always output.
///   • OnlyB(1): A→B output, B→A input.
/// (Compounding(2) is rejected before we get here.)
#[inline]
fn fees_on_input(collect_fee_mode: u8, a_for_b: bool) -> bool {
    match collect_fee_mode {
        COLLECT_FEE_BOTH => false,
        _ => !a_for_b, // OnlyB: fee on input only for B→A
    }
}

/// Exact-in swap quote. `a_for_b = true` means token A in / token B out (price
/// falls toward `sqrt_min_price`); `false` is the reverse.
///
/// Returns the output amount (atomic units) or `None` on: zero/empty inputs,
/// a price-range violation (the swap would push price outside the pool's
/// `[min,max]` band — on-chain this aborts), or arithmetic that doesn't fit.
#[allow(clippy::too_many_arguments)]
pub fn quote_exact_in(
    amount_in: u64,
    a_for_b: bool,
    sqrt_price: u128,
    liquidity: u128,
    sqrt_min_price: u128,
    sqrt_max_price: u128,
    fee_numerator: u64,
    collect_fee_mode: u8,
) -> Option<u64> {
    if amount_in == 0 || liquidity == 0 || sqrt_price == 0 {
        return None;
    }
    if collect_fee_mode == COLLECT_FEE_COMPOUNDING {
        return None; // constant-product curve — not modelled here
    }

    let on_input = fees_on_input(collect_fee_mode, a_for_b);

    // Fee on input (if applicable): taken before the curve, rounded UP.
    let actual_in: u128 = if on_input {
        let fee = mul_div_ceil(amount_in as u128, fee_numerator as u128, FEE_DENOMINATOR)?;
        (amount_in as u128).checked_sub(fee)?
    } else {
        amount_in as u128
    };
    if actual_in == 0 {
        return None;
    }

    let out: u128 = if a_for_b {
        // next = ceil( L * sqrt_price / (L + actual_in * sqrt_price) )
        let product = U256::mul_u128(actual_in, sqrt_price);
        let denom = U256::from_u128(liquidity).checked_add(product)?;
        let numerator = U256::mul_u128(liquidity, sqrt_price);
        let next = numerator.div_ceil_u128(denom)?;
        if next < sqrt_min_price {
            return None; // price-range violation
        }
        // out_B = floor( L * (sqrt_price - next) >> 128 )
        let delta = sqrt_price.checked_sub(next)?;
        U256::mul_u128(liquidity, delta).shr128().as_u128()?
    } else {
        // next = sqrt_price + floor( (actual_in << 128) / L )
        let quotient = U256::shl128_u64(u64::try_from(actual_in).ok()?)
            .div_floor_u128(U256::from_u128(liquidity))?;
        let next = sqrt_price.checked_add(quotient)?;
        if next > sqrt_max_price {
            return None; // price-range violation
        }
        // out_A = floor( L * (next - sqrt_price) / (sqrt_price * next) )
        let delta = next.checked_sub(sqrt_price)?;
        let numerator = U256::mul_u128(liquidity, delta);
        let denom = U256::mul_u128(sqrt_price, next);
        numerator.div_floor_u128(denom)?
    };

    // Fee on output (if applicable): taken after the curve, rounded UP.
    let out = if on_input {
        out
    } else {
        let fee = mul_div_ceil(out, fee_numerator as u128, FEE_DENOMINATOR)?;
        out.checked_sub(fee)?
    };

    u64::try_from(out).ok()
}

/// Convenience wrapper that quotes straight from a decoded [`MeteoraPool`],
/// using its capped total fee numerator. Returns `None` for unsupported
/// (compounding / empty) pools.
pub fn quote_pool(pool: &MeteoraPool, amount_in: u64, a_for_b: bool) -> Option<u64> {
    if !pool.is_supported() {
        return None;
    }
    quote_exact_in(
        amount_in,
        a_for_b,
        pool.sqrt_price,
        pool.liquidity,
        pool.sqrt_min_price,
        pool.sqrt_max_price,
        pool.total_fee_numerator(),
        pool.collect_fee_mode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q64: u128 = 1u128 << 64;
    const SQRT_MIN: u128 = 4_295_048_016;
    const SQRT_MAX: u128 = 79_226_673_521_066_979_257_578_248_091;

    // Golden values from an independent Python reference implementing the
    // verbatim on-chain formula (see commit message / PR). L = 2^100, price 1.0
    // (sqrt_price = 2^64), wide range, fee mode 0 (fee on output) unless noted.
    const L100: u128 = 1u128 << 100;

    #[test]
    fn golden_mode0_b_to_a_quarter_pct() {
        // 0.25% fee (2_500_000 / 1e9), B→A, 1_000_000 in → 997_485 out.
        let out = quote_exact_in(1_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 2_500_000, 0);
        assert_eq!(out, Some(997_485));
    }

    #[test]
    fn golden_mode0_a_to_b_quarter_pct() {
        // Symmetric at price 1.0: A→B matches B→A → 997_485.
        let out = quote_exact_in(1_000_000, true, Q64, L100, SQRT_MIN, SQRT_MAX, 2_500_000, 0);
        assert_eq!(out, Some(997_485));
    }

    #[test]
    fn golden_zero_fee_both_directions() {
        // No fee: pure curve slippage. 1_000_000 in → 999_985 out, both ways.
        assert_eq!(
            quote_exact_in(1_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 0, 0),
            Some(999_985)
        );
        assert_eq!(
            quote_exact_in(1_000_000, true, Q64, L100, SQRT_MIN, SQRT_MAX, 0, 0),
            Some(999_985)
        );
    }

    #[test]
    fn golden_mode1_fee_on_input() {
        // OnlyB mode, B→A → fee taken on INPUT. 1% fee, 5_000_000 in → 4_949_643.
        let out = quote_exact_in(5_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 10_000_000, 1);
        assert_eq!(out, Some(4_949_643));
    }

    #[test]
    fn golden_skewed_price_4x() {
        // sqrt_price = 2*2^64 → price 4.0, L = 2^96, zero fee, 1000 in.
        let sp = Q64 * 2;
        let l = 1u128 << 96;
        // A→B: 1000 token A → ~4× token B = 3999.
        assert_eq!(
            quote_exact_in(1000, true, sp, l, SQRT_MIN, SQRT_MAX, 0, 0),
            Some(3999)
        );
        // B→A: 1000 token B → ~1/4 token A = 249.
        assert_eq!(
            quote_exact_in(1000, false, sp, l, SQRT_MIN, SQRT_MAX, 0, 0),
            Some(249)
        );
    }

    #[test]
    fn compounding_mode_rejected() {
        // Mode 2 must never be priced with this curve.
        assert_eq!(
            quote_exact_in(1_000_000, true, Q64, L100, SQRT_MIN, SQRT_MAX, 2_500_000, 2),
            None
        );
    }

    #[test]
    fn empty_inputs_none() {
        assert!(quote_exact_in(0, true, Q64, L100, SQRT_MIN, SQRT_MAX, 0, 0).is_none());
        assert!(quote_exact_in(1000, true, Q64, 0, SQRT_MIN, SQRT_MAX, 0, 0).is_none());
        assert!(quote_exact_in(1000, true, 0, L100, SQRT_MIN, SQRT_MAX, 0, 0).is_none());
    }

    #[test]
    fn fee_only_reduces_output() {
        // More fee → less output, monotonic.
        let no_fee = quote_exact_in(1_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 0, 0).unwrap();
        let small = quote_exact_in(1_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 1_000_000, 0).unwrap();
        let big = quote_exact_in(1_000_000, false, Q64, L100, SQRT_MIN, SQRT_MAX, 50_000_000, 0).unwrap();
        assert!(no_fee > small && small > big, "{no_fee} > {small} > {big}");
    }

    #[test]
    fn dynamic_fee_disabled_is_zero() {
        assert_eq!(dynamic_fee_numerator(1234, 10, 0), 0);
    }

    #[test]
    fn dynamic_fee_matches_formula() {
        // (vfc * (volAcc*binStep)^2 + 99_999_999_999) / 1e11
        // vfc=100, volAcc=1000, binStep=10 → (1000*10)^2 = 1e8;
        // 100*1e8 = 1e10; (1e10 + 99_999_999_999)/1e11 = 109_999_999_999/1e11 = 1.
        assert_eq!(dynamic_fee_numerator(1000, 10, 100), 1);
    }

    #[test]
    fn total_fee_capped_by_version() {
        let mut pool = sample_pool();
        pool.fee_version = 0;
        pool.base_fee_numerator = 900_000_000; // 90%
        pool.dynamic_fee_numerator = 0;
        assert_eq!(pool.total_fee_numerator(), MAX_FEE_NUMERATOR_V0); // capped to 50%
        pool.fee_version = 1;
        assert_eq!(pool.total_fee_numerator(), 900_000_000); // under 99% cap
    }

    #[test]
    fn parse_roundtrip() {
        // Build a synthetic pool account and confirm every field decodes.
        let mut data = vec![0u8; 1112];
        let a_mint = Pubkey::new_unique();
        let b_mint = Pubkey::new_unique();
        let a_vault = Pubkey::new_unique();
        let b_vault = Pubkey::new_unique();
        data[offsets::CLIFF_FEE_NUMERATOR..offsets::CLIFF_FEE_NUMERATOR + 8]
            .copy_from_slice(&2_500_000u64.to_le_bytes());
        data[offsets::DYNAMIC_FEE_INITIALIZED] = 0; // dynamic disabled
        data[offsets::TOKEN_A_MINT..offsets::TOKEN_A_MINT + 32].copy_from_slice(a_mint.as_ref());
        data[offsets::TOKEN_B_MINT..offsets::TOKEN_B_MINT + 32].copy_from_slice(b_mint.as_ref());
        data[offsets::TOKEN_A_VAULT..offsets::TOKEN_A_VAULT + 32].copy_from_slice(a_vault.as_ref());
        data[offsets::TOKEN_B_VAULT..offsets::TOKEN_B_VAULT + 32].copy_from_slice(b_vault.as_ref());
        data[offsets::LIQUIDITY..offsets::LIQUIDITY + 16].copy_from_slice(&L100.to_le_bytes());
        data[offsets::SQRT_MIN_PRICE..offsets::SQRT_MIN_PRICE + 16]
            .copy_from_slice(&SQRT_MIN.to_le_bytes());
        data[offsets::SQRT_MAX_PRICE..offsets::SQRT_MAX_PRICE + 16]
            .copy_from_slice(&SQRT_MAX.to_le_bytes());
        data[offsets::SQRT_PRICE..offsets::SQRT_PRICE + 16].copy_from_slice(&Q64.to_le_bytes());
        data[offsets::COLLECT_FEE_MODE] = 0;
        data[offsets::FEE_VERSION] = 1;

        let p = parse_pool(&data).expect("parse");
        assert_eq!(p.token_a_mint, a_mint);
        assert_eq!(p.token_b_mint, b_mint);
        assert_eq!(p.token_a_vault, a_vault);
        assert_eq!(p.token_b_vault, b_vault);
        assert_eq!(p.liquidity, L100);
        assert_eq!(p.sqrt_price, Q64);
        assert_eq!(p.sqrt_min_price, SQRT_MIN);
        assert_eq!(p.sqrt_max_price, SQRT_MAX);
        assert_eq!(p.base_fee_numerator, 2_500_000);
        assert_eq!(p.dynamic_fee_numerator, 0);
        assert_eq!(p.collect_fee_mode, 0);
        assert_eq!(p.fee_version, 1);

        // quote_pool agrees with the explicit golden value.
        assert_eq!(quote_pool(&p, 1_000_000, false), Some(997_485));
    }

    #[test]
    fn parse_too_short_none() {
        assert!(parse_pool(&[0u8; 100]).is_none());
    }

    fn sample_pool() -> MeteoraPool {
        MeteoraPool {
            token_a_mint: Pubkey::new_unique(),
            token_b_mint: Pubkey::new_unique(),
            token_a_vault: Pubkey::new_unique(),
            token_b_vault: Pubkey::new_unique(),
            sqrt_price: Q64,
            sqrt_min_price: SQRT_MIN,
            sqrt_max_price: SQRT_MAX,
            liquidity: L100,
            collect_fee_mode: 0,
            fee_version: 1,
            base_fee_numerator: 2_500_000,
            dynamic_fee_numerator: 0,
        }
    }
}
