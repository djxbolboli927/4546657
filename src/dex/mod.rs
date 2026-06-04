//! Per-DEX swap calculators.
//!
//! Each DEX gets its own engine implementing [`DexCalculator`]. The bot needs
//! ~8 DEX engines eventually; this module is the home for all of them so the
//! validator and (later) the live arbitrage path can compute an exact-in quote
//! locally from PoolStateStore reserves — no SDK, no RPC round-trip.
//!
//! Reference: the formulas here are ported from each DEX's on-chain program
//! (NOT from katlogic, which only wraps the vendor TypeScript SDKs and contains
//! no raw math). Each engine documents its source program and is unit-tested
//! against known-good values.
//!
//! Phase 1 shipped the two pure constant-product engines:
//!   • Raydium AMM v4  (`raydium_amm_v4`)
//!   • Raydium CPMM    (`raydium_cpmm`)
//! Phase 2 adds concentrated-liquidity engines:
//!   • Meteora DAMM v2 (`meteora_damm_v2`) — Uniswap-v3-style sqrt_price + L,
//!     priced from the pool account (not vault reserves), with its own 256-bit
//!     math helper (`uint256`).
//!   • Orca Whirlpool (`whirlpool`) — Uniswap-v3-style CLMM; single-tick quotes
//!     with tick-boundary safety (returns None when crossing would require
//!     tick-array state).
//! Remaining DEXes (Raydium CLMM, Meteora DLMM, …) get their own engines later.

pub mod meteora_damm_v2;
pub mod raydium_amm_v4;
pub mod raydium_cpmm;
pub mod uint256;
pub mod whirlpool;

use solana_sdk::pubkey::Pubkey;

/// Result of an exact-in swap quote computed locally from pool reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapQuote {
    /// Output amount in the destination token's atomic units (floor-rounded,
    /// matching on-chain integer math).
    pub amount_out: u64,
    /// Trading fee charged on the input, in the input token's atomic units.
    pub fee_in: u64,
}

/// A constant-product (x*y=k) DEX swap engine.
///
/// `reserve_in` / `reserve_out` are raw vault balances (atomic units) for the
/// input and output sides respectively. `amount_in` is atomic units of input.
/// Returns `None` on empty reserves or arithmetic overflow.
pub trait DexCalculator {
    /// Human-readable engine name (for logs).
    fn name(&self) -> &'static str;

    /// The on-chain program id whose pools this engine handles.
    fn program_id(&self) -> Pubkey;

    /// Compute an exact-in swap. All math uses u128 intermediates and rounds
    /// exactly as the on-chain program does.
    fn quote_exact_in(
        &self,
        amount_in: u64,
        reserve_in: u64,
        reserve_out: u64,
    ) -> Option<SwapQuote>;
}

/// `ceil(a * b / c)` in u128 with no precision loss. Used by DEXes that round
/// the trading fee UP (e.g. Raydium CPMM).
#[inline]
pub fn mul_div_ceil(a: u128, b: u128, c: u128) -> Option<u128> {
    if c == 0 {
        return None;
    }
    let prod = a.checked_mul(b)?;
    Some((prod + (c - 1)) / c)
}

/// `floor(a * b / c)` in u128. Used for the constant-product output amount.
#[inline]
pub fn mul_div_floor(a: u128, b: u128, c: u128) -> Option<u128> {
    if c == 0 {
        return None;
    }
    a.checked_mul(b).map(|p| p / c)
}
