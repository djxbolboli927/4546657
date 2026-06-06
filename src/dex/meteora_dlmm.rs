//! Meteora DLMM (LB CLMM) — bin-based Liquidity Book AMM.
//!
//! On-chain program: `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`
//!
//! Unlike every other engine here, DLMM is **bin-based**, not a continuous
//! curve. Liquidity is sliced into discrete *bins*; each bin holds reserves at
//! a single fixed price and uses a **constant-sum** rule (`x·P + y = const`), so
//! a swap fills the active bin at exactly its price, then crosses to the next
//! bin. The pool (`LbPair`) account stores only `active_id`, `bin_step`, the fee
//! parameters and total vault reserves — the **per-bin reserves live in separate
//! `BinArray` accounts** (70 bins each). Pricing therefore *requires* reading the
//! relevant BinArray(s); without them we return `None` (never a phantom edge).
//!
//! Math ported verbatim from the official `MeteoraAg/dlmm-sdk` `commons/src/`
//! (the same code Meteora's own SDK uses), cross-checked against the deployed
//! program IDL (`idls/dlmm.json`):
//!   • `math/price_math.rs`        — `get_price_from_id`
//!   • `math/u64x64_math.rs`       — `pow` (Q64.64 binary exponentiation)
//!   • `math/u128x128_math.rs`     — `mul_shr` / `shl_div` (constant-sum convert)
//!   • `extensions/bin.rs`         — `get_amount_out` / `get_amount_in`
//!   • `extensions/lb_pair.rs`     — base+variable fee, volatility, bin advance
//!   • `extensions/bin_array.rs`   — bin↔array index, PDA derivation
//!   • `quote.rs`                  — `quote_exact_in` / fill amount
//!
//! Rounding (lamport-critical): output conversions round **down**, input
//! conversions round **up**, fees round **up** — exactly as on-chain.
//!
//! Price (Q64.64) of a bin = `(1 + bin_step/10000)^bin_id`. This is the price of
//! token **X** denominated in token **Y** (Y per X), atomic units.

use solana_sdk::pubkey::Pubkey;

use crate::dex::uint256::U256;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

// ── Constants (from commons/src/constants.rs & math/u64x64_math.rs) ────────────

/// 1.0 in Q64.64.
pub const ONE: u128 = 1u128 << 64;
/// Fixed-point scale (Q64.64).
const SCALE_OFFSET: u32 = 64;
/// Basis-point denominator (10_000).
const BASIS_POINT_MAX: u128 = 10_000;
/// Max `|exp|` accepted by `pow` (exclusive). Bits 0..18 are processed.
const MAX_EXPONENTIAL: u32 = 0x80000; // 524_288

/// Bins per BinArray.
pub const MAX_BIN_PER_ARRAY: i32 = 70;
pub const MIN_BIN_ID: i32 = -443_636;
pub const MAX_BIN_ID: i32 = 443_636;

/// Fee-rate denominator (1e9). A fee *rate* is `rate / FEE_PRECISION`.
const FEE_PRECISION: u128 = 1_000_000_000;
/// Cap on the total fee rate: 10% (1e8 / 1e9).
const MAX_FEE_RATE: u128 = 100_000_000;

/// Variable-fee scaling (ceil-div by 1e11).
const V_FEE_SCALING: u128 = 100_000_000_000; // 1e11
const V_FEE_ROUNDING: u128 = 99_999_999_999;

/// `status == 0` ⇒ Enabled (swap permitted).
const STATUS_ENABLED: u8 = 0;

/// Safety cap on bins crossed in a single quote (guards against pathological
/// loops over empty bins). Real arbitrage sizes cross far fewer.
const MAX_BINS_CROSSED: u32 = 5_000;

// ── Account byte offsets ──────────────────────────────────────────────────────
//
// LbPair = 8-byte Anchor discriminator + a `#[repr(C)]` zero-copy struct.
// Offsets below are absolute into the raw account `data` slice (discriminator
// included). Field order/layout verified against the dlmm IDL.
//
//   8    StaticParameters { base_factor u16, filter_period u16, decay_period u16,
//                           reduction_factor u16, variable_fee_control u32,
//                           max_volatility_accumulator u32, min/max_bin_id i32,
//                           protocol_share u16, base_fee_power_factor u8,
//                           function_type u8, collect_fee_mode u8, _pad[3] }
//   40   VariableParameters { volatility_accumulator u32, volatility_reference u32,
//                             index_reference i32, _pad[4], last_update_timestamp i64, .. }
//   76   active_id i32
//   80   bin_step u16
//   82   status u8
//   88   token_x_mint  Pubkey
//   120  token_y_mint  Pubkey
//   152  reserve_x     Pubkey (vault)
//   184  reserve_y     Pubkey (vault)
mod off {
    // StaticParameters
    pub const BASE_FACTOR: usize = 8;
    pub const FILTER_PERIOD: usize = 10;
    pub const DECAY_PERIOD: usize = 12;
    pub const REDUCTION_FACTOR: usize = 14;
    pub const VARIABLE_FEE_CONTROL: usize = 16;
    pub const MAX_VOLATILITY_ACC: usize = 20;
    pub const PROTOCOL_SHARE: usize = 32;
    pub const BASE_FEE_POWER_FACTOR: usize = 34;
    pub const COLLECT_FEE_MODE: usize = 36;
    // VariableParameters
    pub const VOLATILITY_ACC: usize = 40;
    pub const VOLATILITY_REF: usize = 44;
    pub const INDEX_REF: usize = 48;
    pub const LAST_UPDATE_TS: usize = 56;
    // Top-level
    pub const ACTIVE_ID: usize = 76;
    pub const BIN_STEP: usize = 80;
    pub const STATUS: usize = 82;
    pub const TOKEN_X_MINT: usize = 88;
    pub const TOKEN_Y_MINT: usize = 120;
    pub const RESERVE_X: usize = 152;
    pub const RESERVE_Y: usize = 184;
    /// Min length to read everything we use (reserve_y ends at 216).
    pub const MIN_LEN: usize = 216;
}

// BinArray = 8-byte disc + { index i64, version u8, _pad[7], lb_pair Pubkey,
//                            bins [Bin; 70] }. Bin stride = 144 bytes.
mod bin_arr {
    pub const BINS_START: usize = 56; // 8 disc + 8 index + 8 (ver+pad) + 32 lb_pair
    pub const BIN_STRIDE: usize = 144;
    pub const BIN_AMOUNT_X: usize = 0; // u64 within a bin
    pub const BIN_AMOUNT_Y: usize = 8; // u64
    pub const BIN_PRICE: usize = 16; // u128
    pub const COUNT: usize = 70;
    /// Full account data length (8 + 8 + 8 + 32 + 70*144).
    pub const MIN_LEN: usize = BINS_START + COUNT * BIN_STRIDE; // 10_136
}

// ── Little-endian readers ──────────────────────────────────────────────────────

#[inline]
fn rd_u128(d: &[u8], o: usize) -> Option<u128> {
    d.get(o..o + 16).map(|b| u128::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_u64(d: &[u8], o: usize) -> Option<u64> {
    d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_i64(d: &[u8], o: usize) -> Option<i64> {
    d.get(o..o + 8).map(|b| i64::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_u32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_i32(d: &[u8], o: usize) -> Option<i32> {
    d.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_u16(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()))
}
#[inline]
fn rd_pk(d: &[u8], o: usize) -> Option<Pubkey> {
    d.get(o..o + 32).map(|b| Pubkey::from(<[u8; 32]>::try_from(b).unwrap()))
}

// ── Pool state ─────────────────────────────────────────────────────────────────

/// Decoded LbPair — everything needed to quote a swap (the bin reserves are
/// read separately from BinArray accounts at quote time).
#[derive(Debug, Clone)]
pub struct LbPair {
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub collect_fee_mode: u8,
    // Static fee parameters
    pub base_factor: u16,
    pub base_fee_power_factor: u8,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    // Variable (dynamic) fee state
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

impl LbPair {
    /// Is this pool enabled and usable for quoting?
    pub fn is_supported(&self) -> bool {
        self.status == STATUS_ENABLED && self.bin_step > 0
    }
}

/// Parse an LbPair account. Returns `None` if too short.
pub fn parse_pool(data: &[u8]) -> Option<LbPair> {
    if data.len() < off::MIN_LEN {
        return None;
    }
    Some(LbPair {
        token_x_mint: rd_pk(data, off::TOKEN_X_MINT)?,
        token_y_mint: rd_pk(data, off::TOKEN_Y_MINT)?,
        reserve_x: rd_pk(data, off::RESERVE_X)?,
        reserve_y: rd_pk(data, off::RESERVE_Y)?,
        active_id: rd_i32(data, off::ACTIVE_ID)?,
        bin_step: rd_u16(data, off::BIN_STEP)?,
        status: *data.get(off::STATUS)?,
        collect_fee_mode: *data.get(off::COLLECT_FEE_MODE)?,
        base_factor: rd_u16(data, off::BASE_FACTOR)?,
        base_fee_power_factor: *data.get(off::BASE_FEE_POWER_FACTOR)?,
        filter_period: rd_u16(data, off::FILTER_PERIOD)?,
        decay_period: rd_u16(data, off::DECAY_PERIOD)?,
        reduction_factor: rd_u16(data, off::REDUCTION_FACTOR)?,
        variable_fee_control: rd_u32(data, off::VARIABLE_FEE_CONTROL)?,
        max_volatility_accumulator: rd_u32(data, off::MAX_VOLATILITY_ACC)?,
        volatility_accumulator: rd_u32(data, off::VOLATILITY_ACC)?,
        volatility_reference: rd_u32(data, off::VOLATILITY_REF)?,
        index_reference: rd_i32(data, off::INDEX_REF)?,
        last_update_timestamp: rd_i64(data, off::LAST_UPDATE_TS)?,
    })
}

/// Read `(amount_x, amount_y, stored_price)` for bin at `index_in_array`
/// (0..70) from a raw BinArray account.
fn read_bin(data: &[u8], index_in_array: usize) -> Option<(u64, u64, u128)> {
    if index_in_array >= bin_arr::COUNT {
        return None;
    }
    let base = bin_arr::BINS_START + index_in_array * bin_arr::BIN_STRIDE;
    let amount_x = rd_u64(data, base + bin_arr::BIN_AMOUNT_X)?;
    let amount_y = rd_u64(data, base + bin_arr::BIN_AMOUNT_Y)?;
    let price = rd_u128(data, base + bin_arr::BIN_PRICE)?;
    Some((amount_x, amount_y, price))
}

// ── Bin ↔ BinArray index + PDA ─────────────────────────────────────────────────

/// BinArray index containing `bin_id` (floor division by 70).
pub fn bin_array_index(bin_id: i32) -> i32 {
    bin_id.div_euclid(MAX_BIN_PER_ARRAY)
}

/// Lowest bin_id stored in BinArray `index`.
#[inline]
fn bin_array_lower_id(index: i32) -> i32 {
    index * MAX_BIN_PER_ARRAY
}

/// Derive the BinArray PDA for `lb_pair` and `index`.
/// Seeds: `["bin_array", lb_pair, (index as i64).to_le_bytes()]`.
pub fn derive_bin_array_pda(lb_pair: &Pubkey, index: i32) -> Pubkey {
    let idx_le = (index as i64).to_le_bytes();
    Pubkey::find_program_address(
        &[b"bin_array", lb_pair.as_ref(), &idx_le],
        &PROGRAM_ID,
    )
    .0
}

// ── Price math (price_math.rs / u64x64_math.rs) ────────────────────────────────

/// Q64.64 price of `bin_id`: `(1 + bin_step/10000)^bin_id`.
pub fn get_price_from_id(bin_id: i32, bin_step: u16) -> Option<u128> {
    // bps = (bin_step << 64) / 10000  →  bin_step/10000 in Q64.64
    let bps = ((bin_step as u128) << SCALE_OFFSET) / BASIS_POINT_MAX;
    let base = ONE.checked_add(bps)?;
    pow(base, bin_id)
}

/// `base^exp` in Q64.64. Ported verbatim from `u64x64_math.rs::pow`: binary
/// exponentiation where each multiply is `(a·b) >> 64`. Large bases are inverted
/// (`u128::MAX / x`) to keep every intermediate `< 2^128`. Negative exponents
/// invert the final result.
fn pow(base: u128, exp: i32) -> Option<u128> {
    if exp == 0 {
        return Some(ONE);
    }
    let mut invert = exp < 0;
    let exp_abs: u32 = exp.unsigned_abs();
    if exp_abs >= MAX_EXPONENTIAL {
        return None;
    }

    let mut squared_base = base;
    let mut result = ONE;

    // Keep squared_base < 1.0 (Q64.64) so squaring never overflows u128.
    if squared_base >= result {
        squared_base = u128::MAX / squared_base;
        invert = !invert;
    }

    // Bits 0..=18 (MAX_EXPONENTIAL = 2^19). Square after every bit except the last.
    let mut bit = 1u32;
    for i in 0..19 {
        if exp_abs & bit != 0 {
            result = (result.checked_mul(squared_base)?) >> SCALE_OFFSET;
        }
        if i < 18 {
            squared_base = (squared_base.checked_mul(squared_base)?) >> SCALE_OFFSET;
        }
        bit <<= 1;
    }

    if result == 0 {
        return None;
    }
    if invert {
        result = u128::MAX / result;
    }
    Some(result)
}

// ── Constant-sum conversions (u128x128_math.rs / bin.rs) ───────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    Up,
    Down,
}

/// `(x · y) >> 64`, rounded, as `u64`. `None` if the result exceeds `u64::MAX`.
fn mul_shr_64(x: u128, y: u128, rounding: Rounding) -> Option<u64> {
    let prod = U256::mul_u128(x, y);
    let (q, r) = prod.div_rem(U256::from_u128(ONE));
    let mut out = q.as_u128()?;
    if rounding == Rounding::Up && !r.is_zero() {
        out = out.checked_add(1)?;
    }
    u64::try_from(out).ok()
}

/// `(x << 64) / divisor`, rounded, as `u64`. `None` on overflow / zero divisor.
fn shl64_div(x: u64, divisor: u128, rounding: Rounding) -> Option<u64> {
    if divisor == 0 {
        return None;
    }
    // x << 64 fits u128 (x ≤ 2^64-1 ⇒ value < 2^128).
    let num = U256::from_u128((x as u128) << SCALE_OFFSET);
    let (q, r) = num.div_rem(U256::from_u128(divisor));
    let mut out = q.as_u128()?;
    if rounding == Rounding::Up && !r.is_zero() {
        out = out.checked_add(1)?;
    }
    u64::try_from(out).ok()
}

/// Output token amount for `amount_in` at a bin of `price` (Q64.64).
/// `swap_for_y` = X in → Y out (`out = in · price`); else Y in → X out
/// (`out = in / price`).
fn get_amount_out(amount_in: u64, price: u128, swap_for_y: bool, r: Rounding) -> Option<u64> {
    if swap_for_y {
        mul_shr_64(price, amount_in as u128, r)
    } else {
        shl64_div(amount_in, price, r)
    }
}

/// Input token amount required to obtain `amount_out` at a bin of `price`.
fn get_amount_in(amount_out: u64, price: u128, swap_for_y: bool, r: Rounding) -> Option<u64> {
    if swap_for_y {
        shl64_div(amount_out, price, r)
    } else {
        mul_shr_64(price, amount_out as u128, r)
    }
}

// ── Fees (lb_pair.rs) ──────────────────────────────────────────────────────────

/// Base fee rate = `base_factor · bin_step · 10 · 10^base_fee_power_factor`.
fn base_fee_rate(p: &LbPair) -> u128 {
    let pow10 = 10u128.checked_pow(p.base_fee_power_factor as u32).unwrap_or(u128::MAX);
    (p.base_factor as u128)
        .saturating_mul(p.bin_step as u128)
        .saturating_mul(10)
        .saturating_mul(pow10)
}

/// Variable fee rate = `ceil(variable_fee_control · (vol_acc · bin_step)^2 / 1e11)`.
fn variable_fee_rate(p: &LbPair, vol_acc: u32) -> u128 {
    if p.variable_fee_control == 0 {
        return 0;
    }
    let vfa_bin = (vol_acc as u128).saturating_mul(p.bin_step as u128);
    let square = vfa_bin.saturating_mul(vfa_bin);
    let v_fee = (p.variable_fee_control as u128).saturating_mul(square);
    v_fee.saturating_add(V_FEE_ROUNDING) / V_FEE_SCALING
}

/// Total fee rate (base + variable), capped at `MAX_FEE_RATE` (10%).
fn total_fee_rate(p: &LbPair, vol_acc: u32) -> u128 {
    base_fee_rate(p).saturating_add(variable_fee_rate(p, vol_acc)).min(MAX_FEE_RATE)
}

/// Fee **added on top** of a fee-exclusive `amount`: `ceil(amount·r / (1e9 - r))`.
fn compute_fee(amount: u64, fee_rate: u128) -> Option<u64> {
    let denom = FEE_PRECISION.checked_sub(fee_rate)?; // fee_rate ≤ 1e8 < 1e9
    if denom == 0 {
        return None;
    }
    let num = (amount as u128).checked_mul(fee_rate)?;
    let fee = (num + denom - 1) / denom; // ceil
    u64::try_from(fee).ok()
}

/// Fee **extracted from** a fee-inclusive `amount`: `ceil(amount·r / 1e9)`.
fn compute_fee_from_amount(amount: u64, fee_rate: u128) -> Option<u64> {
    let num = (amount as u128).checked_mul(fee_rate)?;
    let fee = (num + (FEE_PRECISION - 1)) / FEE_PRECISION; // ceil
    u64::try_from(fee).ok()
}

/// Is the fee taken from the *input* (vs output) for this mode + direction?
///   • mode 0 (InputOnly): always input.
///   • mode 1 (OnlyY): input only for Y→X (fee always paid in Y).
fn fee_on_input(collect_fee_mode: u8, swap_for_y: bool) -> bool {
    match collect_fee_mode {
        1 => !swap_for_y,
        _ => true,
    }
}

// ── Volatility (lb_pair.rs) ────────────────────────────────────────────────────

/// Replicate `update_references(now)` → `(volatility_reference, index_reference)`
/// that will be in effect for this swap.
fn update_references(p: &LbPair, now: i64) -> (u32, i32) {
    let elapsed = now.saturating_sub(p.last_update_timestamp);
    if elapsed >= p.filter_period as i64 {
        let index_ref = p.active_id;
        let vol_ref = if elapsed < p.decay_period as i64 {
            ((p.volatility_accumulator as u64 * p.reduction_factor as u64) / BASIS_POINT_MAX as u64)
                as u32
        } else {
            0
        };
        (vol_ref, index_ref)
    } else {
        (p.volatility_reference, p.index_reference)
    }
}

/// `update_volatility_accumulator()` for `active_id`, given the references.
fn volatility_accumulator(p: &LbPair, vol_ref: u32, index_ref: i32, active_id: i32) -> u32 {
    let delta_id = (index_ref as i64 - active_id as i64).unsigned_abs();
    let acc = (vol_ref as u64).saturating_add(delta_id.saturating_mul(BASIS_POINT_MAX as u64));
    acc.min(p.max_volatility_accumulator as u64) as u32
}

// ── Swap (quote.rs) ────────────────────────────────────────────────────────────

/// Fill as much of `amount_in` as the active bin permits.
/// Returns `(amount_in_consumed, amount_out_net)`.
fn fill_bin(
    amount_in: u64,
    bin_reserve_out: u64,
    price: u128,
    swap_for_y: bool,
    fee_rate: u128,
    fee_on_input: bool,
) -> Option<(u64, u64)> {
    if bin_reserve_out == 0 {
        return Some((0, 0)); // empty bin — advance without consuming
    }
    // Net input that fully drains this bin's output reserve (rounded up).
    let max_amount_in = get_amount_in(bin_reserve_out, price, swap_for_y, Rounding::Up)?;

    if fee_on_input {
        let max_fee = compute_fee(max_amount_in, fee_rate)?;
        let max_in_with_fee = max_amount_in.checked_add(max_fee)?;
        if amount_in >= max_in_with_fee {
            // Fully fill bin.
            Some((max_in_with_fee, bin_reserve_out))
        } else {
            // Partial: extract fee from the gross input, convert the remainder.
            let fee = compute_fee_from_amount(amount_in, fee_rate)?;
            let net_in = amount_in.checked_sub(fee)?;
            let out = get_amount_out(net_in, price, swap_for_y, Rounding::Down)?;
            Some((amount_in, out))
        }
    } else {
        // Fee on output.
        if amount_in >= max_amount_in {
            let gross_out = bin_reserve_out;
            let fee = compute_fee_from_amount(gross_out, fee_rate)?;
            Some((max_amount_in, gross_out.checked_sub(fee)?))
        } else {
            let gross_out = get_amount_out(amount_in, price, swap_for_y, Rounding::Down)?;
            let fee = compute_fee_from_amount(gross_out, fee_rate)?;
            Some((amount_in, gross_out.checked_sub(fee)?))
        }
    }
}

#[inline]
fn advance_active_bin(active_id: i32, swap_for_y: bool) -> i32 {
    if swap_for_y {
        active_id - 1
    } else {
        active_id + 1
    }
}

/// Exact-in swap quote across as many bins as needed.
///
/// `swap_for_y == true`  → token **X in**, token **Y out** (`active_id` decreases).
/// `swap_for_y == false` → token **Y in**, token **X out** (`active_id` increases).
///
/// `get_bin_array_data(index)` returns the raw account data for the BinArray at
/// `index`, or `None` if it isn't available. The closure is only invoked when
/// the swap enters a new array; the caller derives the PDA via
/// [`derive_bin_array_pda`]. Returns `None` (no quote) on:
///   • disabled pool / zero input,
///   • a required BinArray not being available (unknown liquidity — conservative),
///   • running out of liquidity within the available bins (on-chain would abort),
///   • arithmetic overflow.
pub fn quote_exact_in<F>(
    pool: &LbPair,
    amount_in: u64,
    swap_for_y: bool,
    current_timestamp: i64,
    mut get_bin_array_data: F,
) -> Option<u64>
where
    F: FnMut(i32) -> Option<Vec<u8>>,
{
    if amount_in == 0 || !pool.is_supported() {
        return None;
    }

    let (vol_ref, index_ref) = update_references(pool, current_timestamp);
    let on_input = fee_on_input(pool.collect_fee_mode, swap_for_y);

    let mut active_id = pool.active_id;
    let mut amount_left = amount_in;
    let mut total_out: u64 = 0;

    // Cache the currently loaded BinArray so we fetch each array at most once.
    let mut cur_index: Option<i32> = None;
    let mut cur_data: Vec<u8> = Vec::new();

    let mut guard = 0u32;
    while amount_left > 0 {
        guard += 1;
        if guard > MAX_BINS_CROSSED {
            return None;
        }
        if active_id < MIN_BIN_ID || active_id > MAX_BIN_ID {
            return None; // out of bin range → insufficient liquidity
        }

        let arr_index = bin_array_index(active_id);
        if cur_index != Some(arr_index) {
            let data = get_bin_array_data(arr_index)?; // missing array ⇒ stop (conservative)
            if data.len() < bin_arr::MIN_LEN {
                return None;
            }
            cur_data = data;
            cur_index = Some(arr_index);
        }

        let in_arr = (active_id - bin_array_lower_id(arr_index)) as usize;
        let (amount_x, amount_y, stored_price) = read_bin(&cur_data, in_arr)?;

        let price = if stored_price != 0 {
            stored_price
        } else {
            get_price_from_id(active_id, pool.bin_step)?
        };

        let vol_acc = volatility_accumulator(pool, vol_ref, index_ref, active_id);
        let fee_rate = total_fee_rate(pool, vol_acc);

        let bin_reserve_out = if swap_for_y { amount_y } else { amount_x };

        let (consumed, out) =
            fill_bin(amount_left, bin_reserve_out, price, swap_for_y, fee_rate, on_input)?;

        amount_left = amount_left.checked_sub(consumed)?;
        total_out = total_out.checked_add(out)?;

        if amount_left == 0 {
            break;
        }
        // Bin exhausted (or empty) — cross to the next.
        active_id = advance_active_bin(active_id, swap_for_y);
    }

    if total_out == 0 {
        return None;
    }
    Some(total_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pow / price ────────────────────────────────────────────────────────────

    #[test]
    fn price_at_id_zero_is_one() {
        for bs in [1u16, 10, 25, 100, 400] {
            assert_eq!(get_price_from_id(0, bs), Some(ONE), "bin_step={bs}");
        }
    }

    #[test]
    fn pow_exp_zero_is_one() {
        assert_eq!(pow(ONE, 0), Some(ONE));
        assert_eq!(pow(ONE * 2, 0), Some(ONE));
    }

    #[test]
    fn price_monotonic_around_zero() {
        let bs = 25u16;
        let p_neg = get_price_from_id(-1, bs).unwrap();
        let p_zero = get_price_from_id(0, bs).unwrap();
        let p_pos = get_price_from_id(1, bs).unwrap();
        assert!(p_neg < p_zero, "{p_neg} < {p_zero}");
        assert!(p_zero < p_pos, "{p_zero} < {p_pos}");
    }

    #[test]
    fn price_id_one_close_to_base() {
        // bin_step=10 ⇒ base = 1.001. price(1) ≈ 1.001 in Q64.64 (within rounding).
        let bs = 10u16;
        let price = get_price_from_id(1, bs).unwrap();
        let base = ONE + ((bs as u128) << SCALE_OFFSET) / BASIS_POINT_MAX;
        // pow(base,1) inverts twice; allow a tiny tolerance.
        let diff = base.abs_diff(price);
        assert!(diff < (ONE / 1_000_000), "price {price} vs base {base}, diff {diff}");
    }

    #[test]
    fn price_inverse_symmetry() {
        // price(-k) ≈ 1 / price(k): product ≈ ONE^2 / ... → price(k)·price(-k) ≈ 2^128.
        let bs = 100u16;
        let pk = get_price_from_id(50, bs).unwrap();
        let pnk = get_price_from_id(-50, bs).unwrap();
        // (pk * pnk) >> 64 should be ≈ ONE.
        let prod = mul_shr_64(pk, pnk, Rounding::Down).map(|v| v as u128);
        // pk*pnk is Q64.64*Q64.64 = Q128; >>64 leaves Q64.64 ≈ ONE.
        // Use the raw product instead since mul_shr_64 casts to u64.
        let raw = U256::mul_u128(pk, pnk);
        let (q, _) = raw.div_rem(U256::from_u128(ONE));
        let one_q64 = q.as_u128().unwrap();
        let diff = one_q64.abs_diff(ONE);
        assert!(diff < (ONE / 100_000), "pk*pnk>>64 = {one_q64} vs ONE {}", ONE);
        let _ = prod;
    }

    // ── conversions ──────────────────────────────────────────────────────────────

    #[test]
    fn convert_at_price_one_is_identity() {
        // price = 1.0 ⇒ out == in, both directions, no rounding loss.
        let out_y = get_amount_out(1000, ONE, true, Rounding::Down).unwrap();
        assert_eq!(out_y, 1000);
        let out_x = get_amount_out(1000, ONE, false, Rounding::Down).unwrap();
        assert_eq!(out_x, 1000);
    }

    #[test]
    fn convert_rounding_directions() {
        // price = 3·2^64 (price 3.0). Y→X out = in/3.
        let price = ONE * 3;
        // in=10, out_x = 10/3 = 3.33 → Down=3, Up=4
        assert_eq!(get_amount_out(10, price, false, Rounding::Down).unwrap(), 3);
        assert_eq!(get_amount_out(10, price, false, Rounding::Up).unwrap(), 4);
        // X→Y out = in*3 (exact)
        assert_eq!(get_amount_out(10, price, true, Rounding::Down).unwrap(), 30);
    }

    // ── fee math ──────────────────────────────────────────────────────────────────

    #[test]
    fn base_fee_legacy_formula() {
        // base_factor=10000, bin_step=10, power=0 → 10000*10*10 = 1_000_000 (0.1%).
        let p = sample_pool(0, 10, 10_000, 0, 0);
        assert_eq!(base_fee_rate(&p), 1_000_000);
        assert_eq!(total_fee_rate(&p, 0), 1_000_000);
    }

    #[test]
    fn base_fee_power_factor_multiplies() {
        // power=1 → ×10.
        let p = sample_pool(0, 10, 10_000, 1, 0);
        assert_eq!(base_fee_rate(&p), 10_000_000);
    }

    #[test]
    fn total_fee_capped_at_10pct() {
        let p = sample_pool(0, 400, 60_000, 0, 0);
        // 60000*400*10 = 240_000_000 > MAX_FEE_RATE(1e8) → capped.
        assert_eq!(total_fee_rate(&p, 0), MAX_FEE_RATE);
    }

    #[test]
    fn compute_fee_roundtrips() {
        // 0.1% fee. Extract from 1000 gross: ceil(1000*1e6/1e9)=1.
        assert_eq!(compute_fee_from_amount(1000, 1_000_000).unwrap(), 1);
        // Add on top of net 999: ceil(999*1e6/(1e9-1e6)) = ceil(999e6/999e6)=1.
        assert_eq!(compute_fee(999, 1_000_000).unwrap(), 1);
    }

    // ── bin/array index ────────────────────────────────────────────────────────

    #[test]
    fn bin_array_index_floor_division() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_array_index(69), 0);
        assert_eq!(bin_array_index(70), 1);
        assert_eq!(bin_array_index(-1), -1);
        assert_eq!(bin_array_index(-70), -1);
        assert_eq!(bin_array_index(-71), -2);
    }

    // ── full swap quotes (single bin, golden) ────────────────────────────────────

    /// Build a BinArray account holding one funded bin at `bin_id`.
    fn make_bin_array(bin_id: i32, amount_x: u64, amount_y: u64, price: u128) -> Vec<u8> {
        let mut data = vec![0u8; bin_arr::MIN_LEN];
        let arr_index = bin_array_index(bin_id);
        let in_arr = (bin_id - bin_array_lower_id(arr_index)) as usize;
        let base = bin_arr::BINS_START + in_arr * bin_arr::BIN_STRIDE;
        data[base..base + 8].copy_from_slice(&amount_x.to_le_bytes());
        data[base + 8..base + 16].copy_from_slice(&amount_y.to_le_bytes());
        data[base + 16..base + 32].copy_from_slice(&price.to_le_bytes());
        data
    }

    #[test]
    fn swap_single_bin_no_fee_price_one() {
        // active_id 0, price 1.0, no fee. X→Y of 1000 → 1000 out.
        let pool = sample_pool(0, 10, 0, 0, 0); // base_factor 0 → zero fee
        let arr = make_bin_array(0, 1_000_000, 1_000_000, ONE);
        let out = quote_exact_in(&pool, 1_000, true, 0, |idx| {
            if idx == 0 { Some(arr.clone()) } else { None }
        });
        assert_eq!(out, Some(1_000));
    }

    #[test]
    fn swap_single_bin_with_fee_on_input() {
        // 0.1% fee on input, partial fill. fee=ceil(1000*1e6/1e9)=1, net=999, out=999.
        let pool = sample_pool(0, 10, 10_000, 0, 0); // 0.1% base fee, collect_fee_mode 0
        let arr = make_bin_array(0, 1_000_000, 1_000_000, ONE);
        let out = quote_exact_in(&pool, 1_000, true, 0, |idx| {
            if idx == 0 { Some(arr.clone()) } else { None }
        });
        assert_eq!(out, Some(999));
    }

    #[test]
    fn swap_missing_bin_array_returns_none() {
        let pool = sample_pool(0, 10, 0, 0, 0);
        let out = quote_exact_in(&pool, 1_000, true, 0, |_| None);
        assert_eq!(out, None);
    }

    #[test]
    fn swap_crosses_into_next_bin() {
        // Y→X swap (active_id increases): bins 0 and 1 are both in array index 0,
        // so a single array serves the whole crossing. Active bin 0 holds only
        // 400 X; swapping 1000 Y crosses up into bin 1. price 1.0, zero fee.
        let pool = sample_pool(0, 10, 0, 0, 0);
        let mut data = vec![0u8; bin_arr::MIN_LEN];
        let put = |data: &mut Vec<u8>, bid: i32, x: u64, y: u64| {
            let ai = bin_array_index(bid);
            let ia = (bid - bin_array_lower_id(ai)) as usize;
            let b = bin_arr::BINS_START + ia * bin_arr::BIN_STRIDE;
            data[b..b + 8].copy_from_slice(&x.to_le_bytes());
            data[b + 8..b + 16].copy_from_slice(&y.to_le_bytes());
            data[b + 16..b + 32].copy_from_slice(&ONE.to_le_bytes());
        };
        put(&mut data, 0, 400, 0); // active bin: only 400 X
        put(&mut data, 1, 1_000, 0); // next bin up: plenty of X
        let out = quote_exact_in(&pool, 1_000, false, 0, |idx| {
            if idx == 0 { Some(data.clone()) } else { None }
        })
        .unwrap();
        // 400 X from bin 0 + 600 X from bin 1 (both price 1.0) = 1000.
        assert_eq!(out, 1_000);
    }

    #[test]
    fn swap_crosses_into_next_array() {
        // X→Y swap (active_id decreases) from bin 0 into bin -1, which lives in
        // array index -1 — exercises the multi-array fetch path. price 1.0, no fee.
        let pool = sample_pool(0, 10, 0, 0, 0);
        let arr0 = make_bin_array(0, 0, 400, ONE); // active bin: 400 Y
        let arr_neg1 = make_bin_array(-1, 0, 1_000, ONE); // next bin down: plenty
        let out = quote_exact_in(&pool, 1_000, true, 0, |idx| match idx {
            0 => Some(arr0.clone()),
            -1 => Some(arr_neg1.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(out, 1_000);
    }

    #[test]
    fn swap_disabled_pool_none() {
        let mut pool = sample_pool(0, 10, 0, 0, 0);
        pool.status = 1; // disabled
        let arr = make_bin_array(0, 1_000_000, 1_000_000, ONE);
        let out = quote_exact_in(&pool, 1_000, true, 0, |_| Some(arr.clone()));
        assert_eq!(out, None);
    }

    #[test]
    fn parse_pool_too_short_none() {
        assert!(parse_pool(&[0u8; 100]).is_none());
    }

    #[test]
    fn parse_pool_roundtrip() {
        let mut data = vec![0u8; 904];
        let x_mint = Pubkey::new_unique();
        let y_mint = Pubkey::new_unique();
        let res_x = Pubkey::new_unique();
        let res_y = Pubkey::new_unique();
        data[off::BASE_FACTOR..off::BASE_FACTOR + 2].copy_from_slice(&10_000u16.to_le_bytes());
        data[off::BIN_STEP..off::BIN_STEP + 2].copy_from_slice(&25u16.to_le_bytes());
        data[off::ACTIVE_ID..off::ACTIVE_ID + 4].copy_from_slice(&(-5i32).to_le_bytes());
        data[off::STATUS] = 0;
        data[off::COLLECT_FEE_MODE] = 1;
        data[off::TOKEN_X_MINT..off::TOKEN_X_MINT + 32].copy_from_slice(x_mint.as_ref());
        data[off::TOKEN_Y_MINT..off::TOKEN_Y_MINT + 32].copy_from_slice(y_mint.as_ref());
        data[off::RESERVE_X..off::RESERVE_X + 32].copy_from_slice(res_x.as_ref());
        data[off::RESERVE_Y..off::RESERVE_Y + 32].copy_from_slice(res_y.as_ref());

        let p = parse_pool(&data).unwrap();
        assert_eq!(p.token_x_mint, x_mint);
        assert_eq!(p.token_y_mint, y_mint);
        assert_eq!(p.reserve_x, res_x);
        assert_eq!(p.reserve_y, res_y);
        assert_eq!(p.active_id, -5);
        assert_eq!(p.bin_step, 25);
        assert_eq!(p.base_factor, 10_000);
        assert_eq!(p.collect_fee_mode, 1);
        assert!(p.is_supported());
    }

    // helper: build a minimal LbPair for swap/fee tests.
    fn sample_pool(
        active_id: i32,
        bin_step: u16,
        base_factor: u16,
        base_fee_power_factor: u8,
        variable_fee_control: u32,
    ) -> LbPair {
        LbPair {
            token_x_mint: Pubkey::new_unique(),
            token_y_mint: Pubkey::new_unique(),
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            active_id,
            bin_step,
            status: 0,
            collect_fee_mode: 0,
            base_factor,
            base_fee_power_factor,
            filter_period: 30,
            decay_period: 600,
            reduction_factor: 5000,
            variable_fee_control,
            max_volatility_accumulator: 350_000,
            volatility_accumulator: 0,
            volatility_reference: 0,
            index_reference: active_id,
            last_update_timestamp: 0,
        }
    }
}
