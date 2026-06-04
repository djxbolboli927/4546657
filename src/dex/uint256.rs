//! Minimal unsigned 256-bit integer for DEX swap math that overflows `u128`.
//!
//! Meteora DAMM v2 (Uniswap-v3-style) needs 256-bit intermediates: `liquidity *
//! sqrt_price` reaches ~2^224 and `amount_in << 128` reaches ~2^192, both well
//! past `u128::MAX`. Rather than pull in an external bignum, this module
//! implements exactly the handful of operations the curve needs, represented as
//! four little-endian `u64` limbs, and unit-tests each against `u128` reference
//! values.
//!
//! Only what the curve uses is implemented: full 128×128 product, add, compare,
//! shift, and a binary long-division `div_rem`. Everything is `checked` or
//! explicitly documented for overflow behaviour.

/// Unsigned 256-bit integer. `limbs[0]` is the least-significant 64 bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct U256 {
    limbs: [u64; 4],
}

impl U256 {
    pub const ZERO: U256 = U256 { limbs: [0; 4] };
    pub const ONE: U256 = U256 { limbs: [1, 0, 0, 0] };

    #[inline]
    pub const fn from_u128(x: u128) -> U256 {
        U256 {
            limbs: [x as u64, (x >> 64) as u64, 0, 0],
        }
    }

    #[inline]
    pub const fn from_u64(x: u64) -> U256 {
        U256 {
            limbs: [x, 0, 0, 0],
        }
    }

    #[inline]
    pub fn is_zero(&self) -> bool {
        self.limbs == [0u64; 4]
    }

    /// `x << 128` for a `u64` input, exact (lands in limb 2). Used for the
    /// B→A next-sqrt-price step: `(amount_in << 128) / liquidity`.
    #[inline]
    pub const fn shl128_u64(x: u64) -> U256 {
        U256 {
            limbs: [0, 0, x, 0],
        }
    }

    /// `self >> 128`, dropping the low 128 bits. Used for the token-B output:
    /// `floor(liquidity * delta_sqrt_price >> 128)`.
    #[inline]
    pub const fn shr128(self) -> U256 {
        U256 {
            limbs: [self.limbs[2], self.limbs[3], 0, 0],
        }
    }

    /// Convert to `u128`, returning `None` if the value exceeds `u128::MAX`
    /// (i.e. the high 128 bits are non-zero).
    #[inline]
    pub fn as_u128(&self) -> Option<u128> {
        if self.limbs[2] != 0 || self.limbs[3] != 0 {
            return None;
        }
        Some((self.limbs[1] as u128) << 64 | self.limbs[0] as u128)
    }

    /// Full 128×128 → 256-bit product, no precision loss.
    pub fn mul_u128(a: u128, b: u128) -> U256 {
        // Split each operand into two 64-bit halves and accumulate the four
        // partial products. Each `pXY` fits in u128 since (2^64-1)^2 < 2^128.
        let a0 = (a as u64) as u128;
        let a1 = (a >> 64) as u128;
        let b0 = (b as u64) as u128;
        let b1 = (b >> 64) as u128;

        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;

        let limb0 = p00 as u64;
        // mid = high(p00) + low(p01) + low(p10); fits in u128 (< 3*2^64).
        let mid = (p00 >> 64) + (p01 & u64::MAX as u128) + (p10 & u64::MAX as u128);
        let limb1 = mid as u64;
        // hi = high(mid) + high(p01) + high(p10) + low(p11); fits in u128.
        let hi = (mid >> 64) + (p01 >> 64) + (p10 >> 64) + (p11 & u64::MAX as u128);
        let limb2 = hi as u64;
        // top = high(hi) + high(p11); fits in u64.
        let top = (hi >> 64) + (p11 >> 64);
        let limb3 = top as u64;

        U256 {
            limbs: [limb0, limb1, limb2, limb3],
        }
    }

    /// `self + other`, returning `None` on 256-bit overflow.
    pub fn checked_add(self, other: U256) -> Option<U256> {
        let mut out = [0u64; 4];
        let mut carry = 0u128;
        for i in 0..4 {
            let sum = self.limbs[i] as u128 + other.limbs[i] as u128 + carry;
            out[i] = sum as u64;
            carry = sum >> 64;
        }
        if carry != 0 {
            return None;
        }
        Some(U256 { limbs: out })
    }

    /// `self - other` mod 2^256 (wrapping). Caller must ensure `self >= other`
    /// for a meaningful result; used only after a `>=` check in `div_rem`.
    fn wrapping_sub(self, other: U256) -> U256 {
        let mut out = [0u64; 4];
        let mut borrow = 0i128;
        for i in 0..4 {
            let diff = self.limbs[i] as i128 - other.limbs[i] as i128 - borrow;
            if diff < 0 {
                out[i] = (diff + (1i128 << 64)) as u64;
                borrow = 1;
            } else {
                out[i] = diff as u64;
                borrow = 0;
            }
        }
        U256 { limbs: out }
    }

    /// `self << 1`, returning the bit shifted out of the top (bit 255).
    fn shl1(self) -> (U256, u64) {
        let mut out = [0u64; 4];
        let mut carry = 0u64;
        for i in 0..4 {
            let new_carry = self.limbs[i] >> 63;
            out[i] = (self.limbs[i] << 1) | carry;
            carry = new_carry;
        }
        (U256 { limbs: out }, carry)
    }

    /// Bit `i` (0 = least significant) as 0 or 1.
    #[inline]
    fn bit(&self, i: usize) -> u64 {
        (self.limbs[i / 64] >> (i % 64)) & 1
    }

    /// Set bit `i` to 1.
    #[inline]
    fn set_bit(&mut self, i: usize) {
        self.limbs[i / 64] |= 1u64 << (i % 64);
    }

    /// `self >= other` (unsigned).
    #[inline]
    fn ge(&self, other: &U256) -> bool {
        for i in (0..4).rev() {
            if self.limbs[i] != other.limbs[i] {
                return self.limbs[i] > other.limbs[i];
            }
        }
        true // equal
    }

    /// Truncating division with remainder: returns `(self / divisor, self %
    /// divisor)`. Classic MSB→LSB binary long division. `divisor` must be
    /// non-zero.
    ///
    /// The running remainder `r` obeys the invariant `r < divisor` at the start
    /// of every iteration, so `r << 1 | bit` is `< 2*divisor` and a single
    /// conditional subtraction restores the invariant — exactly the schoolbook
    /// algorithm. The `carry` out of the shift represents bit 256 of the
    /// (divisor-bounded) value and forces a subtraction when set.
    pub fn div_rem(self, divisor: U256) -> (U256, U256) {
        debug_assert!(!divisor.is_zero(), "division by zero");
        let mut q = U256::ZERO;
        let mut r = U256::ZERO;
        for i in (0..256).rev() {
            let (shifted, carry) = r.shl1();
            let mut nr = shifted;
            if self.bit(i) == 1 {
                nr.limbs[0] |= 1;
            }
            if carry == 1 || nr.ge(&divisor) {
                nr = nr.wrapping_sub(divisor);
                q.set_bit(i);
            }
            r = nr;
        }
        (q, r)
    }

    /// `floor(self / divisor)` as `u128`, or `None` if it doesn't fit / divisor
    /// is zero.
    pub fn div_floor_u128(self, divisor: U256) -> Option<u128> {
        if divisor.is_zero() {
            return None;
        }
        let (q, _) = self.div_rem(divisor);
        q.as_u128()
    }

    /// `ceil(self / divisor)` as `u128`, or `None` if it doesn't fit / divisor
    /// is zero.
    pub fn div_ceil_u128(self, divisor: U256) -> Option<u128> {
        if divisor.is_zero() {
            return None;
        }
        let (q, r) = self.div_rem(divisor);
        let q = if r.is_zero() {
            q
        } else {
            q.checked_add(U256::ONE)?
        };
        q.as_u128()
    }

    /// `self >> 64`, discarding the low 64 bits.
    /// Used in Whirlpool negative-tick math and as a building block for shr96.
    #[inline]
    pub fn shr64(self) -> U256 {
        U256 {
            limbs: [self.limbs[1], self.limbs[2], self.limbs[3], 0],
        }
    }

    /// `self >> 96`, discarding the low 96 bits.
    /// Used in Whirlpool positive-tick math: each step multiplies by a Q96
    /// constant and shifts right 96 to stay in Q96 scale.
    #[inline]
    pub fn shr96(self) -> U256 {
        // Bit 96 of self is bit 32 of limbs[1]; new limbs[0] picks up bits
        // 96..159 (= limbs[1] high half + limbs[2] low half), etc.
        U256 {
            limbs: [
                (self.limbs[1] >> 32) | (self.limbs[2] << 32),
                (self.limbs[2] >> 32) | (self.limbs[3] << 32),
                self.limbs[3] >> 32,
                0,
            ],
        }
    }

    /// `floor(self / 2^32)` as `u128`, or `None` if the result doesn't fit.
    /// Used at the end of the Whirlpool positive-tick algorithm to convert from
    /// Q96 back to Q64.64.
    pub fn shr32_as_u128(self) -> Option<u128> {
        // After >> 32 the result occupies bits [32..160) of self.
        // Ensure the bits above bit 159 are all zero.
        if self.limbs[3] != 0 || (self.limbs[2] >> 32) != 0 {
            return None;
        }
        let lo = (self.limbs[0] >> 32) | (self.limbs[1] << 32);
        let hi = (self.limbs[1] >> 32) | (self.limbs[2] << 32);
        Some((hi as u128) << 64 | lo as u128)
    }

    /// `self << 64`, returning `None` if any bits above bit 191 would be lost
    /// (i.e. `self.limbs[3] != 0`). Used in Whirlpool swap math.
    pub fn checked_shl64(self) -> Option<U256> {
        if self.limbs[3] != 0 {
            return None;
        }
        Some(U256 {
            limbs: [0, self.limbs[0], self.limbs[1], self.limbs[2]],
        })
    }

    /// `self * b` (U256 × u128 → U256), returning `None` on 256-bit overflow.
    /// The four limbs of `self` are each multiplied by both 64-bit halves of
    /// `b`; partial products are accumulated with carry propagation.
    pub fn mul_u256_u128(self, b: u128) -> Option<U256> {
        let b0 = b as u64 as u128;
        let b1 = (b >> 64) as u64 as u128;

        // Five u128 accumulators (positions 0..4 in u64 units). Position 4 is
        // the overflow detector — must be 0 for a valid 256-bit result.
        let mut acc = [0u128; 5];
        for i in 0..4 {
            acc[i] += self.limbs[i] as u128 * b0;
            acc[i + 1] += self.limbs[i] as u128 * b1;
        }

        let mut out = [0u64; 4];
        let mut carry = 0u128;
        for i in 0..4 {
            let sum = acc[i] + carry;
            out[i] = sum as u64;
            carry = sum >> 64;
        }
        if acc[4] + carry != 0 {
            return None; // product overflows 256 bits
        }
        Some(U256 { limbs: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_and_as_u128_roundtrip() {
        for x in [0u128, 1, u64::MAX as u128, (u64::MAX as u128) + 1, u128::MAX] {
            assert_eq!(U256::from_u128(x).as_u128(), Some(x));
        }
    }

    #[test]
    fn mul_u128_matches_u128_when_small() {
        let cases = [
            (0u128, 0u128),
            (1, 1),
            (12345, 67890),
            (u32::MAX as u128, u32::MAX as u128),
            (1_000_000_000, 999_999_999),
        ];
        for (a, b) in cases {
            let got = U256::mul_u128(a, b).as_u128().unwrap();
            assert_eq!(got, a * b, "mul {a}*{b}");
        }
    }

    #[test]
    fn mul_u128_full_width() {
        // 2^96 * 2^96 = 2^192 — must not fit u128, must round-trip via div.
        let a = 1u128 << 96;
        let prod = U256::mul_u128(a, a);
        assert!(prod.as_u128().is_none(), "2^192 exceeds u128");
        // (2^192) / (2^96) = 2^96.
        let (q, r) = prod.div_rem(U256::from_u128(a));
        assert!(r.is_zero());
        assert_eq!(q.as_u128(), Some(a));
    }

    #[test]
    fn max_product_no_panic() {
        // u128::MAX * u128::MAX = (2^128-1)^2 = 2^256 - 2^129 + 1, fits in 256.
        let prod = U256::mul_u128(u128::MAX, u128::MAX);
        // Divide by u128::MAX → u128::MAX - 1 (since (m)^2 / m = m, but
        // (m^2)/m = m exactly; check exact).
        let (q, r) = prod.div_rem(U256::from_u128(u128::MAX));
        assert_eq!(q.as_u128(), Some(u128::MAX));
        assert!(r.is_zero());
    }

    #[test]
    fn div_rem_matches_u128() {
        let cases = [
            (100u128, 7u128),
            (1_000_000_000, 3),
            (u64::MAX as u128, 2),
            (u128::MAX, 999_999_937),
            (0, 5),
        ];
        for (n, d) in cases {
            let (q, r) = U256::from_u128(n).div_rem(U256::from_u128(d));
            assert_eq!(q.as_u128(), Some(n / d), "div {n}/{d}");
            assert_eq!(r.as_u128(), Some(n % d), "rem {n}%{d}");
        }
    }

    #[test]
    fn shl128_and_shr128() {
        let x = 0xDEAD_BEEFu64;
        let v = U256::shl128_u64(x);
        // (x << 128) >> 128 == x
        assert_eq!(v.shr128().as_u128(), Some(x as u128));
        // (x << 128) is exactly x * 2^128.
        let (q, r) = v.div_rem(U256::from_u128(1u128 << 64));
        assert!(r.is_zero());
        // q = x * 2^64
        assert_eq!(q.as_u128(), Some((x as u128) << 64));
    }

    #[test]
    fn checked_add_overflow() {
        let max = U256 {
            limbs: [u64::MAX; 4],
        };
        assert!(max.checked_add(U256::ONE).is_none());
        assert_eq!(
            U256::from_u128(5).checked_add(U256::from_u128(7)),
            Some(U256::from_u128(12))
        );
    }

    #[test]
    fn div_ceil_floor() {
        let n = U256::from_u128(100);
        let d = U256::from_u128(7);
        assert_eq!(n.div_floor_u128(d), Some(14)); // 100/7 = 14.28
        assert_eq!(n.div_ceil_u128(d), Some(15));
        // exact division: ceil == floor
        let n2 = U256::from_u128(98);
        assert_eq!(n2.div_floor_u128(d), Some(14));
        assert_eq!(n2.div_ceil_u128(d), Some(14));
    }
}
