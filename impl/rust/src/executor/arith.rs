//! Integer and binary-float arithmetic kernels: checked integer arithmetic (trap on overflow,
//! types.md), the `f32`/`f64` operators with PostgreSQL's overflow/underflow errors, the
//! int/decimal/float conversions, and the float scalar functions (float.md §8). Pure
//! value-in/value-out functions.

use super::*;

/// Evaluate an integer arithmetic op in 64-bit, trapping 22012 on a zero divisor and
/// 22003 if the 64-bit op overflows OR the in-range result falls outside the declared
/// result type (the i16+i16 → i16 boundary — spec/design/functions.md §7).
pub(crate) fn eval_arith(op: ArithOp, x: i64, y: i64, result: ScalarType) -> Result<Value> {
    let computed = match op {
        ArithOp::Add => x.checked_add(y),
        ArithOp::Sub => x.checked_sub(y),
        ArithOp::Mul => x.checked_mul(y),
        ArithOp::Div => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            x.checked_div(y)
        }
        ArithOp::Mod => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            // `x % -1` is mathematically 0 for every x. Special-cased so i64::MIN % -1
            // returns 0 instead of trapping on the i64 IDIV overflow (which `checked_rem`
            // reports as None) — matching PostgreSQL and the i16/i32 widths, which
            // already compute 0 cleanly in 64-bit (spec/design/types.md §3).
            if y == -1 { Some(0) } else { x.checked_rem(y) }
        }
    };
    let v = computed.ok_or_else(|| overflow(result))?;
    if result.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(result))
    }
}

/// Evaluate `f64 ⊕ f64` for one node (spec/design/float.md §5): the IEEE correctly-rounded
/// op (round-ties-to-even — Rust's default). The PG TRAP model: a FINITE pair whose result
/// overflows to ±Inf traps 22003 (finite arithmetic never PRODUCES non-finite values); `x / 0`
/// (or `x % 0`) traps 22012 for EVERY numerator except NaN (`Inf/0` and `0/0` trap; only `NaN/0`
/// propagates to NaN — matching PG). An operand already Inf/NaN otherwise PROPAGATES (no trap).
pub(crate) fn eval_float64_arith(op: ArithOp, x: f64, y: f64) -> Result<Value> {
    // Division/modulus by a zero divisor traps 22012 for every numerator EXCEPT NaN, which
    // propagates (NaN/0 = NaN, matching PG). `Inf/0` and `0/0` are genuine division by zero.
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y, // IEEE fmod (Rust `%` on f64 is fmod)
    };
    // Finite-overflow trap (§3): a result that became ±Inf from two FINITE operands overflowed.
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float64));
    }
    // Underflow (PG float8_mul / float8_div, float.md §3): a product of two non-zero operands, or
    // a quotient of a non-zero dividend by a finite divisor, that rounds to exactly zero traps
    // 22003 rather than yielding 0. `+`, `-` and `%` never underflow.
    if r == 0.0 && float_underflowed(op, x, y) {
        return Err(float_underflow());
    }
    Ok(Value::Float64(r))
}

/// Is a zero result of `op` PG's underflow: `x * y` with both operands non-zero, or `x / y` with a
/// non-zero dividend and a finite divisor (`1 / Infinity` is a legitimate 0)? Shared by the f64
/// and f32 kernels.
fn float_underflowed(op: ArithOp, x: f64, y: f64) -> bool {
    match op {
        ArithOp::Mul => x != 0.0 && y != 0.0,
        ArithOp::Div => x != 0.0 && !y.is_infinite(),
        _ => false,
    }
}

/// The 22003 a float operation traps when a non-zero exact result rounds to zero (PG's
/// `float_underflow_error` — float.md §3).
pub(crate) fn float_underflow() -> EngineError {
    EngineError::new(
        SqlState::NumericValueOutOfRange,
        "value out of range: underflow",
    )
}

/// The 22003 PG raises for a function input outside its domain (asin/acos/acosh/atanh out of
/// range, sin/cos/tan/cot of ±Infinity).
fn float_input_out_of_range() -> EngineError {
    EngineError::new(SqlState::NumericValueOutOfRange, "input is out of range")
}

/// PostgreSQL's `dpow` (float.c) over binary64 (float.md §8): the POSIX NaN rules first (NaN^0 =
/// 1, 1^NaN = 1, every other NaN input yields NaN), then the SQL-standard domain errors 2201F
/// (0^negative; negative^non-integer), then the infinite cases resolved explicitly per POSIX
/// (never trapped), and finally the libm pow with the finite overflow (±Inf → 22003) and underflow
/// (a non-zero base whose power rounds to 0 → 22003) traps.
fn float_pow(x: f64, y: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(if y.is_nan() || y != 0.0 {
            f64::NAN
        } else {
            1.0
        });
    }
    if y.is_nan() {
        return Ok(if x != 1.0 { f64::NAN } else { 1.0 });
    }
    if x == 0.0 && y < 0.0 {
        return Err(EngineError::new(
            SqlState::InvalidArgumentForPowerFunction,
            "zero raised to a negative power is undefined",
        ));
    }
    if x < 0.0 && y.floor() != y {
        return Err(EngineError::new(
            SqlState::InvalidArgumentForPowerFunction,
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    if y.is_infinite() {
        let absx = x.abs();
        return Ok(if absx == 1.0 {
            1.0
        } else if y > 0.0 {
            if absx > 1.0 { y } else { 0.0 }
        } else if absx > 1.0 {
            0.0
        } else {
            -y
        });
    }
    if x.is_infinite() {
        return Ok(if y == 0.0 {
            1.0
        } else if x > 0.0 {
            if y > 0.0 { x } else { 0.0 }
        } else {
            // x = -Inf: y is an integer (the domain check above); the sign follows its parity.
            let y_odd = (y / 2.0).floor() != y / 2.0;
            if y > 0.0 {
                if y_odd { x } else { -x }
            } else if y_odd {
                -0.0
            } else {
                0.0
            }
        });
    }
    let r = x.powf(y);
    if r.is_nan() {
        // Every domain error was handled above, so a NaN can only be a libm quirk for a huge |y|
        // (PG's glibc-bug branch): resolve it as PG does.
        let absx = x.abs();
        return if x == 0.0 {
            Ok(0.0)
        } else if absx == 1.0 {
            Ok(1.0)
        } else if (y >= 0.0) == (absx > 1.0) {
            Err(overflow(ScalarType::Float64))
        } else {
            Err(float_underflow())
        };
    }
    if r.is_infinite() {
        return Err(overflow(ScalarType::Float64));
    }
    if r == 0.0 && x != 0.0 {
        return Err(float_underflow());
    }
    Ok(r)
}

/// As [`eval_float64_arith`], at binary32 (`f32`). Each op rounds to binary32 (native `f32`
/// arithmetic), so a finite overflow to ±Inf at the f32 range traps 22003.
pub(crate) fn eval_float32_arith(op: ArithOp, x: f32, y: f32) -> Result<Value> {
    // Same zero-divisor rule as f64: traps for every numerator except NaN (Inf/0 traps).
    if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0.0 && !x.is_nan() {
        return Err(EngineError::new(
            SqlState::DivisionByZero,
            "division by zero",
        ));
    }
    let r = match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y,
    };
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(overflow(ScalarType::Float32));
    }
    if r == 0.0 && float_underflowed(op, x as f64, y as f64) {
        return Err(float_underflow()); // the binary32 underflow (PG float4_mul / float4_div)
    }
    Ok(Value::Float32(r))
}

/// Cast an integer to a float of `target` width (spec/design/float.md §6): nearest, round-ties-to-
/// even (Rust `as`), never overflows (i64::MAX < f32::MAX). `target` is a float type.
pub(crate) fn int_to_float(n: i64, target: ScalarType) -> Value {
    if target.is_float32() {
        Value::Float32(n as f32)
    } else {
        Value::Float64(n as f64)
    }
}

/// An integer literal adapted to a float context as a constant `RExpr` (spec/design/float.md §4),
/// at the context width.
pub(crate) fn int_to_const_float(n: i64, target: ScalarType) -> RExpr {
    if target.is_float32() {
        RExpr::ConstFloat32(n as f32)
    } else {
        RExpr::ConstFloat64(n as f64)
    }
}

/// Cast a decimal to a float of `target` width (spec/design/float.md §6): the nearest binary value
/// to the decimal's exact value. A magnitude that overflows the float range traps 22003 (not ±Inf
/// — the §3 finite rule; decimal is always finite, so the result can only be finite or trap).
pub(crate) fn decimal_to_float(d: &Decimal, target: ScalarType) -> Result<Value> {
    // The decimal's canonical string parses to the nearest binary value (Rust's float parser is
    // correctly rounded). A huge decimal parses to ±Inf, which is the overflow case.
    // A non-zero decimal that parses to 0 underflowed the width: 22003 (PG numeric → float8/float4,
    // float.md §6).
    let s = d.render();
    if target.is_float32() {
        let f: f32 = s.parse().map_err(|_| overflow(ScalarType::Float32))?;
        if !f.is_finite() {
            return Err(overflow(ScalarType::Float32));
        }
        if f == 0.0 && !d.is_zero() {
            return Err(float_underflow());
        }
        Ok(Value::Float32(f))
    } else {
        let f: f64 = s.parse().map_err(|_| overflow(ScalarType::Float64))?;
        if !f.is_finite() {
            return Err(overflow(ScalarType::Float64));
        }
        if f == 0.0 && !d.is_zero() {
            return Err(float_underflow());
        }
        Ok(Value::Float64(f))
    }
}

/// Cast a float value (already widened to f64) to a non-float `target` — int / decimal — or to a
/// narrower float width (spec/design/float.md §6). NaN/±Inf → 22003 for every non-float target
/// (and for f64 → f32 overflow). Float → int rounds HALF AWAY FROM ZERO (jed's one mode)
/// then range-checks. Float → decimal is the exact decimal of the binary value, then the typmod.
pub(crate) fn cast_from_float(
    f: f64,
    target: ScalarType,
    typmod: Option<DecimalTypmod>,
) -> Result<Value> {
    if target.is_float64() {
        // float → f64: widening (lossless from f32, identity from f64).
        return Ok(Value::Float64(f));
    }
    if target.is_float32() {
        // f64 → f32: nearest (round-ties-to-even). A finite value beyond the binary32
        // range traps 22003 (not ±Inf); NaN/±Inf convert across widths unchanged (propagate).
        let n = f as f32;
        if n.is_infinite() && f.is_finite() {
            return Err(overflow(ScalarType::Float32));
        }
        if n == 0.0 && f != 0.0 {
            return Err(float_underflow()); // a non-zero f64 that rounds to 0 at binary32 (PG float84)
        }
        return Ok(Value::Float32(n));
    }
    // Non-float targets reject NaN/±Inf (they have no finite representation).
    if !f.is_finite() {
        return Err(overflow(target));
    }
    if target.is_decimal() {
        // float → decimal: the EXACT decimal of the binary value (spec/design/float.md §6), then
        // the typmod's scale coercion. `f` is finite (checked above); a f32 reaches here
        // already losslessly widened to f64, so the exact decimal IS the binary32 value's.
        let d = Decimal::from_float64(f);
        return Ok(Value::Decimal(coerce_decimal(d, typmod)?));
    }
    // float → int: round HALF AWAY FROM ZERO, then range-check the target integer (22003).
    let rounded = f.round(); // Rust `f64::round` is round-half-away-from-zero
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return Err(overflow(target));
    }
    let v = rounded as i64;
    if target.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(target))
    }
}

/// Finalize a float SUM/AVG from the STREAMING scan-order running total (spec/design/float.md §7).
/// The fold order is ledgered non-deterministic (`float-sum-order`); the steps here are all
/// order-independent given the accumulated `total`:
/// 1. Special values FIRST: empty/all-NULL group → NULL; any NaN → NaN; both +Inf and -Inf → NaN;
///    else +Inf → +Inf; else -Inf → -Inf; else all-finite → step 2.
/// 2. The running `total` IS the sum (already width-correct — f32 was re-rounded each add). If it
///    reached ±Inf from finite inputs it is a finite-overflow 22003 (a final `is_infinite` test is
///    equivalent to a per-add one — finite + ±Inf cannot recover to finite).
///
/// AVG = SUM / count, ONE final rounding at the input width.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_float_fold(
    width: ScalarType,
    is_avg: bool,
    total: f64,
    count: i64,
    any_nan: bool,
    pos_inf: bool,
    neg_inf: bool,
) -> Result<Value> {
    let is_f32 = width.is_float32();
    let wrap = |f: f64| -> Value {
        if is_f32 {
            Value::Float32(f as f32)
        } else {
            Value::Float64(f)
        }
    };
    // Step 1 — empty group → NULL (no non-NULL inputs).
    if count == 0 {
        return Ok(Value::Null);
    }
    // Step 1 — special values, resolved before the finite sum (order-independent).
    if any_nan {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf && neg_inf {
        return Ok(wrap(f64::NAN));
    }
    if pos_inf {
        return Ok(wrap(f64::INFINITY));
    }
    if neg_inf {
        return Ok(wrap(f64::NEG_INFINITY));
    }
    // Step 2 — the running total is the sum; f32 already re-rounded each add so `total as f32` is
    // exact. A ±Inf total is a finite-overflow 22003 (the §3 finite-overflow rule).
    let sum = if is_f32 {
        let acc = total as f32;
        if acc.is_infinite() {
            return Err(overflow(ScalarType::Float32));
        }
        acc as f64
    } else {
        if total.is_infinite() {
            return Err(overflow(ScalarType::Float64));
        }
        total
    };
    if !is_avg {
        return Ok(wrap(sum));
    }
    // AVG = SUM / count, one rounding at the input width.
    if is_f32 {
        let avg = (sum as f32) / (count as f32);
        Ok(Value::Float32(avg))
    } else {
        Ok(Value::Float64(sum / count as f64))
    }
}

/// `round(f64, places)` — round half away from zero to `places` decimal digits (the engine's
/// one mode — spec/design/float.md §8). A NaN/±Inf operand passes through. `places` may be
/// negative (round to the left of the point). Done by scaling by 10^places, `round()` (half-away),
/// then unscaling — the approximate float path (binary, so itself inexact, which the `R` tag
/// absorbs).
pub(crate) fn round_f64_places(f: f64, places: i64) -> f64 {
    if !f.is_finite() {
        return f;
    }
    if places == 0 {
        return f.round();
    }
    if places > 308 {
        if places >= 324 {
            return f;
        }
        // Decimal places beyond the finite power-of-ten range can still round subnormals.
        // The exact terminating decimal keeps this bounded fallback free of scaling overflow.
        let rounded = Decimal::from_float64(f).round_to_scale(places as u32);
        return rounded
            .render()
            .parse::<f64>()
            .expect("finite rounded float")
            .copysign(f);
    }
    if places < -323 {
        return 0.0f64.copysign(f);
    }
    let scale = 10f64.powi(places as i32);
    if scale == 0.0 {
        return 0.0f64.copysign(f);
    }
    let scaled = f * scale;
    if !scale.is_finite() || !scaled.is_finite() {
        return f;
    }
    scaled.round() / scale
}

/// Evaluate a float scalar function over a finite/non-finite f64 (spec/design/float.md §8). The
/// EXACT set (ceil/floor/trunc/sqrt) is correctly-rounded (in-contract); the TRANSCENDENTAL set
/// (exp/ln/log10/pow/sin/cos/tan) calls Rust's libm (exempted, may differ by an ULP cross-core).
/// Domain errors follow PG: ln/log10 of zero or a negative → 2201E, sqrt of a negative and pow's
/// 0^negative / negative^non-integer → 2201F, exp/pow overflow and underflow → 22003, sin/cos/
/// tan/cot of ±Infinity → 22003 — keeping NaN/Inf input-only (a NaN/Inf *operand* propagates).
pub(crate) fn eval_float_func(func: ScalarFunc, x: f64, arg2: Option<&Value>) -> Result<Value> {
    // PG's exact RADIANS_PER_DEGREE literal (float.c) — shared by radians/degrees so the single
    // IEEE multiply/divide is byte-identical cross-core and matches PG.
    const RADIANS_PER_DEGREE: f64 = 0.0174532925199432957692;
    let r = match func {
        ScalarFunc::Ceil => x.ceil(),
        ScalarFunc::Floor => x.floor(),
        ScalarFunc::Trunc => x.trunc(),
        ScalarFunc::Sqrt => {
            // sqrt of a NEGATIVE value is a domain error → 2201F (PG dsqrt; NaN stays input-only).
            // A NaN/±Inf operand propagates (sqrt(Inf) = Inf, sqrt(NaN) = NaN); -Inf is negative.
            if x < 0.0 {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForPowerFunction,
                    "cannot take square root of a negative number",
                ));
            }
            x.sqrt()
        }
        ScalarFunc::Exp => {
            let v = x.exp();
            if x.is_finite() {
                // exp overflow (e.g. exp(710)) → ±Inf from a finite operand traps 22003; an exact
                // result that rounds to 0 (exp(-746)) is the underflow trap (PG dexp).
                if v.is_infinite() {
                    return Err(overflow(ScalarType::Float64));
                }
                if v == 0.0 {
                    return Err(float_underflow());
                }
            }
            v
        }
        ScalarFunc::Ln => {
            // ln(0) → 2201E; ln(neg) → 2201E (PG dlog1). NaN/+Inf operands propagate.
            if x == 0.0 {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForLog,
                    "cannot take logarithm of zero",
                ));
            }
            if x < 0.0 {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForLog,
                    "cannot take logarithm of a negative number",
                ));
            }
            x.ln()
        }
        ScalarFunc::Log10 => {
            if x == 0.0 {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForLog,
                    "cannot take logarithm of zero",
                ));
            }
            if x < 0.0 {
                return Err(EngineError::new(
                    SqlState::InvalidArgumentForLog,
                    "cannot take logarithm of a negative number",
                ));
            }
            x.log10()
        }
        ScalarFunc::Pow => {
            let y = match arg2 {
                Some(Value::Float64(y)) => *y,
                _ => unreachable!("pow's second arg is a widened f64"),
            };
            float_pow(x, y)?
        }
        // The circular functions are undefined at ±Infinity: PG raises 22003 "input is out of
        // range" (dsin/dcos/dtan/dcot) rather than returning NaN; a NaN operand propagates.
        ScalarFunc::Sin | ScalarFunc::Cos | ScalarFunc::Tan | ScalarFunc::Cot => {
            if x.is_infinite() {
                return Err(float_input_out_of_range());
            }
            match func {
                ScalarFunc::Sin => x.sin(),
                ScalarFunc::Cos => x.cos(),
                ScalarFunc::Tan => x.tan(),
                // cot(x) = 1/tan(x) (no libm cot; 1/tan bit-matches PG). cot(0) = +Inf (no trap).
                _ => 1.0 / x.tan(),
            }
        }
        // cbrt has no domain restriction: cbrt(-8) = -2, cbrt(±Inf) = ±Inf, cbrt(NaN) = NaN.
        ScalarFunc::Cbrt => x.cbrt(),
        // radians/degrees — a single correctly-rounded IEEE op (multiply/divide) by PG's exact
        // RADIANS_PER_DEGREE literal (float.c), so byte-identical cross-core (in-contract). They
        // are PG's float8_mul / float8_div, so they carry the §3/§5 overflow and underflow traps:
        // degrees(1e308) overflows, radians(5e-324) underflows to 0 — both 22003.
        ScalarFunc::Radians => match eval_float64_arith(ArithOp::Mul, x, RADIANS_PER_DEGREE)? {
            Value::Float64(v) => v,
            _ => unreachable!("eval_float64_arith yields a Float64"),
        },
        ScalarFunc::Degrees => match eval_float64_arith(ArithOp::Div, x, RADIANS_PER_DEGREE)? {
            Value::Float64(v) => v,
            _ => unreachable!("eval_float64_arith yields a Float64"),
        },
        // asin domain is [-1, 1]: a finite |x| > 1 (and ±Inf, magnitude > 1) is out of range →
        // 22003, exactly PG; a NaN operand propagates (no trap).
        ScalarFunc::Asin => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.asin()
        }
        // acos shares asin's domain [-1, 1]: |x| > 1 (or ±Inf) → 22003, NaN propagates.
        ScalarFunc::Acos => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.acos()
        }
        // atan is defined on all of ℝ (no domain trap); atan(±Inf) = ±π/2, atan(NaN) = NaN.
        ScalarFunc::Atan => x.atan(),
        // atan2(y, x): y is the first operand (x here), the second (arg2) is the denominator.
        // Quadrant-aware; no domain trap. The resolver widened both operands to f64.
        ScalarFunc::Atan2 => {
            let x2 = match arg2 {
                Some(Value::Float64(v)) => *v,
                _ => unreachable!("atan2's second arg is a widened f64"),
            };
            x.atan2(x2)
        }
        // Hyperbolics: sinh/cosh overflow to ±Inf with NO trap (PG-faithful, unlike exp/pow);
        // tanh/asinh are total. acosh traps below 1; atanh traps outside [-1, 1] (atanh(±1) =
        // ±Inf is admissible). A NaN operand propagates through every one.
        ScalarFunc::Sinh => x.sinh(),
        ScalarFunc::Cosh => x.cosh(),
        ScalarFunc::Tanh => x.tanh(),
        ScalarFunc::Asinh => x.asinh(),
        ScalarFunc::Acosh => {
            if !x.is_nan() && x < 1.0 {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.acosh()
        }
        ScalarFunc::Atanh => {
            if !x.is_nan() && (x < -1.0 || x > 1.0) {
                return Err(EngineError::new(
                    SqlState::NumericValueOutOfRange,
                    "input is out of range",
                ));
            }
            x.atanh()
        }
        ScalarFunc::Abs
        | ScalarFunc::Round
        | ScalarFunc::Pi
        | ScalarFunc::Log
        | ScalarFunc::Sign
        | ScalarFunc::Div
        | ScalarFunc::Gcd
        | ScalarFunc::Lcm
        | ScalarFunc::Factorial
        | ScalarFunc::WidthBucket
        | ScalarFunc::Scale
        | ScalarFunc::MinScale
        | ScalarFunc::TrimScale
        | ScalarFunc::MakeInterval
        | ScalarFunc::MakeTimestamp
        | ScalarFunc::MakeTimestamptz
        | ScalarFunc::MakeDate
        | ScalarFunc::CurrentDate
        | ScalarFunc::DatePart
        | ScalarFunc::ToChar
        | ScalarFunc::ToCharInt8
        | ScalarFunc::ToTimestamp
        | ScalarFunc::ToDate
        | ScalarFunc::ToNumber
        | ScalarFunc::JustifyHours
        | ScalarFunc::JustifyDays
        | ScalarFunc::JustifyInterval
        | ScalarFunc::Age
        | ScalarFunc::UuidExtractVersion
        | ScalarFunc::UuidExtractTimestamp
        | ScalarFunc::Uuidv4
        | ScalarFunc::Uuidv7
        | ScalarFunc::Now
        | ScalarFunc::ClockTimestamp
        | ScalarFunc::Nextval
        | ScalarFunc::Currval
        | ScalarFunc::Setval
        | ScalarFunc::Lastval
        | ScalarFunc::CurrentSetting
        | ScalarFunc::JsonbTypeof
        | ScalarFunc::JsonTypeof
        | ScalarFunc::JsonbArrayLength
        | ScalarFunc::JsonArrayLength
        | ScalarFunc::JsonbStripNulls
        | ScalarFunc::JsonStripNulls
        | ScalarFunc::JsonbPretty
        | ScalarFunc::JsonSerialize
        | ScalarFunc::Length
        | ScalarFunc::OctetLength
        | ScalarFunc::BitLength
        | ScalarFunc::Substr
        | ScalarFunc::Left
        | ScalarFunc::Right
        | ScalarFunc::Lpad
        | ScalarFunc::Rpad
        | ScalarFunc::Btrim
        | ScalarFunc::Ltrim
        | ScalarFunc::Rtrim
        | ScalarFunc::Replace
        | ScalarFunc::Translate
        | ScalarFunc::Repeat
        | ScalarFunc::Reverse
        | ScalarFunc::Strpos
        | ScalarFunc::SplitPart
        | ScalarFunc::StartsWith
        | ScalarFunc::Ascii
        | ScalarFunc::Chr
        | ScalarFunc::Initcap
        | ScalarFunc::ToHex
        | ScalarFunc::Encode
        | ScalarFunc::GetByte
        | ScalarFunc::SetByte
        | ScalarFunc::GetBit
        | ScalarFunc::SetBit
        | ScalarFunc::Md5
        | ScalarFunc::Sha224
        | ScalarFunc::Sha256
        | ScalarFunc::Sha384
        | ScalarFunc::Sha512
        | ScalarFunc::Overlay
        | ScalarFunc::ByteaConcat
        | ScalarFunc::TextConcat
        | ScalarFunc::Decode
        | ScalarFunc::QuoteLiteral
        | ScalarFunc::QuoteIdent
        | ScalarFunc::QuoteNullable
        | ScalarFunc::LikeEscape
        | ScalarFunc::SimilarToEscape => {
            unreachable!(
                "abs/round/make_interval/uuid_*/now/clock_timestamp/sequence/current_setting/json/string fns are handled before eval_float_func"
            )
        }
    };
    Ok(Value::Float64(r))
}
