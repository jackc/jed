//! Exact base-10 `decimal` / `numeric` (spec/design/decimal.md).
//!
//! A value is `(neg, coefficient, scale)` = `(-1)^neg · coefficient · 10^(-scale)`. The
//! coefficient is a hand-rolled big integer in base 10^9 limbs, **least-significant first**
//! (`limbs[0]` is the low 9 digits; the order is internal — only the rendered value and the
//! on-disk bytes are cross-core contracts, CLAUDE.md §2). No bignum crate (Rust has zero
//! runtime deps); the limb algorithm is the spec, identical across cores and pinned by shared
//! fixtures. Always **finite** (no NaN/±Infinity) and **normalized** (no high zero limbs, no
//! negative zero). Rounding is **half away from zero** (spec/design/decimal.md §3).

use crate::cost::Meter;
use crate::costs::COSTS;
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
        digits.extend(std::iter::repeat_n('0', zeros));
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
    mag: Mag,
}

/// The coefficient magnitude (spec/design/decimal.md §1). **Canonical**: a magnitude below 2^128
/// is always `Small`, so a `Big` magnitude is at least 2^128 and two equal magnitudes have the
/// same variant. Every constructor goes through [`Decimal::from_small`] or
/// [`Decimal::from_parts`], which keep that.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mag {
    /// `lo + hi·2^64`, held inline: no allocation to build, copy, or drop. Two `u64`s rather than
    /// one `u128` so the type aligns to 8 bytes and a `Decimal` stays 32.
    Small(u64, u64),
    /// Base-10^9 limbs, least-significant first, no high zero limbs.
    Big(Vec<u32>),
}

fn overflow() -> EngineError {
    EngineError::new(
        SqlState::NumericValueOutOfRange,
        "value out of range for type decimal",
    )
    .with_data_type("decimal")
}

fn div_by_zero() -> EngineError {
    EngineError::new(SqlState::DivisionByZero, "division by zero")
}

impl Decimal {
    /// Construct from raw parts, normalizing (trim high zero limbs; force `neg = false` for
    /// zero). The single choke-point every constructor ends with.
    fn from_parts(neg: bool, scale: u32, mut limbs: Vec<u32>) -> Decimal {
        mag_trim(&mut limbs);
        match limbs_to_small(&limbs) {
            Some(m) => Decimal::from_small(neg, scale, m),
            None => Decimal {
                neg,
                scale,
                mag: Mag::Big(limbs),
            },
        }
    }

    /// Construct from a magnitude below 2^128 (`neg` is forced `false` for zero).
    #[inline]
    fn from_small(neg: bool, scale: u32, m: u128) -> Decimal {
        Decimal {
            neg: neg && m != 0,
            scale,
            mag: Mag::Small(m as u64, (m >> 64) as u64),
        }
    }

    /// The magnitude when it is held inline (below 2^128), else `None`.
    #[inline]
    fn small(&self) -> Option<u128> {
        match self.mag {
            Mag::Small(lo, hi) => Some((hi as u128) << 64 | lo as u128),
            Mag::Big(_) => None,
        }
    }

    /// The magnitude as base-10^9 limbs, whichever way it is held — what the limb algorithms
    /// (the path for anything the inline arithmetic declines) read.
    fn limbs(&self) -> LimbView<'_> {
        match &self.mag {
            Mag::Small(lo, hi) => {
                let (buf, len) = small_to_limbs((*hi as u128) << 64 | *lo as u128);
                LimbView::Inline(buf, len)
            }
            Mag::Big(limbs) => LimbView::Borrowed(limbs),
        }
    }

    /// The integer whose radix-`base` (2, 8, or 16) digits are `digits` (ASCII, no separators), at
    /// scale 0 — PostgreSQL `numeric_in`'s non-decimal integer input (`numeric '0xff'`, decimal.md
    /// §2). A value certainly above the `MAX_INT_DIGITS` cap traps `22003` before any conversion
    /// work (a resource guard: `base^(n-1) >= 2^435412 > 10^131072`); the caller's cap check
    /// decides the rest.
    pub fn from_radix_digits(neg: bool, digits: &str, base: u32) -> Result<Decimal> {
        let sig = digits.trim_start_matches('0').as_bytes();
        let bits = u64::from(base.trailing_zeros());
        if !sig.is_empty() && (sig.len() as u64 - 1) * bits >= 435_412 {
            return Err(overflow());
        }
        // Fold the digits in groups worth at most 2^28 (< BASE), so each step is one
        // multiply-by-small and add over the LSB-first base-10^9 limbs.
        let group = (28 / bits) as usize;
        let mut limbs: Vec<u32> = Vec::new();
        for chunk in sig.chunks(group) {
            let mut carry: u64 = 0;
            let mut mul: u64 = 1;
            for &c in chunk {
                let d = char::from(c).to_digit(base).expect("a radix digit");
                carry = carry * u64::from(base) + u64::from(d);
                mul *= u64::from(base);
            }
            for limb in limbs.iter_mut() {
                let cur = u64::from(*limb) * mul + carry;
                *limb = (cur % BASE) as u32;
                carry = cur / BASE;
            }
            while carry != 0 {
                limbs.push((carry % BASE) as u32);
                carry /= BASE;
            }
        }
        Ok(Decimal::from_parts(neg, 0, limbs))
    }

    /// Zero at the given display scale.
    pub fn zero(scale: u32) -> Decimal {
        Decimal::from_small(false, scale, 0)
    }

    /// Exact decimal from an integer (the lossless `int → decimal` cast, scale 0).
    pub fn from_i64(v: i64) -> Decimal {
        // i64::MIN handled (|MIN| = 2^63 fits u64)
        Decimal::from_small(v < 0, 0, v.unsigned_abs() as u128)
    }

    /// Build from a sign, an unscaled coefficient as a decimal-digit string (leading zeros
    /// allowed), and a scale. The literal/parse entry point — does NOT enforce the
    /// precision/scale caps (the caller checks them at resolve, trapping 22003).
    pub fn from_digits_scale(neg: bool, digits: &str, scale: u32) -> Decimal {
        // 38 digits are below 10^38 < 2^128.
        if digits.len() <= 38 && digits.bytes().all(|b| b.is_ascii_digit()) {
            let m = digits
                .bytes()
                .fold(0u128, |m, b| m * 10 + (b - b'0') as u128);
            return Decimal::from_small(neg, scale, m);
        }
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

    /// PostgreSQL's `float8_numeric`: the binary64 value printed with `%.15g` (`DBL_DIG`
    /// significant digits) and read back as a numeric. Used by the jsonpath `.double()` method
    /// (spec/design/jsonpath.md §3.1) and a float `PASSING` value; the caller has already rejected
    /// NaN and ±Infinity.
    pub fn from_float64_dbl_dig(f: f64) -> Decimal {
        Decimal::from_float_sig_digits(f, 15)
    }

    /// PostgreSQL's `float4_numeric`: `%.6g` (`FLT_DIG`) of the binary32 value.
    pub fn from_float32_flt_dig(f: f32) -> Decimal {
        Decimal::from_float_sig_digits(f64::from(f), 6)
    }

    /// A finite float printed with `%.<sig>g` and read back as a numeric. The digits come from the
    /// EXACT decimal expansion ([`Decimal::from_float64`]; an `f32` widens exactly), rounded to
    /// `sig` significant digits half-to-even (glibc's printf rounds the exact value to nearest,
    /// ties to even), with trailing fractional zeros trimmed — `%g` drops them, and an exponent
    /// form reads back at display scale `max(0, digits − exponent)`. Pure integer arithmetic, so
    /// every core agrees bit-for-bit.
    fn from_float_sig_digits(f: f64, sig: u32) -> Decimal {
        let exact = Decimal::from_float64(f);
        let digits = exact.precision();
        if digits <= sig {
            return exact;
        }
        let drop = digits - sig;
        let pow = mag_pow10(drop);
        let (mut q, r) = mag_divmod(&exact.limbs(), &pow);
        let round_up = match mag_cmp(&mag_add(&r, &r), &pow) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => q.first().is_some_and(|l| l % 2 == 1),
            std::cmp::Ordering::Less => false,
        };
        if round_up {
            q = mag_add(&q, &[1]);
        }
        let rounded = if exact.scale >= drop {
            Decimal::from_parts(exact.neg, exact.scale - drop, q)
        } else {
            Decimal::from_parts(exact.neg, 0, mag_mul_pow10(&q, drop - exact.scale))
        };
        let (cneg, cdigits, cscale) = rounded.canonical();
        Decimal::from_parts(cneg, cscale, cdigits)
    }

    /// The exact decimal of the nonnegative `o·2^k`, multiplying by 2^30 or 5^13 a step
    /// ([`mag_mul_small`] takes a factor up to about 1.8e10).
    fn exact_dyadic(o: u64, k: i32) -> Decimal {
        let mut mag = mag_from_u64(o);
        let mut left = k.unsigned_abs();
        if k >= 0 {
            while left >= 30 {
                mag = mag_mul_small(&mag, 1 << 30);
                left -= 30;
            }
            return Decimal::from_parts(false, 0, mag_mul_small(&mag, 1 << left));
        }
        while left >= 13 {
            mag = mag_mul_small(&mag, 1_220_703_125); // 5^13
            left -= 13;
        }
        let p = 5u64.pow(left);
        Decimal::from_parts(false, k.unsigned_abs(), mag_mul_small(&mag, p))
    }

    /// The nonnegative decimal `digits·10^q`.
    fn times_pow10(digits: &str, q: i32) -> Decimal {
        if q >= 0 {
            Decimal::from_digits_scale(false, &(digits.to_string() + &"0".repeat(q as usize)), 0)
        } else {
            Decimal::from_digits_scale(false, digits, q.unsigned_abs())
        }
    }

    pub fn is_zero(&self) -> bool {
        // A limb magnitude is at least 2^128.
        matches!(self.mag, Mag::Small(0, 0))
    }

    pub fn is_negative(&self) -> bool {
        self.neg
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Number of significant digits in the coefficient (0 for zero).
    pub fn precision(&self) -> u32 {
        match &self.mag {
            Mag::Small(lo, hi) => small_digits((*hi as u128) << 64 | *lo as u128),
            Mag::Big(limbs) => mag_digit_count(limbs),
        }
    }

    /// Trap 22003 if this (unconstrained) value exceeds the numeric-format caps
    /// (spec/design/decimal.md §2): more than `MAX_INT_DIGITS` integer-part digits or a
    /// scale over `MAX_SCALE` — PG's `make_result` / `numeric_in` checks.
    pub fn check_cap(self) -> Result<Decimal> {
        if self.scale > MAX_SCALE {
            return Err(overflow());
        }
        // An inline magnitude has at most 39 digits, far below the integer-digit cap.
        if let Mag::Big(limbs) = &self.mag
            && mag_digit_count(limbs).saturating_sub(self.scale) > MAX_INT_DIGITS
        {
            return Err(overflow());
        }
        Ok(self)
    }

    /// The value-canonical form `(neg, coeff, scale)` with trailing *fractional* zeros
    /// stripped (scale reduced): `1.50 → (false,15,1)`, `10.0 → (false,10,0)`, `100` stays
    /// `(false,100,0)`. Two values equal in value share this form (spec/design/decimal.md §5).
    /// The coefficient comes back as limbs: this is the form the float conversions and the
    /// number formatter use. Equality and hashing use [`Decimal::reduced`].
    fn canonical(&self) -> (bool, Vec<u32>, u32) {
        let limbs = self.limbs();
        if limbs.is_empty() {
            return (false, Vec::new(), 0);
        }
        let strip = mag_trailing_zeros(&limbs).min(self.scale);
        (
            self.neg,
            mag_div_pow10_exact(&limbs, strip),
            self.scale - strip,
        )
    }

    /// The value-canonical decimal: trailing fractional zeros stripped, zero at scale 0. Two
    /// values equal in value reduce to the same `(neg, scale, magnitude)` — the key for hashing
    /// and DISTINCT/GROUP BY (spec/design/decimal.md §5).
    fn reduced(&self) -> Decimal {
        match self.small() {
            Some(0) => Decimal::zero(0),
            Some(m) => {
                let (m, stripped) = small_strip_zeros(m, self.scale);
                Decimal::from_small(self.neg, self.scale - stripped, m)
            }
            None => {
                let (neg, limbs, scale) = self.canonical();
                Decimal::from_parts(neg, scale, limbs)
            }
        }
    }

    /// The value-canonical form as `(neg, coefficient digits, scale)` — the coefficient as a decimal
    /// digit string without leading zeros (`"0"` for zero), trailing fractional zeros stripped. Used
    /// by the number formatter (formatting.md §6), which lays out digits rather than limbs.
    pub(crate) fn canonical_digits(&self) -> (bool, String, u32) {
        let (neg, limbs, scale) = self.canonical();
        (neg, mag_to_decimal_str(&limbs), scale)
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
        let m = match (self.small(), other.small()) {
            (Some(a), Some(b)) => small_cmp_scaled(a, self.scale, b, other.scale),
            _ => self.cmp_mag_limbs(other),
        };
        if self.neg { m.reverse() } else { m }
    }

    /// Compare the two magnitudes by value, over limbs.
    fn cmp_mag_limbs(&self, other: &Decimal) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let (a, b) = (self.limbs(), other.limbs());
        // Only the operand of smaller scale is rescaled; equal scales compare in place.
        match self.scale.cmp(&other.scale) {
            Ordering::Equal => mag_cmp(&a, &b),
            Ordering::Less => mag_cmp(&mag_mul_pow10(&a, other.scale - self.scale), &b),
            Ordering::Greater => mag_cmp(&a, &mag_mul_pow10(&b, self.scale - other.scale)),
        }
    }

    /// Render to the canonical decimal string (spec/design/decimal.md §6): optional `-`, the
    /// integer digits, and — iff `scale > 0` — `.` and exactly `scale` fractional digits.
    pub fn render(&self) -> String {
        let mut buf = [0u8; SMALL_MAX_DIGITS];
        let owned;
        let digits: &[u8] = match self.small() {
            Some(0) => b"0",
            Some(m) => small_digit_bytes(m, &mut buf),
            None => {
                owned = mag_to_decimal_str(&self.limbs());
                owned.as_bytes()
            }
        };
        render_digits(self.neg, self.scale as usize, digits)
    }

    /// Arithmetic negation (sign flip; zero stays `+0`).
    pub fn neg(&self) -> Decimal {
        Decimal {
            neg: !self.neg && !self.is_zero(),
            scale: self.scale,
            mag: self.mag.clone(),
        }
    }

    /// `self + other`, exact, result scale `max(s1,s2)`, **without** the §2 format-cap check —
    /// the running form for the SUM/AVG accumulator, which (like PG) checks the cap only on the
    /// FINAL result, not each intermediate (spec/design/decimal.md §2, determinism.md §7). That
    /// makes the trap order-independent: whether a fold overflows no longer depends on the order
    /// rows are summed. Standalone arithmetic uses `add` (capped).
    pub fn add_uncapped(&self, other: &Decimal) -> Decimal {
        match self.add_small(other) {
            Some(sum) => sum,
            None => self.add_limbs(other),
        }
    }

    /// The sum when both magnitudes are inline and so are both aligned operands and the result.
    #[inline]
    fn add_small(&self, other: &Decimal) -> Option<Decimal> {
        let s = self.scale.max(other.scale);
        let a = small_scale_up(self.small()?, s - self.scale)?;
        let b = small_scale_up(other.small()?, s - other.scale)?;
        Some(if self.neg == other.neg {
            Decimal::from_small(self.neg, s, a.checked_add(b)?)
        } else if a >= b {
            Decimal::from_small(self.neg, s, a - b)
        } else {
            Decimal::from_small(other.neg, s, b - a)
        })
    }

    /// The sum over limbs, for any operands.
    fn add_limbs(&self, other: &Decimal) -> Decimal {
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs(), s - self.scale);
        let b = mag_mul_pow10(&other.limbs(), s - other.scale);
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
        let exact = self.mul_exact(other);
        let r = if exact.scale > MAX_SCALE {
            exact.round_to_scale(MAX_SCALE)
        } else {
            exact
        };
        r.check_cap()
    }

    /// Exact product (scale s1+s2), NO cap check — the running form of `*` for the kernels
    /// (PG `mul_var` does not cap; only `make_result` does).
    pub(crate) fn mul_exact(&self, other: &Decimal) -> Decimal {
        match self.mul_small(other) {
            Some(product) => product,
            None => self.mul_limbs(other),
        }
    }

    /// The exact product when both magnitudes and the product are inline.
    #[inline]
    fn mul_small(&self, other: &Decimal) -> Option<Decimal> {
        let m = self.small()?.checked_mul(other.small()?)?;
        Some(Decimal::from_small(
            self.neg ^ other.neg,
            self.scale + other.scale,
            m,
        ))
    }

    /// The exact product over limbs, for any operands.
    fn mul_limbs(&self, other: &Decimal) -> Decimal {
        Decimal::from_parts(
            self.neg ^ other.neg,
            self.scale + other.scale,
            mag_mul(&self.limbs(), &other.limbs()),
        )
    }

    /// `round_half_away(|self.coeff|·10^e / |other.coeff|)` with the quotient's sign, at scale
    /// `rscale` — the division step shared by `/` and the kernels' `div_var`. `e` is negative
    /// when the shift moves to the divisor (spec/design/decimal.md §4). `other` is not zero.
    fn div_at(&self, other: &Decimal, e: i64, rscale: u32) -> Decimal {
        match self.div_at_small(other, e, rscale) {
            Some(q) => q,
            None => self.div_at_limbs(other, e, rscale),
        }
    }

    /// The quotient when both magnitudes and the shifted operand are inline.
    fn div_at_small(&self, other: &Decimal, e: i64, rscale: u32) -> Option<Decimal> {
        let (a, b) = (self.small()?, other.small()?);
        let shift = u32::try_from(e.unsigned_abs()).ok()?;
        let (n, d) = if e >= 0 {
            (small_scale_up(a, shift)?, b)
        } else {
            (a, small_scale_up(b, shift)?)
        };
        let (mut q, r) = small_divmod(n, d);
        // Round half away from zero: 2·r >= d, written so it cannot overflow (r < d).
        if r >= d - r {
            q += 1;
        }
        Some(Decimal::from_small(self.neg ^ other.neg, rscale, q))
    }

    /// The quotient over limbs, for any operands.
    fn div_at_limbs(&self, other: &Decimal, e: i64, rscale: u32) -> Decimal {
        let (a, b) = (self.limbs(), other.limbs());
        let (numer, denom) = if e >= 0 {
            (mag_mul_pow10(&a, e as u32), b.to_vec())
        } else {
            (a.to_vec(), mag_mul_pow10(&b, (-e) as u32))
        };
        let (mut q, r) = mag_divmod(&numer, &denom);
        // Round half away from zero: if 2*r >= |divisor|, round the magnitude up.
        if mag_cmp(&mag_add(&r, &r), &denom) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg ^ other.neg, rscale, q)
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
        // E = rscale + s2 - s1. It is negative when the dividend's scale exceeds
        // select_div_scale's 1000-digit display clamp (s1 > 1000): the quotient then has FEWER
        // fractional digits than the dividend, so the shift moves to the divisor instead
        // (N = |C1|, D = |C2|·10^−E) — never a wrapped `as u32` shift (decimal.md §4).
        let e = rscale as i64 + other.scale as i64 - self.scale as i64;
        self.div_at(other, e, rscale).check_cap()
    }

    /// `self % other` — remainder of truncated division; result scale `max(s1,s2)`, sign of
    /// the dividend (matches the integer `%`). Traps 22012 on a zero divisor.
    pub fn rem(&self, other: &Decimal) -> Result<Decimal> {
        if other.is_zero() {
            return Err(div_by_zero());
        }
        Ok(match self.rem_small(other) {
            Some(r) => r,
            None => self.rem_limbs(other),
        })
    }

    /// The remainder when both magnitudes and both aligned operands are inline.
    fn rem_small(&self, other: &Decimal) -> Option<Decimal> {
        let s = self.scale.max(other.scale);
        let a = small_scale_up(self.small()?, s - self.scale)?;
        let b = small_scale_up(other.small()?, s - other.scale)?;
        Some(Decimal::from_small(self.neg, s, small_divmod(a, b).1))
    }

    /// The remainder over limbs, for any operands (`other` is not zero).
    fn rem_limbs(&self, other: &Decimal) -> Decimal {
        let s = self.scale.max(other.scale);
        let a = mag_mul_pow10(&self.limbs(), s - self.scale);
        let b = mag_mul_pow10(&other.limbs(), s - other.scale);
        let (_q, r) = mag_divmod(&a, &b);
        // Remainder takes the dividend's sign; zero normalizes to +0.
        Decimal::from_parts(self.neg, s, r)
    }

    /// Round to `target` scale, half away from zero (spec/design/decimal.md §3). Increasing
    /// the scale only appends zeros (exact).
    pub fn round_to_scale(&self, target: u32) -> Decimal {
        if let Some(m) = self.small() {
            if target < self.scale {
                let q = small_round_drop(m, self.scale - target);
                return Decimal::from_small(self.neg, target, q);
            }
            if let Some(up) = small_scale_up(m, target - self.scale) {
                return Decimal::from_small(self.neg, target, up);
            }
        }
        self.round_to_scale_limbs(target)
    }

    /// `round_to_scale` over limbs, for any value.
    fn round_to_scale_limbs(&self, target: u32) -> Decimal {
        let limbs = self.limbs();
        if target >= self.scale {
            let limbs = mag_mul_pow10(&limbs, target - self.scale);
            return Decimal::from_parts(self.neg, target, limbs);
        }
        let d = self.scale - target;
        let pow = mag_pow10(d);
        let (mut q, r) = mag_divmod(&limbs, &pow);
        // Half away: if 2*r >= 10^d, round the magnitude up.
        if mag_cmp(&mag_add(&r, &r), &pow) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg, target, q)
    }

    /// The magnitude (`abs`), preserving scale — the `abs(numeric)` scalar function
    /// (spec/design/functions.md §9). Cannot overflow.
    pub fn abs(&self) -> Decimal {
        Decimal {
            neg: false,
            scale: self.scale,
            mag: self.mag.clone(),
        }
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
        if let Some(m) = self.small()
            && let Some(scaled) = small_scale_up(small_round_drop(m, drop), k)
        {
            return Decimal::from_small(self.neg, 0, scaled).check_cap();
        }
        self.round_left_limbs(drop, k).check_cap()
    }

    /// Over limbs: drop `drop` digits rounding half away, then append `k` zeros, at scale 0.
    fn round_left_limbs(&self, drop: u32, k: u32) -> Decimal {
        let pow = mag_pow10(drop);
        let (mut q, r) = mag_divmod(&self.limbs(), &pow);
        if mag_cmp(&mag_add(&r, &r), &pow) != std::cmp::Ordering::Less {
            q = mag_add(&q, &[1]);
        }
        let scaled = mag_mul_pow10(&q, k);
        Decimal::from_parts(self.neg, 0, scaled)
    }

    /// Truncate toward zero to `target` scale — drop the dropped fractional digits, no rounding.
    /// Increasing the scale only appends zeros (exact). Truncation never grows the magnitude, so
    /// it cannot overflow. The toward-zero core of `trunc` (spec/design/functions.md §9).
    pub fn trunc_to_scale(&self, target: u32) -> Decimal {
        if let Some(m) = self.small() {
            if target < self.scale {
                let q = small_trunc_drop(m, self.scale - target);
                return Decimal::from_small(self.neg, target, q);
            }
            if let Some(up) = small_scale_up(m, target - self.scale) {
                return Decimal::from_small(self.neg, target, up);
            }
        }
        self.trunc_to_scale_limbs(target)
    }

    /// `trunc_to_scale` over limbs, for any value.
    fn trunc_to_scale_limbs(&self, target: u32) -> Decimal {
        let limbs = self.limbs();
        if target >= self.scale {
            let limbs = mag_mul_pow10(&limbs, target - self.scale);
            return Decimal::from_parts(self.neg, target, limbs);
        }
        let pow = mag_pow10(self.scale - target);
        let (q, _r) = mag_divmod(&limbs, &pow);
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
        if let Some(m) = self.small()
            && let Some(scaled) = small_scale_up(small_trunc_drop(m, self.scale + k), k)
        {
            return Decimal::from_small(self.neg, 0, scaled);
        }
        self.trunc_left_limbs(k)
    }

    /// Over limbs: drop the fraction and `k` integer digits, then append `k` zeros, at scale 0.
    fn trunc_left_limbs(&self, k: u32) -> Decimal {
        let pow = mag_pow10(self.scale + k);
        let (q, _r) = mag_divmod(&self.limbs(), &pow);
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
        if let Some(m) = self.small() {
            let q = small_trunc_drop(m, self.scale);
            // A fraction was dropped unless the kept part, scaled back, is the whole magnitude.
            let has_frac = small_scale_up(q, self.scale) != Some(m);
            let q = q + u128::from(has_frac && self.neg == toward_neg);
            return Decimal::from_small(self.neg, 0, q).check_cap();
        }
        self.round_to_bound_limbs(toward_neg).check_cap()
    }

    /// `round_to_bound` over limbs, for any value, without the cap check.
    fn round_to_bound_limbs(&self, toward_neg: bool) -> Decimal {
        let pow = mag_pow10(self.scale);
        let (mut q, r) = mag_divmod(&self.limbs(), &pow);
        let has_frac = r.iter().any(|&x| x != 0);
        if has_frac && self.neg == toward_neg {
            q = mag_add(&q, &[1]);
        }
        Decimal::from_parts(self.neg, 0, q)
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
        // A limb magnitude is at least 2^128, far outside i64.
        let v = i128::try_from(r.small()?).ok()?;
        let signed = if r.neg { -v } else { v };
        i64::try_from(signed).ok()
    }

    // --- on-disk value codec (base-10^4 groups, MS-first) — spec/fileformat/format.md ---

    /// `(neg, scale, base-10^4 coefficient groups MS-first)` for the value codec. Zero → no
    /// groups.
    pub fn to_codec(&self) -> (bool, u32, Vec<u16>) {
        let groups = match &self.mag {
            Mag::Small(lo, hi) => small_to_nbase4((*hi as u128) << 64 | *lo as u128),
            Mag::Big(limbs) => mag_to_nbase4(limbs),
        };
        (self.neg, self.scale, groups)
    }

    /// The number of base-10^4 groups `to_codec` would emit — `ceil(digits / 4)` of the unscaled
    /// magnitude, `0` for zero — without allocating them (the query-memory size schedule,
    /// spec/design/memory.md §3).
    pub fn codec_group_count(&self) -> usize {
        (self.precision() as usize).div_ceil(4)
    }

    /// Inverse of `to_codec` (used on load). `neg` is forced false for zero by normalize.
    pub fn from_codec(neg: bool, scale: u32, groups: &[u16]) -> Decimal {
        if groups.len() <= SMALL_CODEC_GROUPS {
            let m = groups.iter().fold(0u128, |m, &g| m * 10_000 + g as u128);
            return Decimal::from_small(neg, scale, m);
        }
        Decimal::from_parts(neg, scale, mag_from_nbase4(groups))
    }

    /// The value whose codec groups, most-significant first, fold to `m` — the load path for a
    /// value of at most [`SMALL_CODEC_GROUPS`] groups, which builds no group list
    /// (spec/fileformat/format.md).
    pub(crate) fn from_codec_small(neg: bool, scale: u32, m: u128) -> Decimal {
        Decimal::from_small(neg, scale, m)
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
        let mut buf = [0u8; SMALL_MAX_DIGITS];
        let owned;
        let digits: &[u8] = match self.small() {
            Some(m) => small_digit_bytes(m, &mut buf),
            None => {
                owned = mag_to_decimal_str(&self.limbs());
                owned.as_bytes()
            }
        };
        key_from_digits(self.neg, self.scale, digits)
    }

    /// Whether the coefficient is odd.
    fn coefficient_is_odd(&self) -> bool {
        match &self.mag {
            Mag::Small(lo, _) => lo & 1 == 1,
            // Limbs are little-endian base 10^9 (even), so the parity is limb 0's.
            Mag::Big(limbs) => limbs.first().is_some_and(|l| l % 2 == 1),
        }
    }
}

/// The canonical text of the coefficient `digits` (ASCII, no leading zeros, `"0"` for zero) at
/// display scale `scale` (spec/design/decimal.md §6).
fn render_digits(neg: bool, scale: usize, digits: &[u8]) -> String {
    let mut out = Vec::with_capacity(digits.len().max(scale + 1) + 2);
    if neg {
        out.push(b'-');
    }
    if scale == 0 {
        out.extend_from_slice(digits);
    } else if digits.len() <= scale {
        // No integer digits: a single `0`, then zeros up to the coefficient's first digit.
        out.extend_from_slice(b"0.");
        out.resize(out.len() + (scale - digits.len()), b'0');
        out.extend_from_slice(digits);
    } else {
        let point = digits.len() - scale;
        out.extend_from_slice(&digits[..point]);
        out.push(b'.');
        out.extend_from_slice(&digits[point..]);
    }
    String::from_utf8(out).expect("ASCII digits")
}

/// The `decimal-order-preserving` key body (spec/design/encoding.md §2.5) of the nonzero value
/// whose coefficient is `digits` (ASCII, no leading zeros) at scale `scale`.
fn key_from_digits(neg: bool, scale: u32, digits: &[u8]) -> Vec<u8> {
    // The base-10 decimal-point exponent: value = 0.<digits> × 10^decpt, decpt = precision − scale.
    let decpt = digits.len() as i32 - scale as i32;
    // Drop trailing zero digits (the least-significant ones): the mantissa keeps only its
    // significant part and decpt is unchanged, so `1.50` ("150") collapses onto `1.5` ("15").
    let trailing = digits.iter().rev().take_while(|&&d| d == b'0').count();
    let digits = &digits[..digits.len() - trailing];
    // Base-100 exponent E (value = 0.<pairs> × 100^E) and pair alignment: a leading zero digit
    // when decpt is odd, so the leading base-100 pair is "0 d1", and a zero past the last digit
    // to fill the last pair.
    let e = (decpt + 1).div_euclid(2);
    let lead = usize::from(decpt.rem_euclid(2) == 1);
    let digit = |i: usize| -> u8 {
        if i < lead {
            0
        } else {
            digits.get(i - lead).map_or(0, |d| d - b'0')
        }
    };
    let pairs = (lead + digits.len()).div_ceil(2);
    // Class byte ‖ 4-byte order-preserving exponent (`int-be-signflip`, §2.1) ‖ mantissa pairs
    // (pair+1) ‖ 0x00 terminator.
    let mut out = Vec::with_capacity(1 + 4 + pairs + 1);
    out.push(if neg { 0x03 } else { 0x05 });
    out.extend_from_slice(&((e as u32) ^ 0x8000_0000).to_be_bytes());
    for p in 0..pairs {
        out.push(digit(2 * p) * 10 + digit(2 * p + 1) + 1);
    }
    out.push(0x00);
    // Negatives complement the body (exponent, mantissa, terminator) but not the class byte.
    if neg {
        for b in &mut out[1..] {
            *b = !*b;
        }
    }
    out
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
/// numeric.c's truncated log10(e), used by the transcendentals' f64 weight estimates. Kept verbatim
/// rather than `std::f64::consts::LOG10_E` (a different f64): Go and TS use the same literal, and an
/// estimate truncated to an integer must agree across cores.
#[allow(clippy::approx_constant)]
const PG_LOG10_E: f64 = 0.434294481903252;
/// numeric.c's truncated log10(2) — kept verbatim for the same reason as [`PG_LOG10_E`].
#[allow(clippy::approx_constant)]
const PG_LOG10_2: f64 = 0.301029995663981;
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
        let (mut q, r) = mag_divmod(&self.limbs(), &pow);
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
        Ok(self.div_at(other, e, rscale.max(0) as u32))
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
    fn sqrt_var(&self, rscale: i64, m: Option<&mut Meter>) -> Result<Decimal> {
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
        decimal_math_step(m, rscale, true, &[self])?;
        let s = self.scale as i64;
        let mut kc = rscale.max(0) + 1;
        if 2 * kc < s {
            kc = (s + 1) / 2 + 1; // bumps kc so E ≥ 0; extra guard never changes the rounded result
        }
        let e = 2 * kc - s; // ≥ 0
        let n = mag_mul_pow10(&self.limbs(), e as u32);
        let g = mag_isqrt(&n);
        let at_guard = Decimal::from_parts(false, kc as u32, g);
        Ok(at_guard.round_var(rscale))
    }

    /// `sqrt(numeric)` (PG `numeric_sqrt`): choose rscale for ≥ MIN_SIG_DIGITS significant digits
    /// (never below the input dscale), then call `sqrt_var`.
    pub fn dec_sqrt_metered(&self, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[self])?;
        // sweight = floor(weight·DEC_DIGITS / 2) + 1 (DEC_DIGITS = 4 even ⇒ the division is exact).
        let sweight = self.nbase_weight() * DEC_DIGITS / 2 + 1;
        let mut rscale = MIN_SIG_DIGITS - sweight;
        rscale = rscale.max(self.scale as i64).clamp(0, MAX_DISPLAY_SCALE);
        self.sqrt_var(rscale, m)?.check_cap()
    }

    /// PG `exp_var(arg, result, rscale)`: e^self to `rscale` fractional digits via a range-reduced
    /// Taylor series. Traps 22003 on overflow (a true result outside the numeric format).
    fn exp_var(&self, rscale: i64, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), rscale, false, &[self])?;
        let mut x = self.clone();
        let mut val = self.to_f64_estimate();
        // Overflow / underflow guard (PG: fabs(val) >= NUMERIC_MAX_RESULT_SCALE·3 = 6000).
        if val.abs() >= (MAX_RESULT_SCALE * 3) as f64 {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Decimal::zero(rscale.max(0) as u32));
        }
        let dweight = (val * PG_LOG10_E) as i64; // decimal weight ≈ x·log10(e)
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
        let mut sig_digits = 1 + dweight + rscale + (ndiv2 as f64 * PG_LOG10_2) as i64;
        sig_digits = sig_digits.max(0) + 8;
        let local_rscale = sig_digits - 1;
        // Taylor: exp(x) = 1 + x + x²/2! + x³/3! + …
        let mut result = Decimal::from_i64(1).add_uncapped(&x);
        let mut elem = x.mul_var(&x, local_rscale);
        let mut ni = 2i64;
        elem = elem.div_var_int(ni, local_rscale)?;
        while !elem.is_zero() {
            decimal_math_step(m.as_deref_mut(), local_rscale, false, &[&result, &elem, &x])?;
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
            decimal_math_step(m.as_deref_mut(), rscale, false, &[&result])?;
            let lr = (sig_digits - result.nbase_weight() * 2 * DEC_DIGITS).max(0);
            result = result.mul_var(&result, lr);
        }
        Ok(result.round_var(rscale))
    }

    /// `exp(numeric)` (PG `numeric_exp`): choose rscale, then call `exp_var`.
    pub fn dec_exp_metered(&self, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[self])?;
        let mut val = self.to_f64_estimate() * PG_LOG10_E;
        val = val.clamp(-(MAX_RESULT_SCALE as f64), MAX_RESULT_SCALE as f64);
        let mut rscale = MIN_SIG_DIGITS - (val as i64);
        rscale = rscale.max(self.scale as i64).clamp(0, MAX_DISPLAY_SCALE);
        self.exp_var(rscale, m)?.check_cap()
    }

    /// PG `ln_var(arg, result, rscale)`: the natural log of self (> 0) to `rscale` fractional
    /// digits. Reduces self into (0.9, 1.1) by repeated `sqrt`, then sums the `atanh` series
    /// `2·(z + z³/3 + z⁵/5 + …)` with `z = (x−1)/(x+1)`. The caller guarantees self > 0.
    fn ln_var(&self, rscale: i64, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), rscale, false, &[self])?;
        let nine_tenths = Decimal::from_digits_scale(false, "9", 1); // 0.9
        let eleven_tenths = Decimal::from_digits_scale(false, "11", 1); // 1.1
        let two = Decimal::from_i64(2);
        let one = Decimal::from_i64(1);
        let mut x = self.clone();
        let mut fact = two.clone();
        let mut nsqrt = 0i64;
        while x.cmp_value(&nine_tenths) != std::cmp::Ordering::Greater {
            let local_rscale = rscale - x.nbase_weight() * DEC_DIGITS / 2 + 8;
            x = x.sqrt_var(local_rscale, m.as_deref_mut())?;
            fact = fact.mul_var(&two, 0);
            nsqrt += 1;
        }
        while x.cmp_value(&eleven_tenths) != std::cmp::Ordering::Less {
            let local_rscale = rscale - x.nbase_weight() * DEC_DIGITS / 2 + 8;
            x = x.sqrt_var(local_rscale, m.as_deref_mut())?;
            fact = fact.mul_var(&two, 0);
            nsqrt += 1;
        }
        let local_rscale = rscale + ((nsqrt + 1) as f64 * PG_LOG10_2) as i64 + 8;
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
            decimal_math_step(m.as_deref_mut(), local_rscale, false, &[&result, &xx, &zsq])?;
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
        Ok(result.mul_var(&fact, rscale))
    }

    /// Deterministic PG `estimate_ln_dweight(var)` — an estimate of `trunc(log10(|ln(var)|))`
    /// (PG truncates toward zero via `(int)`), used to pick the result rscale. Computed WITHOUT
    /// libm: branch 1 (0.9 ≤ var ≤ 1.1) is the exact decimal weight of `var − 1`; branch 2 is the
    /// decimal weight of a low-precision exact `ln`, adjusted from floor to trunc-toward-zero
    /// (`dweight + 1` when negative). `var > 0` (caller-guaranteed); returns 0 otherwise.
    fn estimate_ln_dweight(&self, m: Option<&mut Meter>) -> Result<i64> {
        if self.is_zero() || self.neg {
            return Ok(0);
        }
        let nine_tenths = Decimal::from_digits_scale(false, "9", 1);
        let eleven_tenths = Decimal::from_digits_scale(false, "11", 1);
        if self.cmp_value(&nine_tenths) != std::cmp::Ordering::Less
            && self.cmp_value(&eleven_tenths) != std::cmp::Ordering::Greater
        {
            let x = self.sub_uncapped(&Decimal::from_i64(1));
            if x.is_zero() {
                return Ok(0);
            }
            // floor(log10(|x|)) — PG's branch 1 (x.weight·DEC_DIGITS + floor(log10(digits[0]))).
            return Ok(x.precision() as i64 - 1 - x.scale as i64);
        }
        // |ln(self)| ≥ ln(1.1) ≈ 0.095 here, so a 20-digit guard captures its MSD position.
        let t = self.ln_var(20, m)?;
        if t.is_zero() {
            return Ok(0);
        }
        let dw = t.precision() as i64 - 1 - t.scale as i64; // floor(log10(|ln(self)|))
        Ok(if dw < 0 { dw + 1 } else { dw })
    }

    /// `ln(numeric)` (PG `numeric_ln`): choose rscale from the dweight estimate, then `ln_var`.
    pub fn dec_ln_metered(&self, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[self])?;
        if self.is_zero() {
            return Err(log_zero());
        }
        if self.neg {
            return Err(log_negative());
        }
        let ln_dweight = self.estimate_ln_dweight(m.as_deref_mut())?;
        let mut rscale = MIN_SIG_DIGITS - ln_dweight;
        rscale = rscale.max(self.scale as i64).clamp(0, MAX_DISPLAY_SCALE);
        self.ln_var(rscale, m)?.check_cap()
    }

    /// `log(base, num)` (PG `numeric_log` / `log_var`): logarithm of `num` in base `base`,
    /// `= ln(num) / ln(base)`. Both operands must be > 0 (else 2201E). Chooses its own rscale.
    pub fn dec_log_metered(
        base: &Decimal,
        num: &Decimal,
        mut m: Option<&mut Meter>,
    ) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[base, num])?;
        // ln_var(base) is formed first in PG, so a bad base reports first.
        for v in [base, num] {
            if v.is_zero() {
                return Err(log_zero());
            }
            if v.neg {
                return Err(log_negative());
            }
        }
        let ln_base_dweight = base.estimate_ln_dweight(m.as_deref_mut())?;
        let ln_num_dweight = num.estimate_ln_dweight(m.as_deref_mut())?;
        let result_dweight = ln_num_dweight - ln_base_dweight;
        let mut rscale = MIN_SIG_DIGITS - result_dweight;
        rscale = rscale
            .max(base.scale as i64)
            .max(num.scale as i64)
            .clamp(0, MAX_DISPLAY_SCALE);
        let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(0);
        let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(0);
        let ln_base = base.ln_var(ln_base_rscale, m.as_deref_mut())?;
        let ln_num = num.ln_var(ln_num_rscale, m)?;
        ln_num.div_var(&ln_base, rscale)?.check_cap()
    }

    /// `log(numeric)` / `log10(numeric)` — base-10 logarithm (PG defines one-arg `log` as
    /// `log(10, x)`).
    pub fn dec_log10_metered(&self, mut m: Option<&mut Meter>) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[self])?;
        Decimal::dec_log_metered(&Decimal::from_i64(10), self, m)
    }

    /// PG `power_var_int(base, exp, exp_dscale)`: base^exp for an integer exp, by binary
    /// exponentiation with a per-multiplication rscale that keeps a fixed significant-digit count.
    fn power_var_int(
        base: &Decimal,
        exp: i32,
        exp_dscale: u32,
        mut m: Option<&mut Meter>,
    ) -> Result<Decimal> {
        // Estimate the decimal weight of the result, f ≈ exp · log10(|base|) (PG uses libm log10
        // on the leading-digit MAGNITUDE; jed computes log10(|base|) = ln(|base|)/ln(10) exactly —
        // deterministic). The MAGNITUDE matters: the binary-exponentiation muls below carry the
        // sign, but ln() is only defined on the positive magnitude.
        let f: f64 = if base.is_zero() {
            0.0
        } else {
            base.abs()
                .log10_estimate(m.as_deref_mut())?
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
            .clamp(0, MAX_DISPLAY_SCALE);
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
        sig_digits += int_ln_floor(exp.unsigned_abs() as u64, m.as_deref_mut())? + 8;
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
            decimal_math_step(m.as_deref_mut(), rscale, false, &[&base_prod, &result])?;
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
    fn power_var(base: &Decimal, exp: &Decimal, mut m: Option<&mut Meter>) -> Result<Decimal> {
        if let Some(iexp) = exp.to_i32_if_integer() {
            return Decimal::power_var_int(base, iexp, exp.scale, m.as_deref_mut());
        }
        // 0 ^ non-integer = 0 (0 ^ negative was rejected by the caller).
        if base.is_zero() {
            return Ok(Decimal::zero(MIN_SIG_DIGITS as u32));
        }
        // A negative base insists on an integer exponent (PG power_var): an integer exponent
        // outside power_var_int's int32 gate still arrives here, so integrality is tested by
        // value, not by the gate. The result's sign is the exponent's parity (odd → negative)
        // and its magnitude is computed over |base| (decimal.md §8).
        let abs_base;
        let mut res_neg = false;
        let base = if base.neg {
            let int_exp = exp.trunc_to_scale(0);
            if int_exp.cmp_value(exp) != std::cmp::Ordering::Equal {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForPowerFunction,
                    "a negative number raised to a non-integer power yields a complex result",
                ));
            }
            // At scale 0 the coefficient is the integer, so its parity is the exponent's.
            res_neg = int_exp.coefficient_is_odd();
            abs_base = base.abs();
            &abs_base
        } else {
            base
        };
        let ln_dweight = base.estimate_ln_dweight(m.as_deref_mut())?;
        // Low-precision exp·ln(base) to estimate the result weight (and a crude overflow exit).
        let mut local_rscale = (8 - ln_dweight).max(0);
        let mut ln_base = base.ln_var(local_rscale, m.as_deref_mut())?;
        let mut ln_num = ln_base.mul_var(exp, local_rscale);
        let mut val = ln_num.to_f64_estimate();
        if val.abs() > MAX_RESULT_SCALE as f64 * 3.01 {
            if val > 0.0 {
                return Err(overflow());
            }
            return Ok(Decimal::zero(MAX_DISPLAY_SCALE as u32));
        }
        val *= PG_LOG10_E;
        let vi = val as i64;
        let mut rscale = MIN_SIG_DIGITS - vi;
        rscale = rscale
            .max(base.scale as i64)
            .max(exp.scale as i64)
            .clamp(0, MAX_DISPLAY_SCALE);
        let sig_digits = (rscale + vi).max(0);
        local_rscale = (sig_digits - ln_dweight + 8).max(0);
        ln_base = base.ln_var(local_rscale, m.as_deref_mut())?;
        ln_num = ln_base.mul_var(exp, local_rscale);
        let mut r = ln_num.exp_var(rscale, m)?;
        if res_neg && !r.is_zero() {
            r = r.neg(); // a zero result stays positive (no negative zero)
        }
        r.check_cap()
    }

    /// `power(base, exp)` over numeric (PG `numeric_power`, finite path): the domain checks plus
    /// `power_var`. `0 ^ negative` traps 2201F.
    pub fn dec_power_metered(
        base: &Decimal,
        exp: &Decimal,
        mut m: Option<&mut Meter>,
    ) -> Result<Decimal> {
        decimal_math_step(m.as_deref_mut(), 0, false, &[base, exp])?;
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
        Decimal::power_var(base, exp, m)
    }

    /// `log10(self) = ln(self)/ln(10)` to a ~30-digit guard — the deterministic, libm-free
    /// replacement for PG `power_var_int`'s `log10(double)` result-weight estimate. `self > 0`.
    fn log10_estimate(&self, mut m: Option<&mut Meter>) -> Result<Decimal> {
        let guard = 30;
        let ln_self = self.ln_var(guard, m.as_deref_mut())?;
        let ln_ten = Decimal::from_i64(10).ln_var(guard, m)?;
        ln_self.div_var(&ln_ten, guard)
    }
}

/// `floor(ln(n))` for `n >= 1`, computed deterministically via the exact `ln` (no libm) — PG's
/// `(int)log(fabs(exp))` guard term in `power_var_int`. `n == 0` → 0.
fn int_ln_floor(n: u64, m: Option<&mut Meter>) -> Result<i64> {
    if n <= 1 {
        return Ok(0); // ln(1) = 0; ln(0) is never reached (exp != 0 in the guard)
    }
    // ln(n) ≥ 0; trunc-to-scale-0 of a 12-digit-guard ln is its floor for any non-boundary n.
    Ok(Decimal::from_i64(n as i64)
        .ln_var(12, m)?
        .trunc_to_scale(0)
        .to_i64_round()
        .unwrap_or(0))
}

/// `floor(√n)` for a magnitude `n` (base-10⁹ LSB-first), via Newton's method on big integers
/// (`x ← (x + n/x)/2` from `x₀ = 10^⌈digits/2⌉ ≥ √n`, monotone-decreasing to the floor). The
/// exact integer square root underlying `sqrt_var` — deterministic, no float seed.
fn mag_isqrt(n: &[u32]) -> Vec<u32> {
    if n.is_empty() {
        return Vec::new();
    }
    let half = mag_digit_count(n).div_ceil(2); // ⌈digits/2⌉
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
        // At one scale the values agree exactly when the coefficients and signs do: a magnitude
        // has one representation, and zero is never negative.
        if self.scale == other.scale {
            return self.neg == other.neg && self.mag == other.mag;
        }
        self.cmp_value(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Decimal {}
impl std::hash::Hash for Decimal {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Equal values reduce to the same sign, scale, and magnitude representation.
        let reduced = self.reduced();
        reduced.neg.hash(state);
        reduced.scale.hash(state);
        match &reduced.mag {
            Mag::Small(lo, hi) => {
                lo.hash(state);
                hi.hash(state);
            }
            Mag::Big(limbs) => limbs.hash(state),
        }
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
    if let Some(m) = d.small() {
        // The top `g` digits, zero-filled on the right when the coefficient has fewer.
        let (digits, g) = (digits as u32, g as u32);
        let f = if digits >= g {
            small_trunc_drop(m, digits - g)
        } else {
            m * POW10_U128[(g - digits) as usize]
        };
        return (w, f as i64);
    }
    let s = mag_to_decimal_str(&d.limbs());
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

/// W for div: numerator groups × divisor groups, where the rescale shift `E = rscale + s2 − s1`
/// (the same `select_div_scale` as the result) widens the dividend when `E ≥ 0` and the divisor
/// when `E < 0` (s1 > 1000, past the display clamp) — the operands `div` actually divides
/// (cost.md §3). A zero divisor returns 1 — the operation traps 22012 before any work.
pub fn work_div(a: &Decimal, b: &Decimal) -> u64 {
    if b.is_zero() {
        return 1;
    }
    let rscale = select_div_scale(a, b);
    let e = rscale as i64 + b.scale as i64 - a.scale as i64;
    if e >= 0 {
        groups(a.precision() + e as u32) * groups(b.precision())
    } else {
        groups(a.precision()) * groups(b.precision() + (-e) as u32)
    }
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
// Inline magnitudes — a coefficient below 2^128 as one `u128` (decimal.md §1).
// Each helper is exact integer arithmetic; where a result would not fit 128 bits
// it returns `None` and the caller takes the limb path below, which is also the
// reference these are tested against.
// ============================================================================

/// The most decimal digits a `u128` has (`u128::MAX` is about 3.4 × 10^38).
const SMALL_MAX_DIGITS: usize = 39;

/// The most base-10^4 codec groups that always fit a `u128`: 9 groups are below 10^36.
pub(crate) const SMALL_CODEC_GROUPS: usize = 9;

/// 10^0 ..= 10^38 — every power of ten a `u128` holds.
const POW10_U128: [u128; 39] = {
    let mut table = [1u128; 39];
    let mut i = 1;
    while i < table.len() {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// Number of significant decimal digits (0 for zero).
#[inline]
fn small_digits(m: u128) -> u32 {
    if m == 0 {
        0
    } else if let Ok(m) = u64::try_from(m) {
        m.ilog10() + 1
    } else {
        m.ilog10() + 1
    }
}

/// `m · 10^d`, or `None` when that does not fit 128 bits.
#[inline]
fn small_scale_up(m: u128, d: u32) -> Option<u128> {
    if d == 0 || m == 0 {
        return Some(m);
    }
    m.checked_mul(*POW10_U128.get(d as usize)?)
}

/// `(n div d, n mod d)` for `d != 0`, in 64-bit arithmetic when both fit.
#[inline]
fn small_divmod(n: u128, d: u128) -> (u128, u128) {
    if let (Ok(n), Ok(d)) = (u64::try_from(n), u64::try_from(d)) {
        return ((n / d) as u128, (n % d) as u128);
    }
    (n / d, n % d)
}

/// `m` without its low `d` decimal digits (truncated toward zero).
#[inline]
fn small_trunc_drop(m: u128, d: u32) -> u128 {
    match POW10_U128.get(d as usize) {
        Some(&pow) => small_divmod(m, pow).0,
        None => 0, // 10^39 exceeds every u128
    }
}

/// `m` without its low `d` decimal digits, rounded half away from zero in magnitude
/// (spec/design/decimal.md §3: up exactly when `2·r >= 10^d`).
#[inline]
fn small_round_drop(m: u128, d: u32) -> u128 {
    let Some(&pow) = POW10_U128.get(d as usize) else {
        return 0; // 10^39 exceeds 2·u128::MAX, so the magnitude is below half
    };
    let (q, r) = small_divmod(m, pow);
    // r < 10^38, so 2·r fits; q + 1 fits because q <= m.
    if d > 0 && 2 * r >= pow { q + 1 } else { q }
}

/// `m` with up to `max` trailing decimal zeros removed, and how many were (`m != 0`).
fn small_strip_zeros(m: u128, max: u32) -> (u128, u32) {
    let mut stripped = 0;
    if let Ok(mut low) = u64::try_from(m) {
        while stripped < max && low % 10 == 0 {
            low /= 10;
            stripped += 1;
        }
        return (low as u128, stripped);
    }
    let mut m = m;
    while stripped < max && m % 10 == 0 {
        m /= 10;
        stripped += 1;
    }
    (m, stripped)
}

/// Compare `a·10^-sa` with `b·10^-sb`.
#[inline]
fn small_cmp_scaled(a: u128, sa: u32, b: u128, sb: u32) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Only the operand of smaller scale is rescaled. A rescaled operand that no longer fits
    // 128 bits exceeds the other, which does.
    match sa.cmp(&sb) {
        Ordering::Equal => a.cmp(&b),
        Ordering::Less => small_scale_up(a, sb - sa).map_or(Ordering::Greater, |a| a.cmp(&b)),
        Ordering::Greater => small_scale_up(b, sa - sb).map_or(Ordering::Less, |b| a.cmp(&b)),
    }
}

/// The decimal digits of `m` (ASCII, most-significant first, no leading zeros; empty for
/// zero), written at the end of `buf`.
fn small_digit_bytes(m: u128, buf: &mut [u8; SMALL_MAX_DIGITS]) -> &[u8] {
    let mut at = buf.len();
    let mut high = m;
    // Peel 18 digits at a time so the digit loop runs in 64-bit arithmetic.
    while high > u64::MAX as u128 {
        let mut low = (high % 1_000_000_000_000_000_000) as u64;
        high /= 1_000_000_000_000_000_000;
        for _ in 0..18 {
            at -= 1;
            buf[at] = b'0' + (low % 10) as u8;
            low /= 10;
        }
    }
    let mut low = high as u64;
    while low != 0 {
        at -= 1;
        buf[at] = b'0' + (low % 10) as u8;
        low /= 10;
    }
    &buf[at..]
}

/// The base-10^4 codec groups of `m`, most-significant first (zero → empty).
fn small_to_nbase4(m: u128) -> Vec<u16> {
    let mut out = Vec::with_capacity(SMALL_MAX_DIGITS.div_ceil(4));
    let mut high = m;
    while high > u64::MAX as u128 {
        out.push((high % 10_000) as u16);
        high /= 10_000;
    }
    let mut low = high as u64;
    while low != 0 {
        out.push((low % 10_000) as u16);
        low /= 10_000;
    }
    out.reverse();
    out
}

/// `m` as base-10^9 limbs, least-significant first, and how many of the five are used.
fn small_to_limbs(m: u128) -> ([u32; 5], usize) {
    let mut buf = [0u32; 5];
    let mut len = 0;
    let mut m = m;
    while m != 0 {
        let (q, r) = small_divmod(m, BASE as u128);
        buf[len] = r as u32;
        len += 1;
        m = q;
    }
    (buf, len)
}

/// The value of trimmed limbs when it is below 2^128, else `None`.
fn limbs_to_small(limbs: &[u32]) -> Option<u128> {
    if limbs.len() > 5 {
        return None; // at least 10^45
    }
    limbs.iter().rev().try_fold(0u128, |m, &limb| {
        m.checked_mul(BASE as u128)?.checked_add(limb as u128)
    })
}

/// A magnitude as base-10^9 limbs (least-significant first, no high zero limbs): an inline
/// magnitude expanded into a local buffer, or a limb magnitude borrowed.
enum LimbView<'a> {
    Inline([u32; 5], usize),
    Borrowed(&'a [u32]),
}

impl std::ops::Deref for LimbView<'_> {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        match self {
            LimbView::Inline(buf, len) => &buf[..*len],
            LimbView::Borrowed(limbs) => limbs,
        }
    }
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
    // The top limb is non-zero in a normalized magnitude; `max(1)` keeps a denormal zero limb at
    // one digit, as rendering it would.
    let high = limbs[limbs.len() - 1].max(1).ilog10() + 1;
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

/// The number of trailing decimal zeros of a non-zero magnitude.
fn mag_trailing_zeros(limbs: &[u32]) -> u32 {
    let zero_limbs = limbs.iter().take_while(|&&limb| limb == 0).count();
    let mut low = limbs[zero_limbs];
    let mut zeros = zero_limbs as u32 * BASE_DIGITS;
    while low % 10 == 0 {
        low /= 10;
        zeros += 1;
    }
    zeros
}

/// Divide by 10^e, which must divide the magnitude exactly.
fn mag_div_pow10_exact(a: &[u32], e: u32) -> Vec<u32> {
    let mut out = a[(e / BASE_DIGITS) as usize..].to_vec();
    let den = 10u64.pow(e % BASE_DIGITS);
    if den > 1 {
        let mut rem = 0u64;
        for limb in out.iter_mut().rev() {
            let cur = rem * BASE + *limb as u64;
            *limb = (cur / den) as u32;
            rem = cur % den;
        }
        debug_assert_eq!(rem, 0);
        mag_trim(&mut out);
    }
    out
}

/// 10^e as a magnitude.
fn mag_pow10(e: u32) -> Vec<u32> {
    mag_mul_pow10(&[1], e)
}

/// Long division: `(quotient, remainder)` of `num / den` (den != 0). A one-limb divisor divides
/// limb by limb. Otherwise each quotient limb is found by binary search, exactly, inside a bracket
/// the leading limbs prove contains it, so the search takes a step or two rather than thirty.
fn mag_divmod(num: &[u32], den: &[u32]) -> (Vec<u32>, Vec<u32>) {
    debug_assert!(!den.is_empty());
    if mag_cmp(num, den) == std::cmp::Ordering::Less {
        return (Vec::new(), num.to_vec());
    }
    let mut quo = vec![0u32; num.len()];
    if let [den] = *den {
        let den = den as u64;
        let mut rem = 0u64;
        for i in (0..num.len()).rev() {
            let cur = rem * BASE + num[i] as u64; // rem < den < BASE, so this is below BASE²
            quo[i] = (cur / den) as u32;
            rem = cur % den;
        }
        mag_trim(&mut quo);
        return (quo, mag_from_u64(rem));
    }
    let n = den.len();
    // The divisor's top two limbs: den is in [den_top, den_top + 1) × BASE^(n-2).
    let den_top = den[n - 1] as u128 * BASE as u128 + den[n - 2] as u128;
    let mut rem: Vec<u32> = Vec::with_capacity(n + 1);
    for i in (0..num.len()).rev() {
        // rem = rem * BASE + num[i]  (shift up one limb, set the low limb).
        rem.insert(0, num[i]);
        mag_trim(&mut rem);
        // Largest q in [0, BASE) with den*q <= rem. rem < den × BASE has at most n + 1 limbs, so
        // its limbs from n-2 up are at most three: rem is in [top, top + 1) × BASE^(n-2), which
        // puts q in [top / (den_top + 1), (top + 1) / den_top].
        let (mut lo, mut hi) = if rem.len() < n {
            (0, 0)
        } else {
            let top = rem[n - 2..]
                .iter()
                .rev()
                .fold(0u128, |top, &limb| top * BASE as u128 + limb as u128);
            (
                (top / (den_top + 1)) as u64,
                (((top + 1) / den_top) as u64).min(BASE - 1),
            )
        };
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
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
    // Four limbs are 36 digits, exactly nine groups, so the radix converts chunk by chunk from the
    // low end. Groups are collected low-first and reversed.
    let mut out = Vec::with_capacity(limbs.len().div_ceil(4) * 9);
    for chunk in limbs.chunks(4) {
        let mut v = chunk
            .iter()
            .rev()
            .fold(0u128, |v, &limb| v * BASE as u128 + limb as u128);
        for _ in 0..9 {
            out.push((v % 10_000) as u16);
            v /= 10_000;
        }
    }
    while out.last() == Some(&0) {
        out.pop();
    }
    out.reverse();
    out
}

/// MS-first base-10^4 groups → LSB-first base-10^9 limbs (inverse of `mag_to_nbase4`).
fn mag_from_nbase4(groups: &[u16]) -> Vec<u32> {
    if groups.is_empty() {
        return Vec::new();
    }
    // Nine groups are 36 digits, exactly four limbs (see `mag_to_nbase4`).
    let mut out = Vec::with_capacity(groups.len().div_ceil(9) * 4);
    for chunk in groups.rchunks(9) {
        let mut v = chunk.iter().fold(0u128, |v, &g| v * 10_000 + g as u128);
        for _ in 0..4 {
            out.push((v % BASE as u128) as u32);
            v /= BASE as u128;
        }
    }
    mag_trim(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic stream of magnitudes of 0..=`max_limbs` limbs, biased toward the limb
    /// values where carries and quotient estimates are tightest.
    fn sample_mags(seed: u64, count: usize, max_limbs: usize) -> Vec<Vec<u32>> {
        let mut state = seed;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        (0..count)
            .map(|_| {
                let len = (next() % (max_limbs as u64 + 1)) as usize;
                let mut mag: Vec<u32> = (0..len)
                    .map(|_| match next() % 5 {
                        0 => 0,
                        1 => (BASE - 1) as u32,
                        2 => (next() % 10) as u32,
                        _ => (next() % BASE) as u32,
                    })
                    .collect();
                mag_trim(&mut mag);
                mag
            })
            .collect()
    }

    /// The bracketed long division returns exactly what division defines: `num = quo × den + rem`
    /// with `rem < den`.
    #[test]
    fn mag_divmod_satisfies_the_division_identity() {
        let nums = sample_mags(1, 400, 9);
        let dens = sample_mags(2, 400, 5);
        for num in &nums {
            for den in dens.iter().filter(|den| !den.is_empty()) {
                let (quo, rem) = mag_divmod(num, den);
                assert_eq!(
                    mag_cmp(&rem, den),
                    std::cmp::Ordering::Less,
                    "{num:?} / {den:?}"
                );
                assert_eq!(
                    &mag_add(&mag_mul(&quo, den), &rem),
                    num,
                    "{num:?} / {den:?}"
                );
            }
        }
    }

    /// The chunked radix conversions agree with the digit strings they replaced, and invert each
    /// other.
    #[test]
    fn nbase4_conversions_match_the_digit_string() {
        for mag in sample_mags(3, 2000, 11) {
            let digits = mag_to_decimal_str(&mag);
            let groups = mag_to_nbase4(&mag);
            let rendered: String = match groups.split_first() {
                None => "0".to_string(),
                Some((first, rest)) => {
                    first.to_string() + &rest.iter().map(|g| format!("{g:04}")).collect::<String>()
                }
            };
            assert_eq!(rendered, digits);
            assert_eq!(mag_from_nbase4(&groups), mag);
        }
    }

    /// The canonical form strips exactly the trailing fractional zeros the digit string shows.
    #[test]
    fn canonical_strips_trailing_fractional_zeros() {
        for mag in sample_mags(4, 2000, 6) {
            for scale in [0u32, 1, 8, 9, 10, 27, 40] {
                let value = Decimal::from_parts(false, scale, mag.clone());
                let (_, coeff, canon_scale) = value.canonical();
                let mut digits = mag_to_decimal_str(&mag);
                let mut want_scale = scale;
                while want_scale > 0 && digits.ends_with('0') && !mag.is_empty() {
                    digits.pop();
                    want_scale -= 1;
                }
                if mag.is_empty() {
                    want_scale = 0;
                }
                assert_eq!(
                    (mag_to_decimal_str(&coeff), canon_scale),
                    (digits, want_scale)
                );
            }
        }
    }

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
        by_key.sort_by_key(|a| a.encode_key());
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

    // ---- inline magnitudes: every inline path against the limb path (decimal.md §1) ----

    /// Same sign, scale, and magnitude representation — stricter than value equality, so a
    /// result held the wrong way (a small magnitude left as limbs) fails too.
    fn identical(a: &Decimal, b: &Decimal) -> bool {
        a.neg == b.neg && a.scale == b.scale && a.mag == b.mag
    }

    /// The representation invariants: a limb magnitude is trimmed and at least 2^128; zero is
    /// not negative.
    fn assert_canonical(d: &Decimal) {
        match &d.mag {
            Mag::Small(0, 0) => assert!(!d.neg, "negative zero"),
            Mag::Small(..) => {}
            Mag::Big(limbs) => {
                assert_ne!(limbs.last(), Some(&0), "untrimmed limbs");
                assert_eq!(
                    limbs_to_small(limbs),
                    None,
                    "a small magnitude held as limbs"
                );
            }
        }
    }

    /// Magnitudes at every boundary the inline arithmetic branches on — each power of ten and
    /// its half (the rounding tie), the 64- and 128-bit edges — plus a pseudo-random spread of
    /// every bit width.
    fn inline_mags() -> Vec<u128> {
        let mut out = vec![
            0,
            2,
            3,
            7,
            u128::MAX,
            u128::MAX - 1,
            u128::MAX / 2,
            u128::MAX / 10,
        ];
        for edge in [u32::MAX as u128, u64::MAX as u128, BASE as u128] {
            out.extend([edge - 1, edge, edge + 1]);
        }
        for pow in POW10_U128 {
            out.extend([pow.saturating_sub(1), pow, pow + 1]);
            let half = pow / 2; // 5 × the power below: the tie when that many digits drop
            out.extend([half.saturating_sub(1), half, half + 1]);
        }
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for bits in 1..=128u32 {
            let wide = (next() as u128) << 64 | next() as u128;
            out.push(wide >> (128 - bits));
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Values over [`inline_mags`] at scales on both sides of the 38-digit shift limit, both
    /// signs, plus a few magnitudes past 2^128 so the limb representation takes part.
    fn inline_values() -> Vec<Decimal> {
        let mut out = Vec::new();
        for (i, &m) in inline_mags().iter().enumerate() {
            for (j, scale) in [0u32, 1, 2, 4, 9, 18, 19, 20, 37, 38, 39, 40, 77]
                .into_iter()
                .enumerate()
            {
                // A third of the combinations, spread over every magnitude and every scale.
                if (i + j) % 3 == 0 {
                    out.push(Decimal::from_small((i + j) % 2 == 1, scale, m));
                }
            }
        }
        let past = mag_add(&small_to_limbs(u128::MAX).0, &[1]); // 2^128
        for (extra, scale) in [(0u32, 0u32), (0, 3), (1, 40), (9, 2), (20, 0)] {
            out.push(Decimal::from_parts(
                extra % 2 == 1,
                scale,
                mag_mul_pow10(&past, extra),
            ));
        }
        out
    }

    fn hash_of(d: &Decimal) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        d.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn decimal_stays_32_bytes() {
        assert_eq!(std::mem::size_of::<Decimal>(), 32);
    }

    #[test]
    fn inline_helpers_match_their_definitions() {
        for m in inline_mags() {
            let text = m.to_string();
            let mut buf = [0u8; SMALL_MAX_DIGITS];
            let digits = small_digit_bytes(m, &mut buf);
            assert_eq!(digits, if m == 0 { "" } else { text.as_str() }.as_bytes());
            assert_eq!(small_digits(m) as usize, digits.len());
            let (buf, len) = small_to_limbs(m);
            assert_eq!(mag_to_decimal_str(&buf[..len]), text);
            assert_eq!(limbs_to_small(&buf[..len]), Some(m));
            assert_eq!(small_to_nbase4(m), mag_to_nbase4(&buf[..len]));
        }
        // One past the largest inline magnitude is a limb magnitude, and one back is inline again.
        let top = Decimal::from_small(false, 0, u128::MAX);
        let one = Decimal::from_i64(1);
        let past = top.add_uncapped(&one);
        assert!(matches!(past.mag, Mag::Big(_)));
        assert_canonical(&past);
        assert!(identical(&past.add_uncapped(&one.neg()), &top));
    }

    #[test]
    fn inline_unary_paths_match_the_limb_paths() {
        for d in inline_values() {
            assert_canonical(&d);
            let limbs = d.limbs().to_vec();
            let digits = mag_to_decimal_str(&limbs);
            assert_eq!(d.precision(), mag_digit_count(&limbs));
            assert_eq!(
                d.render(),
                render_digits(d.neg, d.scale as usize, digits.as_bytes())
            );
            assert_eq!(
                d.to_i64_round(),
                d.round_to_scale_limbs(0)
                    .render()
                    .parse::<i128>()
                    .ok()
                    .and_then(|v| i64::try_from(v).ok())
            );
            if !d.is_zero() {
                assert_eq!(
                    d.encode_key(),
                    key_from_digits(d.neg, d.scale, digits.as_bytes())
                );
            }

            // Codec: the groups, their count, and both ways back.
            let (neg, scale, groups) = d.to_codec();
            assert_eq!(groups, mag_to_nbase4(&limbs));
            assert_eq!(d.codec_group_count(), groups.len());
            assert!(identical(&Decimal::from_codec(neg, scale, &groups), &d));
            assert!(identical(
                &Decimal::from_parts(neg, scale, mag_from_nbase4(&groups)),
                &d
            ));
            assert!(identical(
                &Decimal::from_digits_scale(d.neg, &digits, d.scale),
                &d
            ));

            // The value-canonical form, by both routes.
            let (cneg, climbs, cscale) = d.canonical();
            assert!(identical(
                &d.reduced(),
                &Decimal::from_parts(cneg, cscale, climbs)
            ));

            for target in [0u32, 1, 2, 5, 17, 18, 19, 36, 38, 39, 41, 80] {
                let rounded = d.round_to_scale(target);
                assert!(
                    identical(&rounded, &d.round_to_scale_limbs(target)),
                    "{d:?} round {target}"
                );
                assert_canonical(&rounded);
                let truncated = d.trunc_to_scale(target);
                assert!(
                    identical(&truncated, &d.trunc_to_scale_limbs(target)),
                    "{d:?} trunc {target}"
                );
                assert_canonical(&truncated);
            }
            for n in [-1i64, -2, -9, -19, -38, -39, -45] {
                let k = n.unsigned_abs().min((d.precision() + 1) as u64) as u32;
                let rounded = d.round_places(n).unwrap();
                assert!(
                    identical(&rounded, &d.round_left_limbs(d.scale + k, k)),
                    "{d:?} round {n}"
                );
                let truncated = d.trunc_places(n);
                assert!(
                    identical(&truncated, &d.trunc_left_limbs(k)),
                    "{d:?} trunc {n}"
                );
            }
            if d.scale > 0 {
                for toward_neg in [false, true] {
                    assert!(
                        identical(
                            &d.round_to_bound(toward_neg).unwrap(),
                            &d.round_to_bound_limbs(toward_neg)
                        ),
                        "{d:?} bound {toward_neg}"
                    );
                }
            }

            // select_div_scale's leading group, against the digit string it is defined on.
            if !d.is_zero() {
                let e = digits.len() as i64 - 1 - d.scale as i64;
                let g = (e - 4 * e.div_euclid(4) + 1) as usize;
                let lead: i64 = (0..g).fold(0, |f, i| {
                    f * 10 + digits.as_bytes().get(i).map_or(0, |b| (b - b'0') as i64)
                });
                assert_eq!(nbase4_weight_lead(&d), (e.div_euclid(4), lead));
            }
        }
    }

    #[test]
    fn inline_binary_paths_match_the_limb_paths() {
        let values = inline_values();
        let count = values.len() as u64;
        let mut state = 0x1319_8A2E_0370_7344u64;
        let mut pick = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) % count) as usize
        };
        for round in 0..40_000 {
            // Every value meets itself and its neighbour once; the rest are random pairs.
            let (a, b) = match round {
                r if r < values.len() => (&values[r], &values[r]),
                r if r < 2 * values.len() => {
                    (&values[r - values.len()], &values[(r + 1) % values.len()])
                }
                _ => (&values[pick()], &values[pick()]),
            };

            let sum = a.add_uncapped(b);
            assert!(identical(&sum, &a.add_limbs(b)), "{a:?} + {b:?}");
            assert_canonical(&sum);
            let difference = a.add_uncapped(&b.neg());
            assert!(
                identical(&difference, &a.add_limbs(&b.neg())),
                "{a:?} - {b:?}"
            );
            assert_canonical(&difference);
            let product = a.mul_exact(b);
            assert!(identical(&product, &a.mul_limbs(b)), "{a:?} * {b:?}");
            assert_canonical(&product);

            // Ordering, equality, and hashing agree with the limb comparison.
            let by_limbs = if a.neg != b.neg {
                b.neg.cmp(&a.neg)
            } else if a.neg {
                a.cmp_mag_limbs(b).reverse()
            } else {
                a.cmp_mag_limbs(b)
            };
            assert_eq!(a.cmp_value(b), by_limbs, "{a:?} <=> {b:?}");
            let equal = by_limbs == std::cmp::Ordering::Equal;
            assert_eq!(a == b, equal, "{a:?} == {b:?}");
            if equal {
                assert_eq!(hash_of(a), hash_of(b), "{a:?} hash {b:?}");
            }

            if b.is_zero() {
                continue;
            }
            let remainder = a.rem(b).unwrap();
            assert!(identical(&remainder, &a.rem_limbs(b)), "{a:?} % {b:?}");
            assert_canonical(&remainder);
            // Division at the scale `/` selects, and at shifts on both sides of it — a negative
            // shift moves to the divisor.
            let rscale = select_div_scale(a, b);
            let e = rscale as i64 + b.scale as i64 - a.scale as i64;
            for e in [e, e - 3, 0, 1, 19, 38, 39, -1, -19, -40] {
                let quotient = a.div_at(b, e, rscale);
                assert!(
                    identical(&quotient, &a.div_at_limbs(b, e, rscale)),
                    "{a:?} / {b:?} e={e}"
                );
                assert_canonical(&quotient);
            }
        }
    }
}

fn decimal_math_step(
    m: Option<&mut Meter>,
    scale: i64,
    sqrt: bool,
    args: &[&Decimal],
) -> Result<()> {
    let Some(m) = m else {
        return Ok(());
    };
    let digits = 32
        + scale.abs()
        + args
            .iter()
            .map(|a| a.precision() as i64 + a.scale as i64)
            .sum::<i64>();
    let groups = (digits + 3) / 4;
    let mut work = groups * groups;
    if sqrt {
        work *= (64 - (digits as u64).leading_zeros()) as i64;
    }
    m.charge(COSTS.decimal_transcend * work);
    m.checkpoint()?;
    m.reserve_scalar(8 * digits)
}

impl Decimal {
    pub fn dec_sqrt(&self) -> Result<Decimal> {
        self.dec_sqrt_metered(None)
    }
    pub fn dec_exp(&self) -> Result<Decimal> {
        self.dec_exp_metered(None)
    }
    pub fn dec_ln(&self) -> Result<Decimal> {
        self.dec_ln_metered(None)
    }
    pub fn dec_log10(&self) -> Result<Decimal> {
        self.dec_log10_metered(None)
    }
    pub fn dec_log(base: &Decimal, num: &Decimal) -> Result<Decimal> {
        Self::dec_log_metered(base, num, None)
    }
    pub fn dec_power(base: &Decimal, exp: &Decimal) -> Result<Decimal> {
        Self::dec_power_metered(base, exp, None)
    }
}

/// PostgreSQL's shortest decimal of the finite, nonzero float `m·2^e` (spec/design/float.md §9)
/// by exact decimal arithmetic over its rounding interval: for each length n from `min_len`, the
/// n-digit decimals just below and just above the value, kept if strictly inside the interval; the
/// first length with one wins, the nearer of two, a tie to the even last digit. `lower_tight` says
/// the interval's lower half is a quarter ulp (m is a power of two above the lowest binade).
/// `min_len` is at most the answer's length. Returns the significant digits (no trailing zeros) and
/// the decimal exponent of the first digit.
pub(crate) fn float_shortest_exact(
    m: u64,
    e: i32,
    lower_tight: bool,
    min_len: usize,
) -> (String, i32) {
    let lower = if lower_tight {
        Decimal::exact_dyadic(4 * m - 1, e - 2)
    } else {
        Decimal::exact_dyadic(2 * m - 1, e - 1)
    };
    let upper = Decimal::exact_dyadic(2 * m + 1, e - 1);
    let tz = m.trailing_zeros();
    let (_, x_limbs, x_scale) = Decimal::exact_dyadic(m >> tz, e + tz as i32).canonical();
    let xd = mag_to_decimal_str(&x_limbs);
    // The decimal exponent of the value's first digit.
    let top = xd.len() as i32 - 1 - x_scale as i32;
    let mut n = min_len;
    loop {
        // The decimal exponent of an n-digit candidate's last digit.
        let q = top - n as i32 + 1;
        let (lo, rest) = if xd.len() > n {
            (xd[..n].to_string(), &xd[n..])
        } else {
            (xd.clone() + &"0".repeat(n - xd.len()), "")
        };
        let hi = increment_digits(&lo);
        let lo_in = Decimal::times_pow10(&lo, q).cmp_value(&lower).is_gt();
        let hi_in = Decimal::times_pow10(&hi, q).cmp_value(&upper).is_lt();
        let pick = match (lo_in, hi_in) {
            (true, true) => {
                // The digits after the n-th against one half of the n-th digit's unit.
                let r = rest.trim_end_matches('0');
                let lo_even = (lo.as_bytes()[n - 1] - b'0').is_multiple_of(2);
                if r.is_empty() || r.as_bytes()[0] < b'5' || (r == "5" && lo_even) {
                    lo
                } else {
                    hi
                }
            }
            (true, false) => lo,
            (false, true) => hi,
            (false, false) => {
                n += 1;
                continue;
            }
        };
        let exp = q + pick.len() as i32 - 1;
        return (pick.trim_end_matches('0').to_string(), exp);
    }
}

/// One added to a decimal digit string, growing it by a digit on a carry out.
fn increment_digits(digits: &str) -> String {
    let mut b = digits.as_bytes().to_vec();
    for i in (0..b.len()).rev() {
        if b[i] < b'9' {
            b[i] += 1;
            return String::from_utf8(b).expect("ASCII digits");
        }
        b[i] = b'0';
    }
    b.insert(0, b'1');
    String::from_utf8(b).expect("ASCII digits")
}

/// PostgreSQL's `numeric_stddev_internal` (spec/design/aggregates.md §23) over the exact moments of
/// `n` non-NULL inputs (`sx` = Σx, `sxx` = Σx²): `None` over no input (or one, for a sample); else
/// (N·Σx² − (Σx)²) / (N·N) for the population or / (N·(N−1)) for a sample, divided at
/// `select_div_scale` and rounded half away from zero, then (for a standard deviation) its square
/// root at the same scale. A numerator that is not positive yields 0. Traps 22003 past the cap.
pub(crate) fn moment_variance(
    n: i64,
    sx: &Decimal,
    sxx: &Decimal,
    sample: bool,
    stddev: bool,
) -> Result<Option<Decimal>> {
    if n == 0 || (sample && n <= 1) {
        return Ok(None);
    }
    let count = Decimal::from_i64(n);
    let rscale = sx.scale as i64 * 2;
    let numerator = count
        .mul_var(sxx, rscale)
        .add_uncapped(&sx.mul_var(sx, rscale).neg());
    if numerator.is_negative() || numerator.is_zero() {
        return Ok(Some(Decimal::from_i64(0)));
    }
    let denominator = count.mul_exact(&if sample {
        Decimal::from_i64(n - 1)
    } else {
        count.clone()
    });
    let scale = select_div_scale(&numerator, &denominator) as i64;
    let mut result = numerator.div_var(&denominator, scale)?;
    if stddev {
        result = result.sqrt_var(scale, None)?;
    }
    Ok(Some(result.check_cap()?))
}
