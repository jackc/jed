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

/// Magnitude clamp for a decimal literal's scientific `e`-notation exponent. Tied to the
/// format caps so lexing/parsing stays bounded — `1e9999999999` must not materialize a
/// gigabyte of coefficient zeros — without changing any outcome: an exponent this large
/// already drives the value past the caps (so it traps `22003` at resolve), and a zero
/// coefficient still normalizes to `0` (spec/design/grammar.md §14). Callers clamp the
/// exponent magnitude to `±EXP_LIMIT` while scanning, both to honor this bound and to keep
/// the accumulation inside `i64`.
pub const EXP_LIMIT: i64 = MAX_INT_DIGITS as i64 + MAX_SCALE as i64 + 2;

/// Canonical `(coefficient-digits, scale)` for a decimal literal, from its mantissa
/// (`int_part` + `frac`) and an optional scientific exponent (already clamped to `±EXP_LIMIT`
/// by the caller's scanner). The display scale is `max(0, frac_len − exp)`; when the exponent
/// drives it below zero the coefficient absorbs the surplus as trailing zeros at scale 0, so
/// the value still reads `coefficient × 10^(−scale)`. Shared by the lexer (bare `1.5e3`) and
/// the text→decimal coercion (`numeric '1.5e3'`) so both spell the SAME value
/// (spec/design/grammar.md §14). The result is fed to [`Decimal::from_digits_scale`] and
/// cap-checked at resolve.
pub fn decimal_from_parts(int_part: &str, frac: &str, exp: Option<i64>) -> (String, u32) {
    let frac_len = frac.len() as i64;
    let Some(exp) = exp else {
        return (format!("{int_part}{frac}"), frac_len as u32);
    };
    let eff_scale = frac_len - exp;
    if eff_scale >= 0 {
        (format!("{int_part}{frac}"), eff_scale as u32)
    } else {
        let zeros = (-eff_scale) as usize;
        let mut digits = String::with_capacity(int_part.len() + frac.len() + zeros);
        digits.push_str(int_part);
        digits.push_str(frac);
        digits.extend(std::iter::repeat('0').take(zeros));
        (digits, 0)
    }
}

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

    /// The EXACT base-10 decimal equal to a finite binary64 (spec/design/float.md §6, the
    /// `float → decimal` cast). A binary64 is `M · 2^E` for an integer significand `M` and
    /// exponent `E`: for `E ≥ 0` the value is the integer `M · 2^E` (scale 0); for `E < 0` it
    /// is `M · 5^(-E)` with scale `-E` (since `2^E = 5^(-E) / 10^(-E)`), an exact terminating
    /// decimal. Computed with the module's own hand-rolled limb arithmetic (`mag_mul_small`
    /// by 2 or 5) — NO float formatting, NO bignum crate — so it is identical bit-for-bit
    /// across cores (the value matches Go's `exactDecimalFromFloat64`). The caller checks
    /// finiteness (NaN/±Inf → 22003) and applies the target typmod's scale coercion.
    pub fn from_float64(f: f64) -> Decimal {
        if f == 0.0 {
            return Decimal::zero(0); // +0 and -0 both → exact 0
        }
        let bits = f.to_bits();
        let neg = (bits >> 63) != 0;
        let raw_exp = ((bits >> 52) & 0x7ff) as i64;
        let mut mant = bits & ((1u64 << 52) - 1);
        let exp: i64 = if raw_exp == 0 {
            // Subnormal: no implicit leading 1; the true exponent is fixed at -1074.
            -1074
        } else {
            mant |= 1u64 << 52; // restore the implicit leading 1
            raw_exp - 1075 // unbias 1023 + shift out the 52 mantissa bits
        };
        let mut mag = mag_from_u64(mant);
        if exp >= 0 {
            // value = M · 2^exp (an integer, scale 0). Multiply the magnitude by 2 `exp` times.
            for _ in 0..exp {
                mag = mag_mul_small(&mag, 2);
            }
            return Decimal::from_parts(neg, 0, mag);
        }
        // value = M · 5^|exp| with scale |exp| (since 2^exp = 5^|exp| / 10^|exp|).
        let k = (-exp) as u32;
        for _ in 0..k {
            mag = mag_mul_small(&mag, 5);
        }
        // Normalize to the minimal display scale (trim trailing fractional zeros): the value is
        // unchanged but the rendered form matches PG's float8->numeric (0.5, not 0.500…0). This
        // is exact — only zero digits are removed — via the canonical form.
        let (cneg, cdigits, cscale) = Decimal::from_parts(neg, k, mag).canonical();
        Decimal::from_parts(cneg, cscale, cdigits)
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

    /// `self + other`, exact, result scale `max(s1,s2)`, **without** the §2 format-cap check —
    /// the running form for the SUM/AVG accumulator, which (like PG) checks the cap only on the
    /// FINAL result, not each intermediate (spec/design/decimal.md §2, determinism.md §7). That
    /// makes the trap order-independent: whether a fold overflows no longer depends on the order
    /// rows are summed. Standalone arithmetic uses `add` (capped).
    pub fn add_uncapped(&self, other: &Decimal) -> Decimal {
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs, s - self.scale);
        let b = mag_mul_pow10(&other.limbs, s - other.scale);
        if self.neg == other.neg {
            Decimal::from_parts(self.neg, s, mag_add(&a, &b))
        } else {
            match mag_cmp(&a, &b) {
                std::cmp::Ordering::Equal => Decimal::zero(s),
                std::cmp::Ordering::Greater => Decimal::from_parts(self.neg, s, mag_sub(&a, &b)),
                std::cmp::Ordering::Less => Decimal::from_parts(other.neg, s, mag_sub(&b, &a)),
            }
        }
    }

    /// `self + other`, exact, result scale `max(s1,s2)`; traps 22003 at the cap.
    pub fn add(&self, other: &Decimal) -> Result<Decimal> {
        self.add_uncapped(other).check_cap()
    }

    /// `self - other` (= `self + (-other)`).
    pub fn sub(&self, other: &Decimal) -> Result<Decimal> {
        self.add(&other.neg())
    }

    /// `self - other`, exact, result scale `max(s1,s2)`, WITHOUT the cap check (the transcendental
    /// kernels' running form — PG checks the cap only at make_result, §8 / decimal.md §2).
    fn sub_uncapped(&self, other: &Decimal) -> Decimal {
        self.add_uncapped(&other.neg())
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

    /// Truncate toward zero to `target` scale — drop the dropped fractional digits, no rounding.
    /// Increasing the scale only appends zeros (exact). Truncation never grows the magnitude, so
    /// it cannot overflow. The toward-zero core of `trunc` (spec/design/functions.md §9).
    pub fn trunc_to_scale(&self, target: u32) -> Decimal {
        if target >= self.scale {
            let limbs = mag_mul_pow10(&self.limbs, target - self.scale);
            return Decimal::from_parts(self.neg, target, limbs);
        }
        let pow = mag_pow10(self.scale - target);
        let (q, _r) = mag_divmod(&self.limbs, &pow);
        Decimal::from_parts(self.neg, target, q)
    }

    /// PG `trunc(numeric, n)` (spec/design/functions.md §9): truncate toward zero to `n` fractional
    /// places. `n >= 0` truncates to scale `n` (`trunc(1.567, 2) = 1.56`, clamped at `MAX_SCALE`);
    /// `n < 0` truncates to the LEFT of the point — result scale 0, a multiple of `10^-n`
    /// (`trunc(1234.5, -2) = 1200`). `trunc(x)` is `trunc_places(0)`. Cannot overflow (truncation
    /// never grows the magnitude — mirrors `round_places` minus the round-up carry).
    pub fn trunc_places(&self, n: i64) -> Decimal {
        if n >= 0 {
            return self.trunc_to_scale(n.min(MAX_SCALE as i64) as u32);
        }
        let k = n.unsigned_abs().min((self.precision() + 1) as u64) as u32;
        let pow = mag_pow10(self.scale + k);
        let (q, _r) = mag_divmod(&self.limbs, &pow);
        let scaled = mag_mul_pow10(&q, k);
        Decimal::from_parts(self.neg, 0, scaled)
    }

    /// `ceil(numeric)` — round toward +∞ to scale 0 (spec/design/functions.md §9).
    pub fn ceil(&self) -> Result<Decimal> {
        self.round_to_bound(false)
    }

    /// `floor(numeric)` — round toward −∞ to scale 0.
    pub fn floor(&self) -> Result<Decimal> {
        self.round_to_bound(true)
    }

    /// Shared kernel for `ceil`/`floor` to scale 0: drop the fraction toward zero, then grow the
    /// magnitude by one iff a fraction was dropped AND the requested direction points away from
    /// zero for this sign — `ceil` (`toward_neg = false`) grows a positive value, `floor`
    /// (`toward_neg = true`) grows a negative one. A carry can push a value at the integer-digit
    /// cap over it → 22003 (like `round`).
    fn round_to_bound(&self, toward_neg: bool) -> Result<Decimal> {
        if self.scale == 0 {
            return Ok(self.clone());
        }
        let pow = mag_pow10(self.scale);
        let (mut q, r) = mag_divmod(&self.limbs, &pow);
        let has_frac = r.iter().any(|&x| x != 0);
        if has_frac && self.neg == toward_neg {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg, 0, q).check_cap()
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

    /// The order-preserving KEY body for a decimal (method `decimal-order-preserving`,
    /// spec/design/encoding.md §2.5). Self-delimiting; sorts byte-for-byte under `memcmp`
    /// identically to numeric value, **independent of display scale** — `1.5` and `1.50`
    /// produce identical bytes (they are equal, so a UNIQUE decimal index treats them as one).
    /// A PK is NOT NULL, so the stored key is this bare body; the §2.2 nullable slot prepends
    /// the presence tag and §2.3 descending inverts the whole component (both at the caller).
    ///
    /// Normalize the value to `(sign, base-100 mantissa pairs, E)` with `value = 0.<pairs> ×
    /// 100^E`, then emit: a sign/class byte (`0x03` neg `<` `0x04` zero `<` `0x05` pos); the
    /// exponent `E` as a 4-byte order-preserving `int-be-signflip` `i32` (§2.1 — larger `E`
    /// sorts later for positives); the mantissa pairs most-significant first, each as `pair+1`
    /// ∈ `[0x01, 0x64]` (`0x00` reserved for the terminator); and a `0x00` terminator (a shorter
    /// mantissa sorts before one that extends it). For NEGATIVE values the exponent, mantissa,
    /// and terminator are **bitwise-complemented** so "more negative" sorts first.
    pub fn encode_key(&self) -> Vec<u8> {
        // Zero is the single class byte 0x04 (neg 0x03 < zero 0x04 < pos 0x05).
        if self.is_zero() {
            return vec![0x04];
        }
        // Significant digits (no leading zeros) and the base-10 decimal-point exponent:
        // value = 0.<digits> × 10^decpt, with decpt = precision − scale.
        let mut digits = mag_to_decimal_str(&self.limbs).into_bytes();
        let decpt = self.precision() as i32 - self.scale as i32;
        // Drop trailing zero digits (the least-significant ones): the mantissa keeps only its
        // significant part and decpt is unchanged, so `1.50` ("150") collapses onto `1.5` ("15").
        while digits.last() == Some(&b'0') {
            digits.pop();
        }
        // Base-100 exponent E (value = 0.<pairs> × 100^E) and pair alignment: prepend a '0' when
        // decpt is odd so the leading base-100 pair is "0 d1", then pad right to an even length.
        let e = (decpt + 1).div_euclid(2);
        let mut grouped: Vec<u8> = Vec::with_capacity(digits.len() + 2);
        if decpt.rem_euclid(2) == 1 {
            grouped.push(b'0');
        }
        grouped.extend_from_slice(&digits);
        if grouped.len() % 2 == 1 {
            grouped.push(b'0');
        }
        // Body: 4-byte order-preserving exponent ‖ mantissa pairs (pair+1) ‖ 0x00 terminator.
        let mut body: Vec<u8> = Vec::with_capacity(4 + grouped.len() / 2 + 1);
        body.extend_from_slice(&crate::encoding::encode_int(
            crate::types::ScalarType::Int32,
            e as i64,
        ));
        for pair in grouped.chunks(2) {
            let v = (pair[0] - b'0') * 10 + (pair[1] - b'0');
            body.push(v + 1);
        }
        body.push(0x00);
        // Assemble with the sign/class byte; negatives complement the body (E+mantissa+terminator).
        let mut out = Vec::with_capacity(1 + body.len());
        if self.neg {
            out.push(0x03);
            out.extend(body.iter().map(|b| !b));
        } else {
            out.push(0x05);
            out.extend_from_slice(&body);
        }
        out
    }
}

// ============================================================================
// Exact-numeric transcendentals — sqrt / ln / exp / log / power over decimal
// (spec/design/decimal.md §8). A hand-rolled, byte-exact port of PostgreSQL's
// arbitrary-precision numeric.c (sqrt_var / ln_var / exp_var / log_var /
// power_var / power_var_int). Unlike the FLOAT transcendentals (float.md §8,
// which ride the `R`-tag ULP exemption), these are IN-CONTRACT: every step is
// exact-decimal limb arithmetic, so the three cores agree byte-for-byte by
// construction. The result SCALE follows PG's rule (≥ MIN_SIG_DIGITS = 16
// significant digits, never below the input's display scale).
//
// The one place PG uses libm (`log`/`log10` inside the scale ESTIMATES) is the
// enemy of cross-core determinism (libm differs by an ULP across platforms), so
// every estimate here is computed WITHOUT a transcendental: the dweight estimate
// is the exact decimal weight of a low-precision exact `ln`, and the `decimal →
// f64` conversions PG keeps (`numericvar_to_double_no_overflow`) are the
// correctly-rounded string→f64 path (deterministic in every core). This makes
// jed's chosen rscale *match PG* in the overwhelming majority of cases and stay
// cross-core identical always; the rare boundary where the true floor differs
// from PG's f64 estimate is a documented divergence (decimal.md §8).
// ============================================================================

/// PG NUMERIC_MIN_SIG_DIGITS — the minimum significant digits the transcendentals target.
const MIN_SIG_DIGITS: i64 = 16;
/// PG DEC_DIGITS — the base-10⁴ group size PG measures `weight` in.
const DEC_DIGITS: i64 = 4;
/// PG NUMERIC_MAX_DISPLAY_SCALE = NUMERIC_MAX_PRECISION (1000) — the result display-scale clamp.
const MAX_DISPLAY_SCALE: i64 = MAX_PRECISION as i64;
/// PG NUMERIC_MAX_RESULT_SCALE = NUMERIC_MAX_PRECISION·2 (2000) — the exp/power input bound base.
const MAX_RESULT_SCALE: i64 = MAX_PRECISION as i64 * 2;
/// PG NUMERIC_WEIGHT_MAX = PG_INT16_MAX (32767) — the base-10⁴ weight ceiling for power overflow.
const NUMERIC_WEIGHT_MAX: i64 = 32767;

fn log_zero() -> EngineError {
    EngineError::new(
        SqlState::InvalidArgumentForLog,
        "cannot take logarithm of zero",
    )
}
fn log_negative() -> EngineError {
    EngineError::new(
        SqlState::InvalidArgumentForLog,
        "cannot take logarithm of a negative number",
    )
}

impl Decimal {
    // ---- internal primitives mirroring PG's NumericVar operations ----------

    /// PG NumericVar `weight`: the base-10⁴ weight of the most-significant digit group =
    /// `floor((precision − 1 − scale) / 4)` (the decimal exponent of the MSD, floored into
    /// base-10⁴ groups). Zero → 0 (PG `zero_var`). Value-derived, so identical across cores
    /// regardless of internal limb base (decimal.md §7 #11).
    fn nbase_weight(&self) -> i64 {
        if self.is_zero() {
            return 0;
        }
        let dexp = self.precision() as i64 - 1 - self.scale as i64;
        dexp.div_euclid(4)
    }

    /// The nearest f64 (correctly-rounded string→f64 — PG `numericvar_to_double_no_overflow`,
    /// which is `strtod(get_str_from_var())`). Deterministic across cores (IEEE strtod is
    /// correctly rounded in Rust/Go/TS); used ONLY by the scale estimates, never the value path.
    /// An out-of-range magnitude parses to ±Inf (PG ignores ERANGE the same way).
    fn to_f64_estimate(&self) -> f64 {
        self.render().parse::<f64>().unwrap_or(0.0)
    }

    /// Exact product (scale s1+s2), NO cap check — the running form of `*` for the kernels
    /// (PG `mul_var` does not cap; only `make_result` does).
    fn mul_exact(&self, other: &Decimal) -> Decimal {
        Decimal::from_parts(
            self.neg ^ other.neg,
            self.scale + other.scale,
            mag_mul(&self.limbs, &other.limbs),
        )
    }

    /// PG `mul_var(a, b, result, rscale)`: exact product rounded half-away to `rscale`
    /// fractional digits; the result carries scale exactly `rscale` (so its dscale tracks PG).
    /// `rscale >= 0` in every kernel use.
    fn mul_var(&self, other: &Decimal, rscale: i64) -> Decimal {
        self.mul_exact(other).round_var(rscale)
    }

    /// PG `round_var(var, rscale)`: round half-away to `rscale` fractional digits, uncapped.
    /// `rscale >= 0` → result scale exactly `rscale` (zero-padded if needed). `rscale < 0` →
    /// round to a multiple of `10^(−rscale)`, result scale 0 — jed represents PG's transient
    /// negative dscale as the equal value at scale 0; only `weight` (value-derived), never the
    /// negative dscale, feeds any later step (decimal.md §8).
    fn round_var(&self, rscale: i64) -> Decimal {
        if rscale >= 0 {
            return self.round_to_scale(rscale as u32);
        }
        let k = (-rscale) as u32;
        let drop = self.scale + k;
        let pow = mag_pow10(drop);
        let (mut q, r) = mag_divmod(&self.limbs, &pow);
        if mag_cmp(&mag_add(&r, &r), &pow) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg, 0, mag_mul_pow10(&q, k))
    }

    /// PG `div_var(a, b, result, rscale, round=true)`: `a / b` to exactly `rscale` fractional
    /// digits, rounded half-away (`rscale >= 0` in every kernel use). Traps 22012 on a zero
    /// divisor. Uncapped (the caller caps the final result).
    fn div_var(&self, other: &Decimal, rscale: i64) -> Result<Decimal> {
        if other.is_zero() {
            return Err(div_by_zero());
        }
        if self.is_zero() {
            return Ok(Decimal::zero(rscale.max(0) as u32));
        }
        // q = round_half_away(|C1|·10^E / |C2|), E = rscale + s2 − s1 (sign handled).
        let e = rscale + other.scale as i64 - self.scale as i64;
        let (numer, denom) = if e >= 0 {
            (mag_mul_pow10(&self.limbs, e as u32), other.limbs.clone())
        } else {
            (self.limbs.clone(), mag_mul_pow10(&other.limbs, (-e) as u32))
        };
        let (mut q, r) = mag_divmod(&numer, &denom);
        if mag_cmp(&mag_add(&r, &r), &denom) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Ok(Decimal::from_parts(
            self.neg ^ other.neg,
            rscale.max(0) as u32,
            q,
        ))
    }

    /// PG `div_var_int(a, ival, 0, result, rscale, round=true)` — `a / ival` to `rscale` digits.
    fn div_var_int(&self, ival: i64, rscale: i64) -> Result<Decimal> {
        self.div_var(&Decimal::from_i64(ival), rscale)
    }

    /// Whether this value is an exact integer that fits `i32` (PG's `power_var_int` gate); the
    /// integral check is `trunc(self) == self`.
    fn to_i32_if_integer(&self) -> Option<i32> {
        if self.trunc_to_scale(0).cmp_value(self) != std::cmp::Ordering::Equal {
            return None;
        }
        i32::try_from(self.to_i64_round()?).ok()
    }

    // ---- the algorithm kernels (PG numeric.c) ------------------------------

    /// PG `sqrt_var(arg, result, rscale)`: √self rounded half-away to `rscale` fractional digits
    /// (rscale may be negative — PG explicitly allows rounding before the point during ln's input
    /// reduction). Computed as `floor(√self · 10^kc)` via an exact big-integer square root, then
    /// rounded to `rscale`. Traps 2201F on a negative operand.
    fn sqrt_var(&self, rscale: i64) -> Result<Decimal> {
        if self.is_zero() {
            return Ok(Decimal::zero(rscale.max(0) as u32));
        }
        if self.neg {
            return Err(EngineError::new(
                SqlState::InvalidArgumentForPowerFunction,
                "cannot take square root of a negative number",
            ));
        }
        // Compute floor(√v · 10^kc) with kc ≥ rscale+1 guard digits, ensuring the scaled value
        // `v · 10^(2·kc)` is an exact integer (E = 2·kc − scale ≥ 0).
        let s = self.scale as i64;
        let mut kc = rscale.max(0) + 1;
        if 2 * kc < s {
            kc = (s + 1) / 2 + 1; // bumps kc so E ≥ 0; extra guard never changes the rounded result
        }
        let e = 2 * kc - s; // ≥ 0
        let n = mag_mul_pow10(&self.limbs, e as u32);
        let g = mag_isqrt(&n);
        let at_guard = Decimal::from_parts(false, kc as u32, g);
        Ok(at_guard.round_var(rscale))
    }

    /// `sqrt(numeric)` (PG `numeric_sqrt`): choose rscale for ≥ MIN_SIG_DIGITS significant digits
    /// (never below the input dscale), then call `sqrt_var`.
    pub fn dec_sqrt(&self) -> Result<Decimal> {
        // sweight = floor(weight·DEC_DIGITS / 2) + 1 (DEC_DIGITS = 4 even ⇒ the division is exact).
        let sweight = self.nbase_weight() * DEC_DIGITS / 2 + 1;
        let mut rscale = MIN_SIG_DIGITS - sweight;
        rscale = rscale.max(self.scale as i64).max(0).min(MAX_DISPLAY_SCALE);
        self.sqrt_var(rscale)?.check_cap()
    }

    /// PG `exp_var(arg, result, rscale)`: e^self to `rscale` fractional digits via a range-reduced
    /// Taylor series. Traps 22003 on overflow (a true result outside the numeric format).
    fn exp_var(&self, rscale: i64) -> Result<Decimal> {
        let mut x = self.clone();
        let mut val = self.to_f64_estimate();
        // Overflow / underflow guard (PG: fabs(val) >= NUMERIC_MAX_RESULT_SCALE·3 = 6000).
        if val.abs() >= (MAX_RESULT_SCALE * 3) as f64 {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Decimal::zero(rscale.max(0) as u32));
        }
        let dweight = (val * 0.434294481903252) as i64; // decimal weight ≈ x·log10(e)
        // Reduce x into roughly [-0.01, 0.01] by halving ndiv2 times, for fast Taylor convergence.
        let ndiv2: i64;
        if val.abs() > 0.01 {
            let mut n = 1i64;
            val /= 2.0;
            while val.abs() > 0.01 {
                n += 1;
                val /= 2.0;
            }
            ndiv2 = n;
            let local_rscale = x.scale as i64 + ndiv2;
            x = x.div_var_int(1i64 << ndiv2, local_rscale)?;
        } else {
            ndiv2 = 0;
        }
        let mut sig_digits = 1 + dweight + rscale + (ndiv2 as f64 * 0.301029995663981) as i64;
        sig_digits = sig_digits.max(0) + 8;
        let local_rscale = sig_digits - 1;
        // Taylor: exp(x) = 1 + x + x²/2! + x³/3! + …
        let mut result = Decimal::from_i64(1).add_uncapped(&x);
        let mut elem = x.mul_var(&x, local_rscale);
        let mut ni = 2i64;
        elem = elem.div_var_int(ni, local_rscale)?;
        while !elem.is_zero() {
            result = result.add_uncapped(&elem);
            elem = elem.mul_var(&x, local_rscale);
            ni += 1;
            elem = elem.div_var_int(ni, local_rscale)?;
        }
        // Compensate the range reduction: square the result ndiv2 times (rscale shrinks as the
        // weight doubles).
        let mut k = ndiv2;
        while k > 0 {
            k -= 1;
            let lr = (sig_digits - result.nbase_weight() * 2 * DEC_DIGITS).max(0);
            result = result.mul_var(&result, lr);
        }
        Ok(result.round_var(rscale))
    }

    /// `exp(numeric)` (PG `numeric_exp`): choose rscale, then call `exp_var`.
    pub fn dec_exp(&self) -> Result<Decimal> {
        let mut val = self.to_f64_estimate() * 0.434294481903252;
        val = val.clamp(-(MAX_RESULT_SCALE as f64), MAX_RESULT_SCALE as f64);
        let mut rscale = MIN_SIG_DIGITS - (val as i64);
        rscale = rscale.max(self.scale as i64).max(0).min(MAX_DISPLAY_SCALE);
        self.exp_var(rscale)?.check_cap()
    }

    /// PG `ln_var(arg, result, rscale)`: the natural log of self (> 0) to `rscale` fractional
    /// digits. Reduces self into (0.9, 1.1) by repeated `sqrt`, then sums the `atanh` series
    /// `2·(z + z³/3 + z⁵/5 + …)` with `z = (x−1)/(x+1)`. The caller guarantees self > 0.
    fn ln_var(&self, rscale: i64) -> Decimal {
        let nine_tenths = Decimal::from_digits_scale(false, "9", 1); // 0.9
        let eleven_tenths = Decimal::from_digits_scale(false, "11", 1); // 1.1
        let two = Decimal::from_i64(2);
        let one = Decimal::from_i64(1);
        let mut x = self.clone();
        let mut fact = two.clone();
        let mut nsqrt = 0i64;
        while x.cmp_value(&nine_tenths) != std::cmp::Ordering::Greater {
            let local_rscale = rscale - x.nbase_weight() * DEC_DIGITS / 2 + 8;
            x = x
                .sqrt_var(local_rscale)
                .expect("ln reduces a positive value");
            fact = fact.mul_var(&two, 0);
            nsqrt += 1;
        }
        while x.cmp_value(&eleven_tenths) != std::cmp::Ordering::Less {
            let local_rscale = rscale - x.nbase_weight() * DEC_DIGITS / 2 + 8;
            x = x
                .sqrt_var(local_rscale)
                .expect("ln reduces a positive value");
            fact = fact.mul_var(&two, 0);
            nsqrt += 1;
        }
        let local_rscale = rscale + ((nsqrt + 1) as f64 * 0.301029995663981) as i64 + 8;
        // z = (x−1)/(x+1)
        let numer = x.sub_uncapped(&one);
        let denom = x.add_uncapped(&one);
        let mut result = numer
            .div_var(&denom, local_rscale)
            .expect("x+1 is positive");
        let mut xx = result.clone(); // running z^(2k+1)
        let zsq = result.mul_var(&result, local_rscale); // z²
        let mut ni = 1i64;
        loop {
            ni += 2;
            xx = xx.mul_var(&zsq, local_rscale);
            let elem = xx.div_var_int(ni, local_rscale).expect("ni != 0");
            if elem.is_zero() {
                break;
            }
            result = result.add_uncapped(&elem);
            if elem.nbase_weight() < result.nbase_weight() - local_rscale * 2 / DEC_DIGITS {
                break;
            }
        }
        result.mul_var(&fact, rscale)
    }

    /// Deterministic PG `estimate_ln_dweight(var)` — an estimate of `trunc(log10(|ln(var)|))`
    /// (PG truncates toward zero via `(int)`), used to pick the result rscale. Computed WITHOUT
    /// libm: branch 1 (0.9 ≤ var ≤ 1.1) is the exact decimal weight of `var − 1`; branch 2 is the
    /// decimal weight of a low-precision exact `ln`, adjusted from floor to trunc-toward-zero
    /// (`dweight + 1` when negative). `var > 0` (caller-guaranteed); returns 0 otherwise.
    fn estimate_ln_dweight(&self) -> i64 {
        if self.is_zero() || self.neg {
            return 0;
        }
        let nine_tenths = Decimal::from_digits_scale(false, "9", 1);
        let eleven_tenths = Decimal::from_digits_scale(false, "11", 1);
        if self.cmp_value(&nine_tenths) != std::cmp::Ordering::Less
            && self.cmp_value(&eleven_tenths) != std::cmp::Ordering::Greater
        {
            let x = self.sub_uncapped(&Decimal::from_i64(1));
            if x.is_zero() {
                return 0;
            }
            // floor(log10(|x|)) — PG's branch 1 (x.weight·DEC_DIGITS + floor(log10(digits[0]))).
            return x.precision() as i64 - 1 - x.scale as i64;
        }
        // |ln(self)| ≥ ln(1.1) ≈ 0.095 here, so a 20-digit guard captures its MSD position.
        let t = self.ln_var(20);
        if t.is_zero() {
            return 0;
        }
        let dw = t.precision() as i64 - 1 - t.scale as i64; // floor(log10(|ln(self)|))
        if dw < 0 { dw + 1 } else { dw }
    }

    /// `ln(numeric)` (PG `numeric_ln`): choose rscale from the dweight estimate, then `ln_var`.
    pub fn dec_ln(&self) -> Result<Decimal> {
        if self.is_zero() {
            return Err(log_zero());
        }
        if self.neg {
            return Err(log_negative());
        }
        let ln_dweight = self.estimate_ln_dweight();
        let mut rscale = MIN_SIG_DIGITS - ln_dweight;
        rscale = rscale.max(self.scale as i64).max(0).min(MAX_DISPLAY_SCALE);
        self.ln_var(rscale).check_cap()
    }

    /// `log(base, num)` (PG `numeric_log` / `log_var`): logarithm of `num` in base `base`,
    /// `= ln(num) / ln(base)`. Both operands must be > 0 (else 2201E). Chooses its own rscale.
    pub fn dec_log(base: &Decimal, num: &Decimal) -> Result<Decimal> {
        // ln_var(base) is formed first in PG, so a bad base reports first.
        for v in [base, num] {
            if v.is_zero() {
                return Err(log_zero());
            }
            if v.neg {
                return Err(log_negative());
            }
        }
        let ln_base_dweight = base.estimate_ln_dweight();
        let ln_num_dweight = num.estimate_ln_dweight();
        let result_dweight = ln_num_dweight - ln_base_dweight;
        let mut rscale = MIN_SIG_DIGITS - result_dweight;
        rscale = rscale
            .max(base.scale as i64)
            .max(num.scale as i64)
            .max(0)
            .min(MAX_DISPLAY_SCALE);
        let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(0);
        let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(0);
        let ln_base = base.ln_var(ln_base_rscale);
        let ln_num = num.ln_var(ln_num_rscale);
        ln_num.div_var(&ln_base, rscale)?.check_cap()
    }

    /// `log(numeric)` / `log10(numeric)` — base-10 logarithm (PG defines one-arg `log` as
    /// `log(10, x)`).
    pub fn dec_log10(&self) -> Result<Decimal> {
        Decimal::dec_log(&Decimal::from_i64(10), self)
    }

    /// PG `power_var_int(base, exp, exp_dscale)`: base^exp for an integer exp, by binary
    /// exponentiation with a per-multiplication rscale that keeps a fixed significant-digit count.
    fn power_var_int(base: &Decimal, exp: i32, exp_dscale: u32) -> Result<Decimal> {
        // Estimate the decimal weight of the result, f ≈ exp · log10(|base|) (PG uses libm log10
        // on the leading-digit MAGNITUDE; jed computes log10(|base|) = ln(|base|)/ln(10) exactly —
        // deterministic). The MAGNITUDE matters: the binary-exponentiation muls below carry the
        // sign, but ln() is only defined on the positive magnitude.
        let f: f64 = if base.is_zero() {
            0.0
        } else {
            base.abs()
                .log10_estimate()
                .mul_exact(&Decimal::from_i64(exp as i64))
                .to_f64_estimate()
        };
        // Crude overflow / underflow exits with PG's fuzz.
        if f > (NUMERIC_WEIGHT_MAX + 1) as f64 * DEC_DIGITS as f64 {
            return Err(overflow());
        }
        if f + 1.0 < -(MAX_DISPLAY_SCALE as f64) {
            return Ok(Decimal::zero(MAX_DISPLAY_SCALE as u32));
        }
        let fi = f as i64; // (int) f — truncated toward zero
        let mut rscale = MIN_SIG_DIGITS - fi;
        rscale = rscale
            .max(base.scale as i64)
            .max(exp_dscale as i64)
            .max(0)
            .min(MAX_DISPLAY_SCALE);
        match exp {
            0 => return Ok(Decimal::from_i64(1).round_var(rscale)),
            1 => return Ok(base.round_var(rscale)),
            -1 => return Decimal::from_i64(1).div_var(base, rscale)?.check_cap(),
            2 => return base.mul_var(base, rscale).check_cap(),
            _ => {}
        }
        if base.is_zero() {
            if exp < 0 {
                return Err(div_by_zero());
            }
            return Ok(Decimal::zero(rscale as u32));
        }
        let mut sig_digits = 1 + rscale + fi;
        // Guard for the multiplication error, ≈ log(|exp|) extra digits (PG: (int)log(fabs(exp))).
        sig_digits += int_ln_floor(exp.unsigned_abs() as u64) + 8;
        let neg = exp < 0;
        let mut mask = exp.unsigned_abs();
        let mut base_prod = base.clone();
        let mut result = if mask & 1 == 1 {
            base.clone()
        } else {
            Decimal::from_i64(1)
        };
        let mut overflowed = false;
        while {
            mask >>= 1;
            mask > 0
        } {
            let lr = (sig_digits - 2 * base_prod.nbase_weight() * DEC_DIGITS)
                .min(2 * base_prod.scale as i64)
                .max(0);
            base_prod = base_prod.mul_var(&base_prod, lr);
            if mask & 1 == 1 {
                let lr2 = (sig_digits
                    - (base_prod.nbase_weight() + result.nbase_weight()) * DEC_DIGITS)
                    .min(base_prod.scale as i64 + result.scale as i64)
                    .max(0);
                result = base_prod.mul_var(&result, lr2);
            }
            if base_prod.nbase_weight() > NUMERIC_WEIGHT_MAX
                || result.nbase_weight() > NUMERIC_WEIGHT_MAX
            {
                if !neg {
                    return Err(overflow());
                }
                result = Decimal::zero(0);
                overflowed = true;
                break;
            }
        }
        if neg && !overflowed {
            Decimal::from_i64(1).div_var(&result, rscale)?.check_cap()
        } else {
            result.round_var(rscale).check_cap()
        }
    }

    /// PG `power_var(base, exp, result)`: base^exp for a general (possibly non-integer) exponent.
    /// Integer exponents route to `power_var_int`; otherwise computes `exp(exp · ln(base))`.
    fn power_var(base: &Decimal, exp: &Decimal) -> Result<Decimal> {
        if let Some(iexp) = exp.to_i32_if_integer() {
            return Decimal::power_var_int(base, iexp, exp.scale);
        }
        // 0 ^ non-integer = 0 (0 ^ negative was rejected by the caller).
        if base.is_zero() {
            return Ok(Decimal::zero(MIN_SIG_DIGITS as u32));
        }
        // A negative base demands an integer exponent (which routed above), so a non-integer
        // exponent here is the complex-result error.
        if base.neg {
            return Err(EngineError::new(
                SqlState::InvalidArgumentForPowerFunction,
                "a negative number raised to a non-integer power yields a complex result",
            ));
        }
        let ln_dweight = base.estimate_ln_dweight();
        // Low-precision exp·ln(base) to estimate the result weight (and a crude overflow exit).
        let mut local_rscale = (8 - ln_dweight).max(0);
        let mut ln_base = base.ln_var(local_rscale);
        let mut ln_num = ln_base.mul_var(exp, local_rscale);
        let mut val = ln_num.to_f64_estimate();
        if val.abs() > MAX_RESULT_SCALE as f64 * 3.01 {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Decimal::zero(MAX_DISPLAY_SCALE as u32));
        }
        val *= 0.434294481903252;
        let vi = val as i64;
        let mut rscale = MIN_SIG_DIGITS - vi;
        rscale = rscale
            .max(base.scale as i64)
            .max(exp.scale as i64)
            .max(0)
            .min(MAX_DISPLAY_SCALE);
        let sig_digits = (rscale + vi).max(0);
        local_rscale = (sig_digits - ln_dweight + 8).max(0);
        ln_base = base.ln_var(local_rscale);
        ln_num = ln_base.mul_var(exp, local_rscale);
        ln_num.exp_var(rscale)?.check_cap()
    }

    /// `power(base, exp)` over numeric (PG `numeric_power`, finite path): the domain checks plus
    /// `power_var`. `0 ^ negative` traps 2201F.
    pub fn dec_power(base: &Decimal, exp: &Decimal) -> Result<Decimal> {
        let sign1 = if base.is_zero() {
            0
        } else if base.neg {
            -1
        } else {
            1
        };
        let sign2 = if exp.is_zero() {
            0
        } else if exp.neg {
            -1
        } else {
            1
        };
        if sign1 == 0 && sign2 < 0 {
            return Err(EngineError::new(
                SqlState::InvalidArgumentForPowerFunction,
                "zero raised to a negative power is undefined",
            ));
        }
        Decimal::power_var(base, exp)
    }

    /// `log10(self) = ln(self)/ln(10)` to a ~30-digit guard — the deterministic, libm-free
    /// replacement for PG `power_var_int`'s `log10(double)` result-weight estimate. `self > 0`.
    fn log10_estimate(&self) -> Decimal {
        let guard = 30;
        let ln_self = self.ln_var(guard);
        let ln_ten = Decimal::from_i64(10).ln_var(guard);
        ln_self.div_var(&ln_ten, guard).expect("ln(10) is positive")
    }
}

/// `floor(ln(n))` for `n >= 1`, computed deterministically via the exact `ln` (no libm) — PG's
/// `(int)log(fabs(exp))` guard term in `power_var_int`. `n == 0` → 0.
fn int_ln_floor(n: u64) -> i64 {
    if n <= 1 {
        return 0; // ln(1) = 0; ln(0) is never reached (exp != 0 in the guard)
    }
    // ln(n) ≥ 0; trunc-to-scale-0 of a 12-digit-guard ln is its floor for any non-boundary n.
    Decimal::from_i64(n as i64)
        .ln_var(12)
        .trunc_to_scale(0)
        .to_i64_round()
        .unwrap_or(0)
}

/// `floor(√n)` for a magnitude `n` (base-10⁹ LSB-first), via Newton's method on big integers
/// (`x ← (x + n/x)/2` from `x₀ = 10^⌈digits/2⌉ ≥ √n`, monotone-decreasing to the floor). The
/// exact integer square root underlying `sqrt_var` — deterministic, no float seed.
fn mag_isqrt(n: &[u32]) -> Vec<u32> {
    if n.is_empty() {
        return Vec::new();
    }
    let half = (mag_digit_count(n) + 1) / 2; // ⌈digits/2⌉
    let mut x = mag_pow10(half); // ≥ √n
    loop {
        let (n_div_x, _) = mag_divmod(n, &x);
        let sum = mag_add(&x, &n_div_x);
        let y = mag_divmod(&sum, &[2]).0; // (x + n/x) / 2
        if mag_cmp(&y, &x) != std::cmp::Ordering::Less {
            return x;
        }
        x = y;
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
    fn float64_to_decimal_is_exact() {
        // Exactly-representable binaries expand to their exact (short) decimal.
        assert_eq!(Decimal::from_float64(0.5).render(), "0.5");
        assert_eq!(Decimal::from_float64(2.5).render(), "2.5");
        assert_eq!(Decimal::from_float64(0.0).render(), "0");
        assert_eq!(Decimal::from_float64(-0.0).render(), "0");
        assert_eq!(Decimal::from_float64(1.0).render(), "1");
        // 1e20 is an exact integer in binary64 (its exponent is positive) — scale 0.
        assert_eq!(
            Decimal::from_float64(1e20).render(),
            "100000000000000000000"
        );

        // 0.1 is NOT exactly representable: the exact value of the nearest binary64 is the long
        // expansion below — the unique cross-core answer (matches Go's exactDecimalFromFloat64).
        let tenth = Decimal::from_float64(0.1);
        assert_eq!(
            tenth.render(),
            "0.1000000000000000055511151231257827021181583404541015625"
        );
        // The full exact value has scale 55; the typmod(60,55) coercion is loss-free here.
        assert_eq!(tenth.scale(), 55);
        assert_eq!(
            tenth.coerce_to_typmod(60, 55).unwrap().render(),
            "0.1000000000000000055511151231257827021181583404541015625"
        );

        // A negative non-representable value expands exactly too (the binary64 nearest -0.2).
        assert_eq!(
            Decimal::from_float64(-0.2).render(),
            "-0.200000000000000011102230246251565404236316680908203125"
        );

        // Round-trip: the exact decimal must re-parse (via render) to the SAME binary64.
        for f in [0.1f64, 1.0 / 3.0, 123456.789, -2.25, 1e-10, 0.2] {
            let back: f64 = Decimal::from_float64(f).render().parse().unwrap();
            assert_eq!(back, f, "exact decimal of {f} must reparse to itself");
        }
    }

    #[test]
    fn int_to_decimal() {
        assert_eq!(Decimal::from_i64(5).render(), "5");
        assert_eq!(Decimal::from_i64(-7).render(), "-7");
        assert_eq!(Decimal::from_i64(i64::MIN).render(), "-9223372036854775808");
        assert_eq!(Decimal::from_i64(0).render(), "0");
    }

    #[test]
    fn key_encoding_is_order_preserving() {
        // A spread crossing the sign boundary, zero, sub-1 magnitudes, scale-equal duplicates,
        // odd/even decpt, and many-digit values. Sorting by encode_key must equal cmp_value order.
        let mut vals: Vec<Decimal> = [
            "-12345.6789",
            "-100",
            "-10",
            "-1.5",
            "-1",
            "-0.5",
            "-0.05",
            "-0.001",
            "0",
            "0.001",
            "0.05",
            "0.5",
            "1",
            "1.5",
            "1.50",
            "5",
            "10",
            "12",
            "50",
            "100",
            "101",
            "123",
            "1000",
            "12345.6789",
            "99999999999999999999",
        ]
        .iter()
        .map(|s| dec(s))
        .collect();
        // Sort a copy by the encoded key; it must match value order.
        let mut by_key = vals.clone();
        by_key.sort_by(|a, b| a.encode_key().cmp(&b.encode_key()));
        vals.sort_by(|a, b| a.cmp_value(b));
        let rk: Vec<String> = by_key.iter().map(|d| d.render()).collect();
        let rv: Vec<String> = vals.iter().map(|d| d.render()).collect();
        assert_eq!(rk, rv, "encode_key order must equal cmp_value order");

        // Scale-independence: 1.5 and 1.50 are equal, so identical key bytes.
        assert_eq!(dec("1.5").encode_key(), dec("1.50").encode_key());
        assert_eq!(dec("100").encode_key(), dec("100.00").encode_key());
        assert_eq!(dec("0").encode_key(), dec("0.000").encode_key());
        // Zero is the single class byte; negatives sort below it, positives above.
        assert_eq!(dec("0").encode_key(), vec![0x04]);
        assert!(dec("-1").encode_key() < dec("0").encode_key());
        assert!(dec("0").encode_key() < dec("1").encode_key());

        // Exact byte vectors (the cross-core contract — Go/TS/Ruby must reproduce these):
        // 1.5 = 0.[01][50] × 100^1: class 0x05, E=1 (i32 int-be-signflip), pairs 01+1/50+1, term.
        assert_eq!(
            dec("1.5").encode_key(),
            vec![0x05, 0x80, 0x00, 0x00, 0x01, 0x02, 0x33, 0x00]
        );
        assert_eq!(
            dec("1.50").encode_key(),
            vec![0x05, 0x80, 0x00, 0x00, 0x01, 0x02, 0x33, 0x00]
        );
        // 100 = 0.[01] × 100^2: class 0x05, E=2, pair 01+1, term.
        assert_eq!(
            dec("100").encode_key(),
            vec![0x05, 0x80, 0x00, 0x00, 0x02, 0x02, 0x00]
        );
        // -1.5: the 1.5 body bitwise-complemented under class 0x03.
        assert_eq!(
            dec("-1.5").encode_key(),
            vec![0x03, 0x7F, 0xFF, 0xFF, 0xFE, 0xFD, 0xCC, 0xFF]
        );
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

    // The SUM/AVG accumulator's `add_uncapped` path (spec/design/decimal.md §2, determinism.md
    // §7): the running sum may cross the §2 format cap mid-fold without trapping; only the FINAL
    // result is cap-checked. This is the order-independent-trap fix — too large to reach through
    // SQL literals (a 131072-digit value is ~74 KB), so it is pinned here. `a` is exactly at the
    // cap (131072 nines); `a + a` is one digit over it.
    #[test]
    fn sum_accumulator_checks_only_the_final_cap() {
        let a = Decimal::from_digits_scale(false, &"9".repeat(MAX_INT_DIGITS as usize), 0);
        assert!(a.clone().check_cap().is_ok()); // exactly at the cap

        // Capped `add` (standalone arithmetic) still traps at the cap — unchanged contract.
        assert_eq!(a.add(&a).unwrap_err().code(), "22003");

        // Uncapped fold may exceed the cap intermediately and NOT trap...
        let over = a.add_uncapped(&a); // 2·a, one digit over the cap
        // ...then come back in range, so the FINAL check passes and the value is exact.
        let back = over.add_uncapped(&a.neg());
        assert_eq!(
            back.clone().check_cap().unwrap().cmp_value(&a),
            std::cmp::Ordering::Equal
        );

        // A final result genuinely over the cap still traps 22003 (PG's make_result).
        assert_eq!(over.check_cap().unwrap_err().code(), "22003");
    }
}
