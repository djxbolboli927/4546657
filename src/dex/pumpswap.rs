//! PumpSwap — pump.fun's constant-product AMM for graduated tokens.
//!
//! Program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`
//!
//! PumpSwap is a standard x*y=k AMM. The total fee is 30 bps (LP 20 bps,
//! protocol 5 bps, creator 5 bps); fees are taken on the input token before
//! the constant-product step — identical structure to Raydium AMM v4.
//!
//! Source cross-validation:
//!   • pump-fun/pump-public-docs AMM mechanism description
//!   • Erbsensuppee/pumpfun-pumpswap-sdk (Uniswap-v2-style fee-on-input)
//!
//! Pool account layout (Anchor, 243 bytes):
//!   0..8    discriminator
//!   8       pool_bump (u8)
//!   9..11   index (u16)
//!   11..43  creator (Pubkey)
//!   43..75  base_mint (Pubkey)
//!   75..107 quote_mint (Pubkey)
//!   107..139 lp_mint (Pubkey)
//!   139..171 pool_base_token_account (Pubkey) — base-token vault
//!   171..203 pool_quote_token_account (Pubkey) — quote-token vault
//!   203..211 lp_supply (u64)
//!   211..243 coin_creator (Pubkey)
//!
//! Reserves are in external SPL token accounts, not encoded in the pool struct.

use solana_sdk::pubkey::Pubkey;

use crate::dex::mul_div_floor;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

/// Total fee in basis points: LP (20) + protocol (5) + creator (5).
const FEE_BPS: u128 = 30;
/// Fee denominator (basis points system).
const FEE_DEN: u128 = 10_000;

mod offsets {
    pub const BASE_MINT: usize = 43;
    pub const QUOTE_MINT: usize = 75;
    pub const BASE_VAULT: usize = 139;
    pub const QUOTE_VAULT: usize = 171;
    /// Minimum data length we need (through end of pool_quote_token_account).
    pub const MIN_LEN: usize = 203;
}

/// Parsed snapshot of a PumpSwap pool account.
#[derive(Debug, Clone)]
pub struct PumpPool {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// SPL token account holding the base-token reserves.
    pub base_vault: Pubkey,
    /// SPL token account holding the quote-token reserves.
    pub quote_vault: Pubkey,
}

/// Parse the fields we need from a raw PumpSwap pool account.
pub fn parse_pool(data: &[u8]) -> Option<PumpPool> {
    if data.len() < offsets::MIN_LEN {
        return None;
    }
    let base_mint = Pubkey::from(
        <[u8; 32]>::try_from(&data[offsets::BASE_MINT..offsets::BASE_MINT + 32]).ok()?,
    );
    let quote_mint = Pubkey::from(
        <[u8; 32]>::try_from(&data[offsets::QUOTE_MINT..offsets::QUOTE_MINT + 32]).ok()?,
    );
    let base_vault = Pubkey::from(
        <[u8; 32]>::try_from(&data[offsets::BASE_VAULT..offsets::BASE_VAULT + 32]).ok()?,
    );
    let quote_vault = Pubkey::from(
        <[u8; 32]>::try_from(&data[offsets::QUOTE_VAULT..offsets::QUOTE_VAULT + 32]).ok()?,
    );
    Some(PumpPool { base_mint, quote_mint, base_vault, quote_vault })
}

/// Exact-in swap quote for PumpSwap (x*y=k, 30 bps total fee on input).
///
/// Fee is folded into the numerator Uniswap-v2-style (no intermediate floor):
///
/// ```text
/// scaled     = amount_in × (FEE_DEN − FEE_BPS)   // = amount_in × 9970
/// denom      = reserve_in × FEE_DEN + scaled
/// amount_out = ⌊ reserve_out × scaled / denom ⌋
/// ```
pub fn quote_exact_in(amount_in: u64, reserve_in: u64, reserve_out: u64) -> Option<u64> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return None;
    }
    let a = amount_in as u128;
    let ri = reserve_in as u128;
    let ro = reserve_out as u128;

    let scaled = a.checked_mul(FEE_DEN - FEE_BPS)?;
    let denom = ri.checked_mul(FEE_DEN)?.checked_add(scaled)?;
    let out = mul_div_floor(ro, scaled, denom)?;
    u64::try_from(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_reserves_return_none() {
        assert!(quote_exact_in(1_000, 0, 1_000).is_none());
        assert!(quote_exact_in(1_000, 1_000, 0).is_none());
        assert!(quote_exact_in(0, 1_000, 1_000).is_none());
    }

    #[test]
    fn golden_balanced_pool_small() {
        // in=1_000, reserves=1_000_000 each:
        //   scaled = 1_000 × 9970 = 9_970_000
        //   denom  = 1_000_000 × 10_000 + 9_970_000 = 10_009_970_000
        //   out    = ⌊1_000_000 × 9_970_000 / 10_009_970_000⌋ = 996
        assert_eq!(quote_exact_in(1_000, 1_000_000, 1_000_000).unwrap(), 996);
    }

    #[test]
    fn golden_balanced_pool_large() {
        // in=100_000, reserves=1_000_000 each:
        //   scaled = 100_000 × 9970 = 997_000_000
        //   denom  = 1_000_000 × 10_000 + 997_000_000 = 10_997_000_000
        //   out    = ⌊1_000_000 × 997_000_000 / 10_997_000_000⌋
        //          = ⌊997_000_000_000_000 / 10_997_000_000⌋ = 90_661
        assert_eq!(quote_exact_in(100_000, 1_000_000, 1_000_000).unwrap(), 90_661);
    }

    #[test]
    fn output_bounded_by_reserve() {
        let out = quote_exact_in(u64::MAX / 2, 1_000, 5_000).unwrap();
        assert!(out < 5_000);
    }

    #[test]
    fn large_reserves_no_overflow() {
        // ~1.8e19 reserves — u128 arithmetic must not overflow.
        let out =
            quote_exact_in(1_000_000_000, 9_000_000_000_000_000_000, 5_000_000_000_000_000_000);
        assert!(out.is_some());
    }

    #[test]
    fn fee_30_bps_higher_than_25_bps_for_large_input() {
        // For in=100_000, r=1_000_000 the 5-bps difference becomes visible.
        // Raydium AMM v4 (25 bps) would give 90_702; PumpSwap (30 bps) gives 90_661.
        let pump = quote_exact_in(100_000, 1_000_000, 1_000_000).unwrap();
        assert_eq!(pump, 90_661);
    }
}
