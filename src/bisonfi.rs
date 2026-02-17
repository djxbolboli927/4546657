/// BisonFi WSOL/USDC AMM pool parser and math
///
/// BisonFi is a constant-product AMM (x*y=k).
/// Pool: 51FQwjrvo8J8zXUaKyAznJ5NYpoiTCuqAqCu3HAMB9NZ
/// Program: BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
///
/// Account layout (Anchor-based, 8-byte discriminator prefix):
///   [0..8]   = discriminator
///   [8..40]  = token_a_mint (WSOL)
///   [40..72] = token_b_mint (USDC)
///   [72..80] = reserve_a (WSOL, u64 LE)
///   [80..88] = reserve_b (USDC, u64 LE)
///   [88..90] = fee_bps (u16 LE)
///
/// NOTE: The actual offsets above are PLACEHOLDER — run calibrate() at startup
/// to discover the correct offsets from live account data.

use anyhow::{anyhow, Result};
use log::{debug, warn};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const BISONFI_PROGRAM_ID: &str = "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi";
pub const BISONFI_POOL_ADDRESS: &str = "51FQwjrvo8J8zXUaKyAznJ5NYpoiTCuqAqCu3HAMB9NZ";

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Fee for BisonFi: ~0.3% (30 bps). Update once confirmed.
pub const BISONFI_FEE_BPS: u64 = 30;
pub const FEE_DENOM: u64 = 10_000;

/// Byte offsets within the BisonFi pool account data.
/// These are discovered via calibrate() at startup.
#[derive(Debug, Clone, Copy)]
pub struct BisonFiLayout {
    pub wsol_reserve_offset: usize,
    pub usdc_reserve_offset: usize,
    pub fee_bps_offset: Option<usize>,
}

impl Default for BisonFiLayout {
    fn default() -> Self {
        // Try common Anchor AMM layout: discriminator(8) + mint_a(32) + mint_b(32) + reserve_a(8) + reserve_b(8)
        Self {
            wsol_reserve_offset: 72,
            usdc_reserve_offset: 80,
            fee_bps_offset: Some(88),
        }
    }
}

/// Current state of the BisonFi WSOL/USDC pool.
#[derive(Debug, Clone)]
pub struct BisonFiPool {
    pub wsol_reserve: u64,
    pub usdc_reserve: u64,
    pub fee_bps: u64,
    pub layout: BisonFiLayout,
}

impl BisonFiPool {
    /// Parse pool state from raw account data using the given layout.
    pub fn parse(data: &[u8], layout: &BisonFiLayout) -> Option<Self> {
        if data.len() < layout.usdc_reserve_offset + 8 {
            warn!(
                "BisonFi account data too short: {} bytes (need {})",
                data.len(),
                layout.usdc_reserve_offset + 8
            );
            return None;
        }

        let wsol_reserve = u64::from_le_bytes(
            data[layout.wsol_reserve_offset..layout.wsol_reserve_offset + 8]
                .try_into()
                .ok()?,
        );
        let usdc_reserve = u64::from_le_bytes(
            data[layout.usdc_reserve_offset..layout.usdc_reserve_offset + 8]
                .try_into()
                .ok()?,
        );

        // Read fee_bps if offset is provided and data is long enough
        let fee_bps = if let Some(off) = layout.fee_bps_offset {
            if data.len() >= off + 2 {
                u16::from_le_bytes(data[off..off + 2].try_into().ok()?) as u64
            } else {
                BISONFI_FEE_BPS
            }
        } else {
            BISONFI_FEE_BPS
        };

        // Sanity check: reserves must be non-zero
        if wsol_reserve == 0 || usdc_reserve == 0 {
            warn!(
                "BisonFi pool has zero reserve at offsets w={} u={}: wsol={} usdc={}",
                layout.wsol_reserve_offset, layout.usdc_reserve_offset,
                wsol_reserve, usdc_reserve
            );
            return None;
        }

        Some(Self {
            wsol_reserve,
            usdc_reserve,
            fee_bps,
            layout: *layout,
        })
    }

    /// Spot price: USDC per lamport of WSOL (scaled by 10^6 for USDC decimals).
    /// Returns the price as a float: how many USDC micro-units per WSOL lamport.
    pub fn spot_price_usdc_per_wsol(&self) -> f64 {
        // WSOL: 9 decimals, USDC: 6 decimals
        // spot_price = (usdc_reserve / 10^6) / (wsol_reserve / 10^9)
        //            = usdc_reserve * 10^3 / wsol_reserve
        (self.usdc_reserve as f64 * 1_000.0) / self.wsol_reserve as f64
    }

    /// Quote: how many USDC (micro-units, 6 decimals) you get for `wsol_in` lamports.
    /// Direction: WSOL → USDC (selling WSOL into the pool)
    pub fn quote_wsol_to_usdc(&self, wsol_in: u64) -> u64 {
        amm_quote(wsol_in, self.wsol_reserve, self.usdc_reserve, self.fee_bps)
    }

    /// Quote: how many WSOL lamports you get for `usdc_in` micro-units.
    /// Direction: USDC → WSOL (buying WSOL from the pool)
    pub fn quote_usdc_to_wsol(&self, usdc_in: u64) -> u64 {
        amm_quote(usdc_in, self.usdc_reserve, self.wsol_reserve, self.fee_bps)
    }

    /// Calculate the optimal WSOL amount to arbitrage between BisonFi and Tessera V.
    ///
    /// Strategy:
    ///   Direction A (BisonFi price > oracle):
    ///     WSOL → Tessera V → USDC → BisonFi → WSOL
    ///     Optimal input Δw = (sqrt(Rw * Ru * p * f) - Rw) / f
    ///     where p = tessera_rate (USDC per WSOL lamport, net of Tessera fee)
    ///           f = 1 - fee_bps/10000 (BisonFi fee multiplier)
    ///
    ///   Direction B (Tessera V price > oracle):
    ///     WSOL → BisonFi → USDC → Tessera V → WSOL
    ///     Optimal input Δw = (sqrt(Ru * Rw * q * f) - Rw) / f
    ///     where q = 1 / tessera_rate * (1 - tessera_fee) (effective USDC→WSOL rate)
    ///
    /// Returns (direction, optimal_wsol_in, expected_wsol_out).
    pub fn find_optimal_arbitrage(
        &self,
        tessera_usdc_per_wsol: f64, // USDC per WSOL lamport from oracle
        tessera_fee_bps: u64,
    ) -> Option<ArbitrageOpportunity> {
        let rw = self.wsol_reserve as f64;
        let ru = self.usdc_reserve as f64;
        let f = 1.0 - (self.fee_bps as f64 / FEE_DENOM as f64);
        let ft = 1.0 - (tessera_fee_bps as f64 / FEE_DENOM as f64);

        // BisonFi spot price (USDC per WSOL lamport)
        let bisonfi_price = ru / rw;

        // Tessera V effective rate (USDC per WSOL lamport, after fee)
        let tessera_rate = tessera_usdc_per_wsol * ft;

        // Check Direction A: WSOL → Tessera V → USDC → BisonFi → WSOL
        // Profitable when BisonFi price > Tessera V price (WSOL is expensive on BisonFi)
        if bisonfi_price > tessera_rate {
            // Effective USDC → WSOL rate on BisonFi:
            //   effective rate q = f / (ru/rw) ≈ f * rw / ru
            // For each WSOL of input, Tessera V gives tessera_rate USDC.
            // Those USDC go into BisonFi: wsol_out = rw * usdc_in * f / (ru + usdc_in * f)
            // Let usdc_in = Δw * tessera_rate
            // wsol_out = rw * Δw * tessera_rate * f / (ru + Δw * tessera_rate * f)
            // profit(Δw) = wsol_out - Δw
            // optimal: Δw = (sqrt(rw * ru * tessera_rate * f) - ru) / (tessera_rate * f)
            let pf = tessera_rate * f;
            let optimal_wsol_in_f = (f64::sqrt(rw * ru * pf) - ru) / pf;

            if optimal_wsol_in_f > 0.0 {
                let optimal_wsol_in = optimal_wsol_in_f as u64;
                let usdc_from_tessera =
                    (optimal_wsol_in as f64 * tessera_usdc_per_wsol * ft) as u64;
                let wsol_out_from_bisonfi = amm_quote(
                    usdc_from_tessera,
                    self.usdc_reserve,
                    self.wsol_reserve,
                    self.fee_bps,
                );

                if wsol_out_from_bisonfi > optimal_wsol_in {
                    let gross_profit = wsol_out_from_bisonfi - optimal_wsol_in;
                    return Some(ArbitrageOpportunity {
                        direction: ArbDirection::WsolTesseraUsdcBisonfi,
                        wsol_input: optimal_wsol_in,
                        expected_wsol_output: wsol_out_from_bisonfi,
                        gross_profit_lamports: gross_profit,
                    });
                }
            }
        }

        // Check Direction B: WSOL → BisonFi → USDC → Tessera V → WSOL
        // Profitable when Tessera V price > BisonFi price (WSOL is cheap on BisonFi)
        if tessera_rate > bisonfi_price * f {
            // wsol_out = usdc_from_bisonfi / tessera_usdc_per_wsol * ft
            // usdc_from_bisonfi = ru * Δw * f / (rw + Δw * f)
            // wsol_out = ru * Δw * f / (rw + Δw * f) / tessera_usdc_per_wsol * ft
            // Let q = ft / tessera_usdc_per_wsol (effective USDC → WSOL rate at Tessera V)
            // wsol_out = ru * Δw * f * q / (rw + Δw * f)
            // profit(Δw) = wsol_out - Δw
            // optimal: Δw = (sqrt(ru * rw * f * q) - rw) / f
            let q = ft / tessera_usdc_per_wsol;
            let optimal_wsol_in_f = (f64::sqrt(ru * rw * f * q) - rw) / f;

            if optimal_wsol_in_f > 0.0 {
                let optimal_wsol_in = optimal_wsol_in_f as u64;
                let usdc_from_bisonfi = amm_quote(
                    optimal_wsol_in,
                    self.wsol_reserve,
                    self.usdc_reserve,
                    self.fee_bps,
                );
                let wsol_from_tessera = (usdc_from_bisonfi as f64 * q) as u64;

                if wsol_from_tessera > optimal_wsol_in {
                    let gross_profit = wsol_from_tessera - optimal_wsol_in;
                    return Some(ArbitrageOpportunity {
                        direction: ArbDirection::WsolBisonfiUsdcTessera,
                        wsol_input: optimal_wsol_in,
                        expected_wsol_output: wsol_from_tessera,
                        gross_profit_lamports: gross_profit,
                    });
                }
            }
        }

        None
    }
}

/// Constant-product AMM quote: output for given input.
/// amount_out = reserve_out * amount_in * (1-fee) / (reserve_in + amount_in * (1-fee))
fn amm_quote(amount_in: u64, reserve_in: u64, reserve_out: u64, fee_bps: u64) -> u64 {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 {
        return 0;
    }
    let amount_in_with_fee = amount_in * (FEE_DENOM - fee_bps);
    let numerator = amount_in_with_fee as u128 * reserve_out as u128;
    let denominator = reserve_in as u128 * FEE_DENOM as u128 + amount_in_with_fee as u128;
    (numerator / denominator) as u64
}

/// Arbitrage direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbDirection {
    /// WSOL → Tessera V → USDC → BisonFi → WSOL
    WsolTesseraUsdcBisonfi,
    /// WSOL → BisonFi → USDC → Tessera V → WSOL
    WsolBisonfiUsdcTessera,
}

impl ArbDirection {
    pub fn first_dex(&self) -> &'static str {
        match self {
            ArbDirection::WsolTesseraUsdcBisonfi => "TesseraV",
            ArbDirection::WsolBisonfiUsdcTessera => "BisonFi",
        }
    }
    pub fn second_dex(&self) -> &'static str {
        match self {
            ArbDirection::WsolTesseraUsdcBisonfi => "BisonFi",
            ArbDirection::WsolBisonfiUsdcTessera => "TesseraV",
        }
    }
}

/// A detected arbitrage opportunity.
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub direction: ArbDirection,
    pub wsol_input: u64,
    pub expected_wsol_output: u64,
    pub gross_profit_lamports: u64,
}

/// Scan account data for plausible reserve values by cross-referencing with
/// the actual token vault balances. Returns a calibrated BisonFiLayout.
///
/// Call this once at startup after fetching the pool account and vault balances.
pub fn calibrate_layout(
    pool_data: &[u8],
    expected_wsol_reserve: u64,
    expected_usdc_reserve: u64,
) -> Result<BisonFiLayout> {
    // Try all 8-byte-aligned offsets from 8 to data.len()-16
    let mut wsol_offset = None;
    let mut usdc_offset = None;

    for off in (8..pool_data.len().saturating_sub(8)).step_by(1) {
        if off + 8 > pool_data.len() {
            break;
        }
        let val = u64::from_le_bytes(pool_data[off..off + 8].try_into().unwrap());
        // Allow ±1% tolerance for the reserve values
        if is_close(val, expected_wsol_reserve, 0.01) && wsol_offset.is_none() {
            debug!("BisonFi calibrate: wsol_reserve found at offset {off} (val={val})");
            wsol_offset = Some(off);
        }
        if is_close(val, expected_usdc_reserve, 0.01) && usdc_offset.is_none() {
            debug!("BisonFi calibrate: usdc_reserve found at offset {off} (val={val})");
            usdc_offset = Some(off);
        }
    }

    match (wsol_offset, usdc_offset) {
        (Some(w), Some(u)) => Ok(BisonFiLayout {
            wsol_reserve_offset: w,
            usdc_reserve_offset: u,
            fee_bps_offset: None,
        }),
        _ => Err(anyhow!(
            "Could not calibrate BisonFi layout: wsol_offset={:?} usdc_offset={:?} \
            (expected wsol={expected_wsol_reserve} usdc={expected_usdc_reserve}). \
            Check vault account subscriptions.",
            wsol_offset, usdc_offset
        )),
    }
}

fn is_close(val: u64, target: u64, tolerance: f64) -> bool {
    if target == 0 {
        return val == 0;
    }
    let diff = if val > target { val - target } else { target - val };
    (diff as f64) / (target as f64) <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amm_quote_basic() {
        // Pool: 100 WSOL, 15000 USDC, fee=30bps
        let out = amm_quote(1_000_000_000, 100_000_000_000, 15_000_000_000, 30);
        // roughly 150 USDC * (1 - 0.003) ≈ 149.55 USDC
        assert!(out > 148_000_000 && out < 151_000_000, "quote={out}");
    }

    #[test]
    fn test_optimal_arb_direction_b() {
        // BisonFi: 100 WSOL = 15000 USDC → price = 150 USDC/WSOL
        // Tessera V oracle: 155 USDC/WSOL → Tessera V is more expensive
        // → Direction B: WSOL→BisonFi→USDC→TesseraV
        let pool = BisonFiPool {
            wsol_reserve: 100 * LAMPORTS_PER_SOL,
            usdc_reserve: 15_000_000_000, // 15000 USDC
            fee_bps: 30,
            layout: BisonFiLayout::default(),
        };

        // oracle price: 155 USDC / SOL = 155 * 10^6 / 10^9 = 0.000155 USDC/lamport
        let oracle_price = 155_000_000.0 / LAMPORTS_PER_SOL as f64;
        let opp = pool.find_optimal_arbitrage(oracle_price, 10);
        assert!(opp.is_some());
        let opp = opp.unwrap();
        assert_eq!(opp.direction, ArbDirection::WsolBisonfiUsdcTessera);
        assert!(opp.gross_profit_lamports > 0);
    }

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
}
