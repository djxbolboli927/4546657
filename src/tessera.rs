/// Tessera V (dark pool by Wintermute) oracle price tracker.
///
/// Tessera V uses Pyth oracle pricing with near-zero slippage for < $100k trades.
/// Program: TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH
/// Authority: 8ekCy2jHHUbW2yeNGFWYJT9Hm9FW7SvZcZK66dSZCDiF
///
/// Price updates 11 times per second via Pyth oracle feed.
/// For our purposes, we treat Tessera V as a linear price source:
///   amount_out_usdc = amount_in_wsol * oracle_price * (1 - fee)
///   amount_out_wsol = amount_in_usdc / oracle_price * (1 - fee)
///
/// The oracle price is extracted from the authority account data at startup
/// and cached. The authority account is subscribed via Geyser to track updates.
///
/// Account data layout for Tessera V authority (reverse-engineered placeholder):
///   Bytes contain the Pyth price as a fixed-point value or f64.
///   The calibrate() function scans for plausible USDC/SOL price values.

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const TESSERA_PROGRAM_ID: &str = "TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH";
pub const TESSERA_AUTHORITY: &str = "8ekCy2jHHUbW2yeNGFWYJT9Hm9FW7SvZcZK66dSZCDiF";

/// Tessera V fee: approximately 0.05% (5 bps) — oracle-based dark pool.
/// Adjust once confirmed from their documentation.
pub const TESSERA_FEE_BPS: u64 = 5;

/// The oracle price encoded as u64: price * PRICE_SCALE
/// This allows atomic updates without floating-point races.
pub const PRICE_SCALE: u64 = 1_000_000_000; // 9 decimal places

/// Layout descriptor for parsing the Tessera V authority account.
#[derive(Debug, Clone, Copy)]
pub struct TesseraLayout {
    /// Byte offset in the account data where the WSOL/USDC price is stored.
    /// The price value is interpreted according to `price_format`.
    pub price_offset: usize,
    pub price_format: PriceFormat,
}

#[derive(Debug, Clone, Copy)]
pub enum PriceFormat {
    /// 64-bit float, little-endian (f64)
    F64Le,
    /// Fixed-point u64 with given denominator (e.g., price = raw / 10^6)
    U64FixedPoint { denominator: u64 },
    /// Price stored in Pyth format: exponent separate from mantissa
    PythCompact { mantissa_offset: usize, exponent_offset: usize },
}

impl Default for TesseraLayout {
    fn default() -> Self {
        // Default: try f64 at offset 8 (after discriminator)
        Self {
            price_offset: 8,
            price_format: PriceFormat::F64Le,
        }
    }
}

/// Snapshot of Tessera V oracle price.
#[derive(Debug, Clone)]
pub struct TesseraPool {
    /// Oracle price: USDC (6 dec) per WSOL lamport (9 dec)
    /// = USDC_price_per_sol / 10^3
    /// Example: SOL = $155, so oracle_price = 155_000_000 / 1_000_000_000 = 0.000155
    pub oracle_price_per_lamport: f64,
    pub fee_bps: u64,
    pub layout: TesseraLayout,
}

impl TesseraPool {
    pub fn new(oracle_price_per_lamport: f64, layout: TesseraLayout) -> Self {
        Self {
            oracle_price_per_lamport,
            fee_bps: TESSERA_FEE_BPS,
            layout,
        }
    }

    /// Parse a Tessera V authority account update.
    /// Returns None if the price cannot be extracted or is implausible.
    pub fn parse(data: &[u8], layout: &TesseraLayout) -> Option<Self> {
        let price = extract_price(data, layout)?;

        // Sanity: SOL price should be between $1 and $100,000
        // oracle_price_per_lamport = usdc_per_sol / 1e3
        let usdc_per_sol = price * 1_000.0;
        if usdc_per_sol < 1.0 || usdc_per_sol > 100_000.0 {
            warn!(
                "Tessera V oracle price out of range: ${usdc_per_sol:.2}/SOL at offset {}",
                layout.price_offset
            );
            return None;
        }

        debug!("Tessera V oracle: ${usdc_per_sol:.4}/SOL (per-lamport={price:.12})");

        Some(Self {
            oracle_price_per_lamport: price,
            fee_bps: TESSERA_FEE_BPS,
            layout: *layout,
        })
    }

    /// Quote: USDC micro-units (6 dec) out for `wsol_in` lamports.
    pub fn quote_wsol_to_usdc(&self, wsol_in: u64) -> u64 {
        let usdc_out = wsol_in as f64
            * self.oracle_price_per_lamport
            * (1.0 - self.fee_bps as f64 / 10_000.0);
        usdc_out as u64
    }

    /// Quote: WSOL lamports out for `usdc_in` micro-units.
    pub fn quote_usdc_to_wsol(&self, usdc_in: u64) -> u64 {
        if self.oracle_price_per_lamport == 0.0 {
            return 0;
        }
        let wsol_out = usdc_in as f64
            / self.oracle_price_per_lamport
            * (1.0 - self.fee_bps as f64 / 10_000.0);
        wsol_out as u64
    }

    /// Current oracle price in USD per SOL (human readable).
    pub fn usdc_per_sol(&self) -> f64 {
        self.oracle_price_per_lamport * 1_000.0
    }
}

/// Scan account data for a plausible SOL/USD oracle price.
/// Returns a layout descriptor once found.
///
/// Call at startup with a known SOL price to find the correct offset.
pub fn calibrate_layout(
    data: &[u8],
    expected_usdc_per_sol: f64, // e.g. 155.0
) -> Result<TesseraLayout> {
    let tolerance = 0.05; // 5% tolerance

    // Try f64 at each byte offset
    for off in (0..data.len().saturating_sub(8)).step_by(1) {
        let raw = f64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        // Direct price match
        if f64::abs(raw - expected_usdc_per_sol) / expected_usdc_per_sol < tolerance {
            info!("Tessera V calibrate: f64 price found at offset {off} (raw={raw:.4})");
            return Ok(TesseraLayout {
                price_offset: off,
                price_format: PriceFormat::F64Le,
            });
        }
        // Per-lamport price match
        let per_lamport = expected_usdc_per_sol / 1_000.0;
        if f64::abs(raw - per_lamport) / per_lamport < tolerance {
            info!("Tessera V calibrate: per-lamport price found at offset {off} (raw={raw:.12})");
            return Ok(TesseraLayout {
                price_offset: off,
                price_format: PriceFormat::F64Le,
            });
        }
    }

    // Try u64 fixed-point at each 8-byte offset
    for off in (0..data.len().saturating_sub(8)).step_by(8) {
        let raw = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        // Try common denominators
        for &denom in &[1u64, 100, 1_000, 10_000, 1_000_000, 1_000_000_000] {
            if denom == 0 {
                continue;
            }
            let price = raw as f64 / denom as f64;
            if f64::abs(price - expected_usdc_per_sol) / expected_usdc_per_sol < tolerance {
                info!(
                    "Tessera V calibrate: u64 price found at offset {off} denom={denom} (raw={raw} → ${price:.2})"
                );
                return Ok(TesseraLayout {
                    price_offset: off,
                    price_format: PriceFormat::U64FixedPoint { denominator: denom },
                });
            }
        }
    }

    Err(anyhow!(
        "Could not calibrate Tessera V layout for expected price ${expected_usdc_per_sol:.2}/SOL. \
        The account data format may be Pyth-native. \
        Check if the authority account embeds a Pyth PriceAccount directly."
    ))
}

fn extract_price(data: &[u8], layout: &TesseraLayout) -> Option<f64> {
    let off = layout.price_offset;
    match layout.price_format {
        PriceFormat::F64Le => {
            if data.len() < off + 8 {
                return None;
            }
            let raw = f64::from_le_bytes(data[off..off + 8].try_into().ok()?);
            if raw.is_nan() || raw.is_infinite() || raw <= 0.0 {
                return None;
            }
            Some(raw)
        }
        PriceFormat::U64FixedPoint { denominator } => {
            if data.len() < off + 8 || denominator == 0 {
                return None;
            }
            let raw = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
            Some(raw as f64 / denominator as f64)
        }
        PriceFormat::PythCompact {
            mantissa_offset,
            exponent_offset,
        } => {
            if data.len() < mantissa_offset + 8 || data.len() < exponent_offset + 4 {
                return None;
            }
            let mantissa =
                i64::from_le_bytes(data[mantissa_offset..mantissa_offset + 8].try_into().ok()?);
            let exponent =
                i32::from_le_bytes(data[exponent_offset..exponent_offset + 4].try_into().ok()?);
            let price = mantissa as f64 * 10f64.powi(exponent);
            if price <= 0.0 {
                return None;
            }
            Some(price)
        }
    }
}

/// Shared atomic oracle price (scaled by PRICE_SCALE) for lock-free access.
/// Stores oracle_price_per_lamport * PRICE_SCALE as u64.
#[derive(Default)]
pub struct AtomicOraclePrice(AtomicU64);

impl AtomicOraclePrice {
    pub fn store(&self, price_per_lamport: f64) {
        let scaled = (price_per_lamport * PRICE_SCALE as f64) as u64;
        self.0.store(scaled, Ordering::Relaxed);
    }

    pub fn load(&self) -> f64 {
        let scaled = self.0.load(Ordering::Relaxed);
        scaled as f64 / PRICE_SCALE as f64
    }

    pub fn is_valid(&self) -> bool {
        self.0.load(Ordering::Relaxed) > 0
    }
}

impl std::fmt::Debug for AtomicOraclePrice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AtomicOraclePrice(${:.4}/SOL)", self.load() * 1_000.0)
    }
}
