//! Raydium CPMM — the newer constant-product market maker (`raydium-cp-swap`).
//!
//! On-chain program: `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`
//!
//! Unlike AMM v4, the trading fee is NOT fixed: it is read from the pool's
//! `AmmConfig` account. The swap deducts the fee from the input first (rounded
//! UP), then applies constant product (floor):
//!
//! ```text
//! trade_fee  = ceil(amount_in * trade_fee_rate / 1_000_000)
//! amount_in_less_fee = amount_in - trade_fee
//! amount_out = floor(amount_in_less_fee * reserve_out
//!                    / (reserve_in + amount_in_less_fee))
//! ```
//!
//! Account layout (from the on-chain program / katlogic `getDexAccounts.ts`):
//!   • Pool account: `amm_config` pubkey at byte offset 8 (after the 8-byte
//!     Anchor discriminator).
//!   • AmmConfig account: `trade_fee_rate` is a u64 little-endian at offset 12.
//!
//! Fee denominator is 1_000_000 (so e.g. trade_fee_rate = 2500 → 0.25%).

use solana_sdk::pubkey::Pubkey;

use super::{mul_div_ceil, mul_div_floor, DexCalculator, SwapQuote};

/// Raydium CPMM mainnet program id.
pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

/// CPMM fee denominator (parts-per-million).
pub const FEE_RATE_DENOMINATOR: u128 = 1_000_000;

/// Byte offset of the `amm_config` pubkey inside the CPMM pool account.
pub const POOL_AMM_CONFIG_OFFSET: usize = 8;
/// Byte offset of `trade_fee_rate` (u64 LE) inside the AmmConfig account.
pub const CONFIG_TRADE_FEE_RATE_OFFSET: usize = 12;

/// Extract the `amm_config` pubkey from a CPMM pool account's data.
pub fn parse_amm_config(pool_data: &[u8]) -> Option<Pubkey> {
    let end = POOL_AMM_CONFIG_OFFSET + 32;
    if pool_data.len() < end {
        return None;
    }
    let bytes: [u8; 32] = pool_data[POOL_AMM_CONFIG_OFFSET..end].try_into().ok()?;
    Some(Pubkey::from(bytes))
}

/// Extract `trade_fee_rate` (u64 LE) from an AmmConfig account's data.
pub fn parse_trade_fee_rate(config_data: &[u8]) -> Option<u64> {
    let end = CONFIG_TRADE_FEE_RATE_OFFSET + 8;
    if config_data.len() < end {
        return None;
    }
    let bytes: [u8; 8] = config_data[CONFIG_TRADE_FEE_RATE_OFFSET..end].try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

pub struct RaydiumCpmm {
    /// Trading fee rate (parts-per-million), read from the pool's AmmConfig.
    pub trade_fee_rate: u64,
}

impl RaydiumCpmm {
    pub fn new(trade_fee_rate: u64) -> Self {
        Self { trade_fee_rate }
    }

    /// Exact-in swap with an explicit fee rate (ppm). Free function form for
    /// the validator.
    pub fn amount_out(
        amount_in: u64,
        reserve_in: u64,
        reserve_out: u64,
        trade_fee_rate: u64,
    ) -> Option<SwapQuote> {
        if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
            return None;
        }
        let amount_in = amount_in as u128;
        let reserve_in = reserve_in as u128;
        let reserve_out = reserve_out as u128;

        // Fee rounded UP, matching the on-chain `Fees::trading_fee` ceil_div.
        let fee_in = mul_div_ceil(amount_in, trade_fee_rate as u128, FEE_RATE_DENOMINATOR)?;
        let amount_in_less_fee = amount_in.checked_sub(fee_in)?;

        let denominator = reserve_in.checked_add(amount_in_less_fee)?;
        let amount_out = mul_div_floor(amount_in_less_fee, reserve_out, denominator)?;

        Some(SwapQuote {
            amount_out: u64::try_from(amount_out).ok()?,
            fee_in: u64::try_from(fee_in).ok()?,
        })
    }
}

impl DexCalculator for RaydiumCpmm {
    fn name(&self) -> &'static str {
        "RaydiumCpmm"
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
        Self::amount_out(amount_in, reserve_in, reserve_out, self.trade_fee_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_value_quarter_percent() {
        // trade_fee_rate = 2500 ppm = 0.25%, symmetric 1e9 pool, 1e6 in.
        //   fee = ceil(1_000_000 * 2500 / 1_000_000) = 2500
        //   less_fee = 997_500
        //   out = floor(997_500 * 1_000_000_000 / (1_000_000_000 + 997_500))
        //       = floor(997_500_000_000_000 / 1_000_997_500) = 996_505
        let q = RaydiumCpmm::amount_out(1_000_000, 1_000_000_000, 1_000_000_000, 2500).unwrap();
        assert_eq!(q.fee_in, 2500);
        assert_eq!(q.amount_out, 996_505);
    }

    #[test]
    fn fee_rounds_up() {
        // amount_in * rate / 1e6 = 1*1/1e6 → ceil = 1, not 0.
        let q = RaydiumCpmm::amount_out(1, 1_000_000, 1_000_000, 1).unwrap();
        assert_eq!(q.fee_in, 1);
    }

    #[test]
    fn zero_reserves_none() {
        assert!(RaydiumCpmm::amount_out(1_000, 0, 1_000, 2500).is_none());
        assert!(RaydiumCpmm::amount_out(1_000, 1_000, 0, 2500).is_none());
    }

    #[test]
    fn parse_offsets() {
        // Build a fake pool account: 8-byte discriminator + 32-byte config pubkey.
        let cfg = Pubkey::new_unique();
        let mut pool = vec![0u8; 8];
        pool.extend_from_slice(cfg.as_ref());
        assert_eq!(parse_amm_config(&pool), Some(cfg));

        // AmmConfig: 12 bytes then u64 LE trade_fee_rate = 2500.
        let mut conf = vec![0u8; 12];
        conf.extend_from_slice(&2500u64.to_le_bytes());
        assert_eq!(parse_trade_fee_rate(&conf), Some(2500));
    }

    #[test]
    fn large_reserves_no_overflow() {
        let q = RaydiumCpmm::amount_out(
            1_000_000_000,
            9_000_000_000_000_000_000,
            5_000_000_000_000_000_000,
            2500,
        );
        assert!(q.is_some());
    }
}
