//! Raydium AMM v4 — the classic OpenBook constant-product AMM.
//!
//! On-chain program: `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`
//!
//! Swap math ported from the on-chain program (`raydium-amm`, `math.rs`,
//! `Calculator::swap_token_amount_base_in`). The fee is folded directly into
//! the constant-product numerator (Uniswap-style fee-on-input), and every
//! division floors:
//!
//! ```text
//! amount_in_with_fee = amount_in * (FEE_DEN - FEE_NUM)          // *9975
//! amount_out = reserve_out * amount_in_with_fee
//!              / (reserve_in * FEE_DEN + amount_in_with_fee)     // floor
//! ```
//!
//! Fee = 25 bps (numerator 25, denominator 10000) — fixed on the program,
//! NOT read from a config account (unlike CPMM). This matches Raydium's
//! documented 0.25% swap fee.

use solana_sdk::pubkey::Pubkey;

use super::{mul_div_floor, DexCalculator, SwapQuote};

/// Raydium AMM v4 mainnet program id.
pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

/// Swap fee numerator (25) and denominator (10000) → 0.25%.
pub const FEE_NUMERATOR: u128 = 25;
pub const FEE_DENOMINATOR: u128 = 10_000;

pub struct RaydiumAmmV4;

impl RaydiumAmmV4 {
    pub fn new() -> Self {
        Self
    }

    /// Exact-in swap. Free function form so the validator can call it without
    /// constructing the engine.
    pub fn amount_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> Option<SwapQuote> {
        if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
            return None;
        }
        let amount_in = amount_in as u128;
        let reserve_in = reserve_in as u128;
        let reserve_out = reserve_out as u128;

        // fee charged on input (floor), reported for transparency
        let fee_in = (amount_in * FEE_NUMERATOR) / FEE_DENOMINATOR;

        let amount_in_with_fee = amount_in.checked_mul(FEE_DENOMINATOR - FEE_NUMERATOR)?;
        let denominator = reserve_in
            .checked_mul(FEE_DENOMINATOR)?
            .checked_add(amount_in_with_fee)?;
        let amount_out = mul_div_floor(reserve_out, amount_in_with_fee, denominator)?;

        Some(SwapQuote {
            amount_out: u64::try_from(amount_out).ok()?,
            fee_in: u64::try_from(fee_in).ok()?,
        })
    }
}

impl DexCalculator for RaydiumAmmV4 {
    fn name(&self) -> &'static str {
        "RaydiumAmmV4"
    }
    fn program_id(&self) -> Pubkey {
        PROGRAM_ID
    }
    fn quote_exact_in(
        &self,
        amount_in: u64,
        reserve_in: u64,
        reserve_out: u64,
    ) -> Option<SwapQuote> {
        Self::amount_out(amount_in, reserve_in, reserve_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_reserves_return_none() {
        assert!(RaydiumAmmV4::amount_out(1_000, 0, 1_000).is_none());
        assert!(RaydiumAmmV4::amount_out(1_000, 1_000, 0).is_none());
        assert!(RaydiumAmmV4::amount_out(0, 1_000, 1_000).is_none());
    }

    #[test]
    fn known_value_balanced_pool() {
        // Symmetric pool, 1000 in. With 0.25% fee:
        //   in_with_fee = 1000 * 9975 = 9_975_000
        //   den = 1_000_000 * 10000 + 9_975_000 = 10_009_975_000
        //   out = 1_000_000 * 9_975_000 / 10_009_975_000 = 996  (floor)
        let q = RaydiumAmmV4::amount_out(1_000, 1_000_000, 1_000_000).unwrap();
        assert_eq!(q.amount_out, 996);
        assert_eq!(q.fee_in, 2); // floor(1000*25/10000)=2
    }

    #[test]
    fn output_never_exceeds_reserve() {
        // Even a huge input can't drain more than reserve_out.
        let q = RaydiumAmmV4::amount_out(u64::MAX / 2, 1_000, 5_000).unwrap();
        assert!(q.amount_out < 5_000);
    }

    #[test]
    fn large_reserves_no_overflow() {
        // ~1.8e19 reserves, large input — u128 math must not overflow.
        let q = RaydiumAmmV4::amount_out(1_000_000_000, 9_000_000_000_000_000_000, 5_000_000_000_000_000_000);
        assert!(q.is_some());
    }
}
