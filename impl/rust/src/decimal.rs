//! Exact base-10 `decimal` / `numeric` (spec/design/decimal.md).
//!
//! A value is `(neg, coefficient, scale)` = `(-1)^neg · coefficient · 10^(-scale)`. The
//! coefficient is a hand-rolled big integer in base 10^9 limbs, **least-significant first**
//! (`limbs[0]` is the low 9 digits; the order is internal — only the rendered value and the
//! on-disk bytes are cross-core contracts, CLAUDE.md §2). No bignum crate (Rust has zero
//! runtime deps); the limb algorithm is the spec, identical across cores and pinned by shared
//! fixtures. Always **finite** (no NaN/±Infinity) and **normalized** (no high zero limbs, no
//! negative zero). Rounding is **half away from zero** (spec/design/decimal.md §3).

use crate::error::{EngineError, Result, SqlState};

/// Limb base: 10^9 (a `u32` limb holds 9 decimal digits; products fit `u64`).
const BASE: u64 = 1_000_000_000;
/// Decimal digits per limb.
const BASE_DIGITS: u32 = 9;
/// Max DECLARABLE precision of `numeric(p,s)`, and the division display-scale clamp —
/// spec/types/scalars.toml `max_precision` (PG `NUMERIC_MAX_PRECISION`, which is also its
/// `NUMERIC_MAX_DISPLAY_SCALE`). NOT a cap on what an unconstrained value may carry.
pub const MAX_PRECISION: u32 = 1000;
/// Max integer-part digits ANY value may carry — spec/types/scalars.toml `max_int_digits`
/// (PG `(NUMERIC_WEIGHT_MAX + 1) * DEC_DIGITS`; spec/design/decimal.md §2).
pub const MAX_INT_DIGITS: u32 = 131072;
/// Max digits after the point ANY value may carry — spec/types/scalars.toml `max_scale`
/// (PG `NUMERIC_DSCALE_MAX`; spec/design/decimal.md §2).
pub const MAX_SCALE: u32 = 16383;

/// An exact base-10 decimal. `neg` is the sign (always `false` for zero — no negative zero);
/// `scale` is the display scale; `limbs` is the coefficient magnitude (base 10^9, LSB-first,
/// no high zero limbs; empty == zero).
#[derive(Clone, Debug)]
pub struct Decimal {
    neg: bool,
    scale: u32,
    limbs: Vec<u32>,
}

fn overflow() -> EngineError {
    EngineError::new(
        SqlState::NumericValueOutOfRange,
        "value out of range for type decimal",
    )
}

fn div_by_zero() -> EngineError {
    EngineError::new(SqlState::DivisionByZero, "division by zero")
}

impl Decimal {
    /// Construct from raw parts, normalizing (trim high zero limbs; force `neg = false` for
    /// zero). The single choke-point every constructor ends with.
    fn from_parts(neg: bool, scale: u32, mut limbs: Vec<u32>) -> Decimal {
        mag_trim(&mut limbs);
        let neg = neg && !limbs.is_empty();
        Decimal { neg, scale, limbs }
    }

    /// Zero at the given display scale.
    pub fn zero(scale: u32) -> Decimal {
        Decimal {
            neg: false,
            scale,
            limbs: Vec::new(),
        }
    }

    /// Exact decimal from an integer (the lossless `int → decimal` cast, scale 0).
    pub fn from_i64(v: i64) -> Decimal {
        let neg = v < 0;
        let mag = v.unsigned_abs(); // i64::MIN handled (|MIN| = 2^63 fits u64)
        Decimal::from_parts(neg, 0, mag_from_u64(mag))
    }

    /// Build from a sign, an unscaled coefficient as a decimal-digit string (leading zeros
    /// allowed), and a scale. The literal/parse entry point — does NOT enforce the
    /// precision/scale caps (the caller checks them at resolve, trapping 22003).
    pub fn from_digits_scale(neg: bool, digits: &str, scale: u32) -> Decimal {
        Decimal::from_parts(neg, scale, mag_from_decimal_str(digits))
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Number of significant digits in the coefficient (0 for zero).
    pub fn precision(&self) -> u32 {
        mag_digit_count(&self.limbs)
    }

    /// Trap 22003 if this (unconstrained) value exceeds the numeric-format caps
    /// (spec/design/decimal.md §2): more than `MAX_INT_DIGITS` integer-part digits or a
    /// scale over `MAX_SCALE` — PG's `make_result` / `numeric_in` checks.
    pub fn check_cap(self) -> Result<Decimal> {
        if self.precision().saturating_sub(self.scale) > MAX_INT_DIGITS || self.scale > MAX_SCALE {
            return Err(overflow());
        }
        Ok(self)
    }

    /// The value-canonical form `(neg, coeff, scale)` with trailing *fractional* zeros
    /// stripped (scale reduced): `1.50 → (false,15,1)`, `10.0 → (false,10,0)`, `100` stays
    /// `(false,100,0)`. Two values equal in value share this form — the key for equality,
    /// hashing, and DISTINCT/GROUP BY (spec/design/decimal.md §5).
    fn canonical(&self) -> (bool, Vec<u32>, u32) {
        if self.limbs.is_empty() {
            return (false, Vec::new(), 0);
        }
        let mut digits = mag_to_decimal_str(&self.limbs);
        let mut scale = self.scale;
        while scale > 0 && digits.ends_with('0') {
            digits.pop();
            scale -= 1;
        }
        (self.neg, mag_from_decimal_str(&digits), scale)
    }

    /// Total order over finite decimals by value (sign then scale-aligned magnitude).
    pub fn cmp_value(&self, other: &Decimal) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.neg != other.neg {
            // neg (true) sorts below non-neg (false); zero is non-neg.
            return if self.neg {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs, s - self.scale);
        let b = mag_mul_pow10(&other.limbs, s - other.scale);
        let m = mag_cmp(&a, &b);
        if self.neg { m.reverse() } else { m }
    }

    /// Render to the canonical decimal string (spec/design/decimal.md §6): optional `-`, the
    /// integer digits, and — iff `scale > 0` — `.` and exactly `scale` fractional digits.
    pub fn render(&self) -> String {
        let digits = mag_to_decimal_str(&self.limbs); // "0" for zero
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        if self.scale == 0 {
            out.push_str(&digits);
            return out;
        }
        let scale = self.scale as usize;
        // Left-pad so there is at least one integer digit and `scale` fractional digits.
        let want = scale + 1;
        let padded = if digits.len() < want {
            format!("{}{}", "0".repeat(want - digits.len()), digits)
        } else {
            digits
        };
        let point = padded.len() - scale;
        out.push_str(&padded[..point]);
        out.push('.');
        out.push_str(&padded[point..]);
        out
    }

    /// Arithmetic negation (sign flip; zero stays `+0`).
    pub fn neg(&self) -> Decimal {
        Decimal::from_parts(!self.neg, self.scale, self.limbs.clone())
    }

    /// `self + other`, exact, result scale `max(s1,s2)`; traps 22003 at the cap.
    pub fn add(&self, other: &Decimal) -> Result<Decimal> {
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs, s - self.scale);
        let b = mag_mul_pow10(&other.limbs, s - other.scale);
        let r = if self.neg == other.neg {
            Decimal::from_parts(self.neg, s, mag_add(&a, &b))
        } else {
            match mag_cmp(&a, &b) {
                std::cmp::Ordering::Equal => Decimal::zero(s),
                std::cmp::Ordering::Greater => Decimal::from_parts(self.neg, s, mag_sub(&a, &b)),
                std::cmp::Ordering::Less => Decimal::from_parts(other.neg, s, mag_sub(&b, &a)),
            }
        };
        r.check_cap()
    }

    /// `self - other` (= `self + (-other)`).
    pub fn sub(&self, other: &Decimal) -> Result<Decimal> {
        self.add(&other.neg())
    }

    /// `self * other`, exact, result scale `s1 + s2`; traps 22003 at the integer-digit cap.
    /// A product scale over `MAX_SCALE` ROUNDS to it instead of trapping (PG `numeric_mul`
    /// rounds the exact product — spec/design/decimal.md §2).
    pub fn mul(&self, other: &Decimal) -> Result<Decimal> {
        let scale = self.scale + other.scale;
        let neg = self.neg ^ other.neg;
        let exact = Decimal::from_parts(neg, scale, mag_mul(&self.limbs, &other.limbs));
        let r = if scale > MAX_SCALE {
            exact.round_to_scale(MAX_SCALE)
        } else {
            exact
        };
        r.check_cap()
    }

    /// `self / other` with PostgreSQL's `select_div_scale` result scale, rounded half away
    /// from zero (spec/design/decimal.md §4). Traps 22012 on a zero divisor, 22003 at the cap.
    pub fn div(&self, other: &Decimal) -> Result<Decimal> {
        if other.is_zero() {
            return Err(div_by_zero());
        }
        if self.is_zero() {
            // 0 / x = 0; result scale per the rule still applies but the value is zero.
            let rscale = select_div_scale(self, other);
            return Ok(Decimal::zero(rscale));
        }
        let rscale = select_div_scale(self, other);
        // numerator = |C1| * 10^E, E = rscale + s2 - s1 (>= 0 since rscale >= s1).
        let e = rscale as i64 + other.scale as i64 - self.scale as i64;
        debug_assert!(e >= 0);
        let numer = mag_mul_pow10(&self.limbs, e as u32);
        let (mut q, r) = mag_divmod(&numer, &other.limbs);
        // Round half away from zero: if 2*r >= |divisor|, round the magnitude up.
        if mag_cmp(&mag_add(&r, &r), &other.limbs) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg ^ other.neg, rscale, q).check_cap()
    }

    /// `self % other` — remainder of truncated division; result scale `max(s1,s2)`, sign of
    /// the dividend (matches the integer `%`). Traps 22012 on a zero divisor.
    pub fn rem(&self, other: &Decimal) -> Result<Decimal> {
        if other.is_zero() {
            return Err(div_by_zero());
        }
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs, s - self.scale);
        let b = mag_mul_pow10(&other.limbs, s - other.scale);
        let (_q, r) = mag_divmod(&a, &b);
        // Remainder takes the dividend's sign; zero normalizes to +0.
        Ok(Decimal::from_parts(self.neg, s, r))
    }

    /// Round to `target` scale, half away from zero (spec/design/decimal.md §3). Increasing
    /// the scale only appends zeros (exact).
    pub fn round_to_scale(&self, target: u32) -> Decimal {
        if target >= self.scale {
            let limbs = mag_mul_pow10(&self.limbs, target - self.scale);
            return Decimal::from_parts(self.neg, target, limbs);
        }
        let d = self.scale - target;
        let pow = mag_pow10(d);
        let (mut q, r) = mag_divmod(&self.limbs, &pow);
        // Half away: if 2*r >= 10^d, round the magnitude up.
        if mag_cmp(&mag_add(&r, &r), &pow) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg, target, q)
    }

    /// The magnitude (`abs`), preserving scale — the `abs(numeric)` scalar function
    /// (spec/design/functions.md §9). Cannot overflow.
    pub fn abs(&self) -> Decimal {
        Decimal::from_parts(false, self.scale, self.limbs.clone())
    }

    /// PG `round(numeric, n)` (spec/design/functions.md §9): round half away from zero to `n`
    /// fractional places. `n >= 0` rounds to scale `n` (delegating to `round_to_scale`, with
    /// `n` clamped at `MAX_SCALE` like PG `numeric_round`); `n < 0` rounds to the LEFT of the
    /// point — result scale 0, value a multiple of `10^-n` (`round(150, -2) = 200`).
    /// `round(x)` is `round_places(0)`. Traps 22003 when the round-up carry pushes a value at
    /// the integer-digit cap over it (spec/design/decimal.md §4).
    pub fn round_places(&self, n: i64) -> Result<Decimal> {
        if n >= 0 {
            return self
                .round_to_scale(n.min(MAX_SCALE as i64) as u32)
                .check_cap();
        }
        // Drop `self.scale + k` digits of the magnitude (rounding half away), then re-append
        // the k integer zeros. `k` is capped at the digit count + 1: beyond that every value
        // rounds to 0 (or a single carried `1`), so the clamp changes no result but bounds work.
        let k = n.unsigned_abs().min((self.precision() + 1) as u64) as u32;
        let drop = self.scale + k;
        let pow = mag_pow10(drop);
        let (mut q, r) = mag_divmod(&self.limbs, &pow);
        if mag_cmp(&mag_add(&r, &r), &pow) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        let scaled = mag_mul_pow10(&q, k);
        Decimal::from_parts(self.neg, 0, scaled).check_cap()
    }

    /// Coerce into `numeric(precision, scale)`: round to `scale` (half away), then trap 22003
    /// if the integer-part digits exceed `precision - scale` (spec/design/decimal.md §2).
    pub fn coerce_to_typmod(&self, precision: u32, scale: u32) -> Result<Decimal> {
        let rounded = self.round_to_scale(scale);
        // Integer-part digit count = sig digits beyond the scale (0 for a value < 1 or zero).
        let sig = rounded.precision();
        let int_digits = sig.saturating_sub(scale);
        if int_digits > precision - scale {
            return Err(overflow());
        }
        Ok(rounded)
    }

    /// Round to an integer (scale 0, half away) and return it as `i64` if it fits, else
    /// `None` (the `decimal → int` cast; the caller range-checks against the target int type).
    pub fn to_i64_round(&self) -> Option<i64> {
        let r = self.round_to_scale(0);
        let mut v: i128 = 0;
        for &limb in r.limbs.iter().rev() {
            v = v.checked_mul(BASE as i128)?.checked_add(limb as i128)?;
            if v > (i64::MAX as i128) + 1 {
                return None;
            }
        }
        let signed = if r.neg { -v } else { v };
        i64::try_from(signed).ok()
    }

    // --- on-disk value codec (base-10^4 groups, MS-first) — spec/fileformat/format.md ---

    /// `(neg, scale, base-10^4 coefficient groups MS-first)` for the value codec. Zero → no
    /// groups.
    pub fn to_codec(&self) -> (bool, u32, Vec<u16>) {
        (self.neg, self.scale, mag_to_nbase4(&self.limbs))
    }

    /// Inverse of `to_codec` (used on load). `neg` is forced false for zero by normalize.
    pub fn from_codec(neg: bool, scale: u32, groups: &[u16]) -> Decimal {
        Decimal::from_parts(neg, scale, mag_from_nbase4(groups))
    }
}

// Value-canonical equality / hashing (1.5 == 1.50; spec/design/decimal.md §5).
impl PartialEq for Decimal {
    fn eq(&self, other: &Decimal) -> bool {
        self.canonical() == other.canonical()
    }
}
impl Eq for Decimal {}
impl std::hash::Hash for Decimal {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.canonical().hash(state);
    }
}

/// PG `select_div_scale` (spec/design/decimal.md §4): at least 16 significant quotient digits,
/// no fewer fractional digits than either input, in PG's base-10^4 (DEC_DIGITS = 4) units.
fn select_div_scale(a: &Decimal, b: &Decimal) -> u32 {
    let (w1, f1) = nbase4_weight_lead(a);
    let (w2, f2) = nbase4_weight_lead(b);
    let mut qweight = w1 - w2;
    if f1 <= f2 {
        qweight -= 1;
    }
    let mut rscale = 16 - 4 * qweight;
    rscale = rscale.max(a.scale as i64).max(b.scale as i64).max(0);
    // PG's display-scale clamp: NUMERIC_MAX_DISPLAY_SCALE = NUMERIC_MAX_PRECISION (1000),
    // deliberately NOT the MAX_SCALE value cap (spec/design/decimal.md §4).
    rscale = rscale.min(MAX_PRECISION as i64);
    rscale as u32
}

/// For a decimal value, its PG base-10^4 `weight` (the power of 10^4 of the most-significant
/// digit group) and the leading group `f` (0..=9999). Zero → (0, 0).
fn nbase4_weight_lead(d: &Decimal) -> (i64, i64) {
    if d.is_zero() {
        return (0, 0);
    }
    let digits = d.precision() as i64; // significant decimal digit count
    let e = digits - 1 - d.scale as i64; // exponent of the leading significant digit
    let w = e.div_euclid(4); // floor(e / 4)
    let g = (e - 4 * w + 1) as usize; // 1..=4 leading-group decimal digits
    let s = mag_to_decimal_str(&d.limbs);
    let bytes = s.as_bytes();
    let mut f: i64 = 0;
    for i in 0..g {
        let digit = if i < bytes.len() {
            (bytes[i] - b'0') as i64
        } else {
            0
        };
        f = f * 10 + digit;
    }
    (w, f)
}

// ============================================================================
// `decimal_work` group counts (spec/design/cost.md §3 "decimal_work") — an operation's
// work W in base-10⁴ digit groups, computed from LOGICAL significant-digit counts, never
// from this core's internal limb count (the cross-core contract, decimal.md §7 #11).
// All return W >= 1; the evaluator charges `decimal_work` × (W − 1).
// ============================================================================

/// `max(1, ceil(n/4))` — the base-10⁴ group count of an `n`-digit coefficient.
fn groups(n: u32) -> u64 {
    (n as u64).div_ceil(4).max(1)
}

/// Both operands' digit counts after aligning to the common scale `max(s1, s2)` (the digit
/// count once the lower-scale coefficient is multiplied up — exactly the add/sub/cmp work).
fn aligned_digits(a: &Decimal, b: &Decimal) -> (u32, u32) {
    let s = a.scale.max(b.scale);
    (a.precision() + (s - a.scale), b.precision() + (s - b.scale))
}

/// W for add/sub/compare: the larger aligned operand.
pub fn work_linear(a: &Decimal, b: &Decimal) -> u64 {
    let (a1, a2) = aligned_digits(a, b);
    groups(a1).max(groups(a2))
}

/// W for mul: the product of the (unaligned) operand group counts — schoolbook-quadratic.
pub fn work_mul(a: &Decimal, b: &Decimal) -> u64 {
    groups(a.precision()) * groups(b.precision())
}

/// W for div: numerator groups (dividend digits + the rescale shift `E`) × divisor groups,
/// `E = rscale + s2 − s1` with the same `select_div_scale` as the result. A zero divisor
/// returns 1 — the operation traps 22012 before any work (cost.md §3).
pub fn work_div(a: &Decimal, b: &Decimal) -> u64 {
    if b.is_zero() {
        return 1;
    }
    let rscale = select_div_scale(a, b);
    let e = rscale as i64 + b.scale as i64 - a.scale as i64; // >= 0 since rscale >= s1
    groups(a.precision() + e as u32) * groups(b.precision())
}

/// W for mod: the aligned divmod — the product of the aligned group counts. Zero divisor → 1.
pub fn work_mod(a: &Decimal, b: &Decimal) -> u64 {
    if b.is_zero() {
        return 1;
    }
    let (a1, a2) = aligned_digits(a, b);
    groups(a1) * groups(a2)
}

// ============================================================================
// Magnitude helpers — base 10^9, LSB-first, normalized (no high zero limbs).
// ============================================================================

fn mag_trim(limbs: &mut Vec<u32>) {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
}

fn mag_from_u64(mut v: u64) -> Vec<u32> {
    let mut out = Vec::new();
    while v != 0 {
        out.push((v % BASE) as u32);
        v /= BASE;
    }
    out
}

/// Parse a decimal-digit string (leading zeros allowed) into LSB-first base-10^9 limbs.
fn mag_from_decimal_str(s: &str) -> Vec<u32> {
    let bytes: Vec<u8> = s.bytes().filter(|b| b.is_ascii_digit()).collect();
    let mut out = Vec::new();
    let mut end = bytes.len();
    while end > 0 {
        let start = end.saturating_sub(BASE_DIGITS as usize);
        let mut limb: u32 = 0;
        for &b in &bytes[start..end] {
            limb = limb * 10 + (b - b'0') as u32;
        }
        out.push(limb);
        end = start;
    }
    mag_trim(&mut out);
    out
}

/// LSB-first limbs → a decimal-digit string with no leading zeros ("0" for zero).
fn mag_to_decimal_str(limbs: &[u32]) -> String {
    if limbs.is_empty() {
        return "0".to_string();
    }
    let mut out = String::new();
    // Highest limb without leading zeros, the rest zero-padded to 9 digits.
    out.push_str(&limbs[limbs.len() - 1].to_string());
    for &limb in limbs[..limbs.len() - 1].iter().rev() {
        out.push_str(&format!("{limb:09}"));
    }
    out
}

/// Number of significant decimal digits (0 for zero).
fn mag_digit_count(limbs: &[u32]) -> u32 {
    if limbs.is_empty() {
        return 0;
    }
    let high = limbs[limbs.len() - 1].to_string().len() as u32;
    high + (limbs.len() as u32 - 1) * BASE_DIGITS
}

fn mag_cmp(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry: u64 = 0;
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0) as u64;
        let y = *b.get(i).unwrap_or(&0) as u64;
        let sum = x + y + carry;
        out.push((sum % BASE) as u32);
        carry = sum / BASE;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    mag_trim(&mut out);
    out
}

/// `a - b` assuming `a >= b`.
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow: i64 = 0;
    for i in 0..a.len() {
        let x = a[i] as i64;
        let y = *b.get(i).unwrap_or(&0) as i64;
        let mut diff = x - y - borrow;
        if diff < 0 {
            diff += BASE as i64;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(diff as u32);
    }
    mag_trim(&mut out);
    out
}

fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; a.len() + b.len()];
    for (i, &ai) in a.iter().enumerate() {
        let mut carry: u64 = 0;
        for (j, &bj) in b.iter().enumerate() {
            let cur = out[i + j] + ai as u64 * bj as u64 + carry;
            out[i + j] = cur % BASE;
            carry = cur / BASE;
        }
        out[i + b.len()] += carry;
    }
    let mut res: Vec<u32> = out.iter().map(|&x| x as u32).collect();
    mag_trim(&mut res);
    res
}

/// Multiply a magnitude by a single small factor `s` (0 <= s < BASE).
fn mag_mul_small(a: &[u32], s: u64) -> Vec<u32> {
    if s == 0 || a.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry: u64 = 0;
    for &ai in a {
        let cur = ai as u64 * s + carry;
        out.push((cur % BASE) as u32);
        carry = cur / BASE;
    }
    while carry != 0 {
        out.push((carry % BASE) as u32);
        carry /= BASE;
    }
    mag_trim(&mut out);
    out
}

/// Multiply by 10^e.
fn mag_mul_pow10(a: &[u32], e: u32) -> Vec<u32> {
    if a.is_empty() || e == 0 {
        return a.to_vec();
    }
    let full = (e / BASE_DIGITS) as usize;
    let rem = e % BASE_DIGITS;
    let mut shifted = Vec::with_capacity(a.len() + full + 1);
    shifted.resize(full, 0); // prepend `full` zero limbs = * BASE^full
    shifted.extend_from_slice(a);
    if rem > 0 {
        shifted = mag_mul_small(&shifted, 10u64.pow(rem));
    }
    mag_trim(&mut shifted);
    shifted
}

/// 10^e as a magnitude.
fn mag_pow10(e: u32) -> Vec<u32> {
    mag_mul_pow10(&[1], e)
}

/// Long division: `(quotient, remainder)` of `num / den` (den != 0). Each quotient limb is
/// found by binary search in `[0, BASE)` — boring and identical across cores.
fn mag_divmod(num: &[u32], den: &[u32]) -> (Vec<u32>, Vec<u32>) {
    debug_assert!(!den.is_empty());
    if mag_cmp(num, den) == std::cmp::Ordering::Less {
        return (Vec::new(), num.to_vec());
    }
    let mut quo = vec![0u32; num.len()];
    let mut rem: Vec<u32> = Vec::new();
    for i in (0..num.len()).rev() {
        // rem = rem * BASE + num[i]  (shift up one limb, set the low limb).
        rem.insert(0, num[i]);
        mag_trim(&mut rem);
        // Largest q in [0, BASE) with den*q <= rem.
        let (mut lo, mut hi) = (0u64, BASE - 1);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if mag_cmp(&mag_mul_small(den, mid), &rem) != std::cmp::Ordering::Greater {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        quo[i] = lo as u32;
        rem = mag_sub(&rem, &mag_mul_small(den, lo));
    }
    mag_trim(&mut quo);
    (quo, rem)
}

/// LSB-first base-10^9 limbs → MS-first base-10^4 groups (the on-disk codec). Zero → empty.
fn mag_to_nbase4(limbs: &[u32]) -> Vec<u16> {
    if limbs.is_empty() {
        return Vec::new();
    }
    let s = mag_to_decimal_str(limbs);
    let bytes = s.as_bytes();
    // Pad on the left to a multiple of 4, then split into 4-digit groups MS-first.
    let pad = (4 - bytes.len() % 4) % 4;
    let mut padded = vec![b'0'; pad];
    padded.extend_from_slice(bytes);
    let mut out = Vec::with_capacity(padded.len() / 4);
    let mut i = 0;
    while i < padded.len() {
        let mut g: u16 = 0;
        for &b in &padded[i..i + 4] {
            g = g * 10 + (b - b'0') as u16;
        }
        out.push(g);
        i += 4;
    }
    out
}

/// MS-first base-10^4 groups → LSB-first base-10^9 limbs (inverse of `mag_to_nbase4`).
fn mag_from_nbase4(groups: &[u16]) -> Vec<u32> {
    if groups.is_empty() {
        return Vec::new();
    }
    let mut s = groups[0].to_string();
    for &g in &groups[1..] {
        s.push_str(&format!("{g:04}"));
    }
    mag_from_decimal_str(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        // Test helper: parse "[-]int[.frac]" into a Decimal (mirrors the lexer/parser).
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int, frac) = match body.split_once('.') {
            Some((i, f)) => (i, f),
            None => (body, ""),
        };
        let digits = format!("{int}{frac}");
        Decimal::from_digits_scale(neg, &digits, frac.len() as u32)
    }

    #[test]
    fn render_preserves_scale() {
        assert_eq!(dec("1.50").render(), "1.50");
        assert_eq!(dec("1.5").render(), "1.5");
        assert_eq!(dec("0.00").render(), "0.00");
        assert_eq!(dec("0").render(), "0");
        assert_eq!(dec("-0.013").render(), "-0.013");
        assert_eq!(dec("123").render(), "123");
        assert_eq!(dec(".5").render(), "0.5");
        assert_eq!(dec("100").render(), "100");
    }

    #[test]
    fn no_negative_zero() {
        assert!(!dec("0").is_negative());
        assert!(!dec("-0").is_negative());
        assert!(!dec("-0.00").is_negative());
        assert_eq!(dec("1.0").sub(&dec("1.0")).unwrap().render(), "0.0");
        assert!(!dec("1.0").sub(&dec("1.0")).unwrap().is_negative());
    }

    #[test]
    fn value_equality_ignores_scale() {
        assert_eq!(dec("1.5"), dec("1.50"));
        assert_eq!(dec("10"), dec("10.0"));
        assert_ne!(dec("1.5"), dec("1.6"));
        assert_eq!(
            dec("1.5").cmp_value(&dec("1.50")),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn ordering() {
        use std::cmp::Ordering::*;
        assert_eq!(dec("-10").cmp_value(&dec("-1")), Less);
        assert_eq!(dec("-1").cmp_value(&dec("0")), Less);
        assert_eq!(dec("0").cmp_value(&dec("0.5")), Less);
        assert_eq!(dec("0.5").cmp_value(&dec("1")), Less);
        assert_eq!(dec("1.23").cmp_value(&dec("1.2")), Greater);
    }

    #[test]
    fn add_sub_scale() {
        assert_eq!(dec("1.50").add(&dec("1.5")).unwrap().render(), "3.00");
        assert_eq!(dec("1.234").sub(&dec("1.2")).unwrap().render(), "0.034");
        assert_eq!(dec("-5").add(&dec("3")).unwrap().render(), "-2");
        assert_eq!(dec("5").sub(&dec("8")).unwrap().render(), "-3");
    }

    #[test]
    fn mul_scale() {
        assert_eq!(dec("1.50").mul(&dec("1.5")).unwrap().render(), "2.250");
        assert_eq!(dec("2.0").mul(&dec("3.000")).unwrap().render(), "6.0000");
        assert_eq!(dec("-2").mul(&dec("3")).unwrap().render(), "-6");
    }

    #[test]
    fn division_scale_and_rounding() {
        assert_eq!(
            dec("1").div(&dec("3")).unwrap().render(),
            "0.33333333333333333333"
        );
        assert_eq!(
            dec("2").div(&dec("3")).unwrap().render(),
            "0.66666666666666666667"
        );
        assert_eq!(
            dec("1").div(&dec("7")).unwrap().render(),
            "0.14285714285714285714"
        );
        assert_eq!(
            dec("10.0").div(&dec("4.0")).unwrap().render(),
            "2.5000000000000000"
        );
        // f1=1 <= f2=8 ⇒ qweight=-1 ⇒ rscale=20 (the PG NBASE-4 granularity, decimal.md §4).
        assert_eq!(
            dec("1.0").div(&dec("8.0")).unwrap().render(),
            "0.12500000000000000000"
        );
        assert_eq!(
            dec("100").div(&dec("7")).unwrap().render(),
            "14.2857142857142857"
        );
    }

    #[test]
    fn modulo() {
        assert_eq!(dec("5.5").rem(&dec("2")).unwrap().render(), "1.5");
        assert_eq!(dec("-5.5").rem(&dec("2")).unwrap().render(), "-1.5");
        assert_eq!(dec("5.50").rem(&dec("2.0")).unwrap().render(), "1.50");
    }

    #[test]
    fn rounding_half_away() {
        assert_eq!(dec("0.125").round_to_scale(2).render(), "0.13");
        assert_eq!(dec("-0.125").round_to_scale(2).render(), "-0.13");
        assert_eq!(dec("2.5").round_to_scale(0).render(), "3");
        assert_eq!(dec("-2.5").round_to_scale(0).render(), "-3");
        assert_eq!(dec("2.45").round_to_scale(1).render(), "2.5");
        assert_eq!(dec("9.5").round_to_scale(0).render(), "10");
    }

    #[test]
    fn coerce_typmod() {
        assert_eq!(dec("1.5").coerce_to_typmod(10, 2).unwrap().render(), "1.50");
        assert_eq!(
            dec("1.555").coerce_to_typmod(10, 2).unwrap().render(),
            "1.56"
        );
        // integer part exceeds p - s = 1 → overflow
        assert!(dec("12.34").coerce_to_typmod(3, 2).is_err());
        assert_eq!(dec("0").coerce_to_typmod(1, 1).unwrap().render(), "0.0");
    }

    #[test]
    fn div_zero_traps() {
        assert_eq!(dec("1").div(&dec("0")).unwrap_err().code(), "22012");
        assert_eq!(dec("1").rem(&dec("0")).unwrap_err().code(), "22012");
    }

    #[test]
    fn decimal_to_int_rounds() {
        assert_eq!(dec("2.5").to_i64_round(), Some(3));
        assert_eq!(dec("-2.5").to_i64_round(), Some(-3));
        assert_eq!(dec("2.4").to_i64_round(), Some(2));
        assert_eq!(dec("100").to_i64_round(), Some(100));
        assert_eq!(
            dec("100000000000000000000000000000").to_i64_round(),
            None,
            "out of i64 range"
        );
    }

    #[test]
    fn int_to_decimal() {
        assert_eq!(Decimal::from_i64(5).render(), "5");
        assert_eq!(Decimal::from_i64(-7).render(), "-7");
        assert_eq!(Decimal::from_i64(i64::MIN).render(), "-9223372036854775808");
        assert_eq!(Decimal::from_i64(0).render(), "0");
    }

    #[test]
    fn codec_round_trip() {
        for s in [
            "0",
            "1.50",
            "-12345.6789",
            "100000000.000001",
            "999999999999",
        ] {
            let d = dec(s);
            let (neg, scale, groups) = d.to_codec();
            let back = Decimal::from_codec(neg, scale, &groups);
            assert_eq!(back.render(), d.render(), "codec round trip {s}");
        }
        // Zero carries no groups.
        assert_eq!(dec("0.00").to_codec().2.len(), 0);
    }

    #[test]
    fn big_multiplication_exact() {
        // 38-digit * 38-digit fits no i128; the limb path is exact.
        let a = dec("12345678901234567890123456789012345678");
        let b = dec("99999999999999999999999999999999999999");
        let p = a.mul(&b).unwrap();
        assert_eq!(p.precision(), 76);
    }

    #[test]
    fn caps_match_spec() {
        // Cross-checked against spec/types/scalars.toml in the types cross-check test.
        assert_eq!(MAX_PRECISION, 1000);
        assert_eq!(MAX_SCALE, 16383);
        assert_eq!(MAX_INT_DIGITS, 131072);
    }
}
