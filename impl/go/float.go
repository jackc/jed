package jed

// IEEE 754 binary floating point — float32 / float64 (spec/design/float.md). The engine's
// APPROXIMATE numeric, the deliberate opposite of decimal: inexact, base-2, and admitting
// NaN/±Infinity. It is the FIRST type partially exempted from cross-core byte-identity
// (determinism.md §6), but the exemption is NARROW — storage, the total order, the
// +-*/ /sqrt kernel, the canonical-fold SUM/AVG, MIN/MAX/COUNT, and cost all stay fully
// deterministic and cross-core. Only TRANSCENDENTAL function values and text-render LAYOUT
// are exempt (absorbed by the conformance `R` tag). This file holds the float-specific
// value logic: the total order, the kernel, the casts, and the canonical-order fold.

import (
	"math"
	"sort"
	"strconv"
)

// --- the total order (spec/design/float.md §3) --------------------------------------------
//
// IEEE comparison is a PARTIAL order (NaN unordered; -0 == +0). SQL needs a TOTAL order for
// ORDER BY / DISTINCT / GROUP BY / MIN / MAX / =. jed adopts PostgreSQL's float8 btree order:
//
//	-Infinity  <  (finite, numerically)  <  +Infinity  <  NaN
//
// with -0 = +0 and NaN = NaN (all NaN bit patterns are ONE equivalence class). So NaN is the
// single largest value and `NaN = NaN` is TRUE. A documented divergence from raw IEEE.

// floatTotalRank maps a float64 to a totally-ordered class rank: every NaN → the largest class,
// everything else compares numerically with -0 folded to +0. Used only as a tie-break gate.
//
// floatTotalCmp is the total-order comparison of two float64 values (the §3 order), returning
// <0, 0, >0. NaN is the largest (NaN vs NaN is 0; NaN vs anything finite/Inf is +1). -0 and +0
// compare equal because Go's < / > already treat them equal and 0 == 0. Mixed widths reach
// here already widened to float64 (lossless — §2).
func floatTotalCmp(a, b float64) int {
	aNaN, bNaN := math.IsNaN(a), math.IsNaN(b)
	switch {
	case aNaN && bNaN:
		return 0 // all NaNs are one equivalence class
	case aNaN:
		return 1 // NaN is the largest value
	case bNaN:
		return -1
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		// a == b numerically — this folds -0 == +0 (IEEE equality), which is exactly the §3 rule.
		return 0
	}
}

// canonicalizeFloat64 maps -0.0 → +0.0 (leaving every other value, incl NaN/±Inf, unchanged) —
// the canonical form for keys / dedup / the SUM fold (spec/design/float.md §3/§7). NaN patterns
// are not folded here (the fold extracts NaN before sorting; comparison uses floatTotalCmp).
func canonicalizeFloat64(f float64) float64 {
	if f == 0 {
		return 0 // both -0 and +0 satisfy f==0; return the literal +0.0
	}
	return f
}

// --- rendering (spec/design/float.md §9) --------------------------------------------------
//
// Each core uses its NATIVE shortest-round-trip formatter; the `R` tag absorbs layout
// differences. Go: strconv.FormatFloat('g', -1, width). Special values render PG-style —
// `Infinity` / `-Infinity` / `NaN` (Go prints `+Inf`/`-Inf`/`NaN`), and -0 renders `-0`.

// renderFloat64 formats a float64 as its shortest round-trip decimal, mapping Go's special-value
// spellings to the spec's PG spellings (spec/design/float.md §9).
func renderFloat64(f float64) string {
	if math.IsNaN(f) {
		return "NaN"
	}
	if math.IsInf(f, 1) {
		return "Infinity"
	}
	if math.IsInf(f, -1) {
		return "-Infinity"
	}
	if f == 0 && math.Signbit(f) {
		return "-0" // negative zero renders with its sign (PG)
	}
	return strconv.FormatFloat(f, 'g', -1, 64)
}

// renderFloat32 is renderFloat64 at binary32 width (shortest round-trip for the 32-bit value).
func renderFloat32(f float32) string {
	d := float64(f)
	if math.IsNaN(d) {
		return "NaN"
	}
	if math.IsInf(d, 1) {
		return "Infinity"
	}
	if math.IsInf(d, -1) {
		return "-Infinity"
	}
	if d == 0 && math.Signbit(d) {
		return "-0"
	}
	return strconv.FormatFloat(d, 'g', -1, 32)
}

// --- the arithmetic kernel (spec/design/float.md §5) --------------------------------------
//
// float ⊕ float → float for + - * / % and unary -. Each is the IEEE 754 correctly-rounded
// operation (round-ties-to-even), ONE operator per node (the tree-walk guarantees no FMA
// contraction — float.md §5). FINITE arithmetic NEVER produces Inf/NaN: a finite result that
// overflows traps 22003; x/0 traps 22012. An operand that is ALREADY Inf/NaN propagates per
// IEEE (no trap). Mixed widths promote to float64 first (so the result kind is float64).

// evalFloatArith evaluates one float arithmetic op. a and b are the operand VALUES (each
// ValFloat32 or ValFloat64). resultIs32 says the static result type is float32 (both operands
// float32); otherwise float64 (either operand float64 — the promotion). Returns a float Value
// of the result width, or a trap (22003 finite-overflow, 22012 division by zero).
func evalFloatArith(op BinaryOp, a, b Value, resultIs32 bool) (Value, error) {
	if resultIs32 {
		x, y := a.F32(), b.F32()
		r, err := float32Op(op, x, y)
		if err != nil {
			return Value{}, err
		}
		return Float32Value(r), nil
	}
	x, y := a.asF64(), b.asF64()
	r, err := float64Op(op, x, y)
	if err != nil {
		return Value{}, err
	}
	return Float64Value(r), nil
}

// float64Op applies one IEEE binary op at binary64 width with the trap model (§3/§5): a finite
// pair whose result overflows to ±Inf traps 22003; / or % by (finite) zero traps 22012; an
// Inf/NaN operand (or a NaN/Inf result from non-finite inputs) propagates without trapping.
func float64Op(op BinaryOp, x, y float64) (float64, error) {
	var r float64
	switch op {
	case OpAdd:
		r = x + y
	case OpSub:
		r = x - y
	case OpMul:
		r = x * y
	case OpDiv:
		// finite / 0 traps 22012 (PG), including 0/0 and (finite)/±0. Inf/0 and NaN/0 propagate.
		if y == 0 && isFinite(x) {
			return 0, NewError(DivisionByZero, "division by zero")
		}
		r = x / y
	default: // OpMod — IEEE fmod
		if y == 0 && !math.IsNaN(x) && !math.IsInf(x, 0) {
			return 0, NewError(DivisionByZero, "division by zero")
		}
		r = math.Mod(x, y)
	}
	// Finite operands that produced a non-finite result = finite overflow → trap 22003. If an
	// operand was already non-finite, the result propagates (no trap).
	if (math.IsInf(r, 0) || math.IsNaN(r)) && isFinite(x) && isFinite(y) {
		return 0, overflowFloatErr()
	}
	return r, nil
}

// float32Op is float64Op at binary32 width — every op rounds to binary32 (Go float32 arithmetic),
// matching the §2/§5 "compute at the input width" rule. The finite-overflow check is against the
// binary32 range (a finite float32 pair whose true result exceeds float32 max → ±Inf → 22003).
func float32Op(op BinaryOp, x, y float32) (float32, error) {
	var r float32
	switch op {
	case OpAdd:
		r = x + y
	case OpSub:
		r = x - y
	case OpMul:
		r = x * y
	case OpDiv:
		if y == 0 && isFinite32(x) {
			return 0, NewError(DivisionByZero, "division by zero")
		}
		r = x / y
	default: // OpMod
		if y == 0 && isFinite32(x) {
			return 0, NewError(DivisionByZero, "division by zero")
		}
		r = float32(math.Mod(float64(x), float64(y)))
	}
	if (math.IsInf(float64(r), 0) || math.IsNaN(float64(r))) && isFinite32(x) && isFinite32(y) {
		return 0, overflowFloatErr()
	}
	return r, nil
}

// evalFloatNeg negates a float value (unary -) — pure IEEE sign flip, never traps (negation
// cannot overflow; -NaN is NaN, -(-0) = +0). Preserves the operand width.
func evalFloatNeg(v Value) Value {
	if v.Kind == ValFloat32 {
		return Float32Value(-v.F32())
	}
	return Float64Value(-v.F64())
}

func isFinite(f float64) bool   { return !math.IsInf(f, 0) && !math.IsNaN(f) }
func isFinite32(f float32) bool { return isFinite(float64(f)) }

// overflowFloatErr is the 22003 a finite float operation traps on overflow to ±Inf (§3).
func overflowFloatErr() error {
	return NewError(NumericValueOutOfRange, "value out of range: overflow")
}

// --- casts (spec/design/float.md §6, ../types/casts.toml) ---------------------------------

// intToFloat64 converts an integer (any width, carried in int64) to the nearest binary64,
// round-ties-to-even (the IEEE conversion; never traps — §6).
func intToFloat64(n int64) float64 { return float64(n) }

// intToFloat32 converts an integer to the nearest binary32, round-ties-to-even (never traps).
func intToFloat32(n int64) float32 { return float32(n) }

// floatToInt converts a float value to an integer of target type, rounding HALF AWAY FROM ZERO
// (jed's one rounding mode — a documented divergence from PG's half-to-even rint), then
// range-checking (22003). NaN / ±Inf → 22003 (no integer representation). spec/design/float.md §6.
func floatToInt(f float64, target ScalarType) (int64, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return 0, overflowErr(target)
	}
	r := math.Round(f) // Go's math.Round is round-half-AWAY-from-zero — exactly jed's mode
	// math.Round of a huge value stays huge; the int64 conversion below must be range-guarded
	// BEFORE the Go float→int conversion (which is undefined for out-of-int64-range values).
	if r >= 9223372036854775808.0 || r < -9223372036854775808.0 {
		return 0, overflowErr(target)
	}
	n := int64(r)
	if !target.InRange(n) {
		return 0, overflowErr(target)
	}
	return n, nil
}

// floatToDecimal converts a float value to the EXACT decimal of its binary64 value, then applies
// the target typmod's scale coercion (spec/design/float.md §6). NaN / ±Inf → 22003 (decimal is
// finite). The exact decimal is produced WITHOUT a bignum library (decimal's limb arithmetic),
// so it is byte-identical across cores. typmod nil = cap-check only.
func floatToDecimal(f float64, typmod *DecimalTypmod) (Value, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return Value{}, NewError(NumericValueOutOfRange, "cannot convert a non-finite float to decimal")
	}
	d := exactDecimalFromFloat64(f)
	if typmod != nil {
		var err error
		d, err = d.CoerceToTypmod(uint32(typmod.Precision), uint32(typmod.Scale))
		if err != nil {
			return Value{}, err
		}
	} else {
		var err error
		d, err = d.CheckCap()
		if err != nil {
			return Value{}, err
		}
	}
	return DecimalValue(d), nil
}

// exactDecimalFromFloat64 builds the EXACT base-10 decimal equal to a finite binary64. A binary64
// is mantissa·2^exp; for exp ≥ 0 the value is mantissa·2^exp (an integer, scale 0); for exp < 0
// it is mantissa·5^|exp| · 10^(-|exp|) (since 2^-k = 5^k·10^-k), an exact terminating decimal of
// scale |exp|. Computed with decimal's own limb multiply (magMulSmall by 2 or 5), so it is
// hand-rolled and cross-core identical (no math/big on the value path).
func exactDecimalFromFloat64(f float64) Decimal {
	if f == 0 {
		return DecimalZero(0) // +0 and -0 both → exact 0
	}
	bits := math.Float64bits(f)
	neg := bits>>63 != 0
	exp := int((bits >> 52) & 0x7ff)
	mant := bits & ((uint64(1) << 52) - 1)
	if exp == 0 {
		// Subnormal: no implicit leading 1; true exponent is -1074.
		exp = -1074
	} else {
		mant |= uint64(1) << 52 // implicit leading 1
		exp -= 1075             // bias 1023 + 52 mantissa bits
	}
	mag := magFromUint64(mant)
	if exp >= 0 {
		// value = mant · 2^exp (integer). Multiply by 2 exp times.
		for i := 0; i < exp; i++ {
			mag = magMulSmall(mag, 2)
		}
		return newDecimal(neg, 0, mag)
	}
	// value = mant · 5^|exp| with scale |exp| (since 2^exp = 5^|exp|/10^|exp|).
	k := -exp
	for i := 0; i < k; i++ {
		mag = magMulSmall(mag, 5)
	}
	// Normalize to the MINIMAL display scale (trim trailing decimal zeros): the value is unchanged
	// but the rendered form matches PG's float8->numeric (0.5, not 0.500…0). This is exact — only
	// zero digits are removed. Reuse the decimal module's canonical trim via a digit round-trip.
	d := newDecimal(neg, uint32(k), mag)
	dneg, digits, scale := d.canonical()
	return DecimalFromDigitsScale(dneg, digits, scale)
}

// decimalToFloat64 converts a decimal to the nearest binary64, round-ties-to-even via Go's
// correctly-rounded strconv.ParseFloat over the decimal's canonical string. A finite decimal
// whose magnitude overflows binary64 traps 22003 (the §3 finite-overflow rule — NOT ±Inf).
func decimalToFloat64(d Decimal) (float64, error) {
	f, err := strconv.ParseFloat(d.Render(), 64)
	if err != nil || math.IsInf(f, 0) {
		return 0, overflowFloatErr()
	}
	return f, nil
}

// decimalToFloat32 is decimalToFloat64 at binary32 width.
func decimalToFloat32(d Decimal) (float32, error) {
	f, err := strconv.ParseFloat(d.Render(), 32)
	if err != nil || math.IsInf(f, 0) {
		return 0, overflowFloatErr()
	}
	return float32(f), nil
}

// float64ToFloat32 narrows a float64 to float32, round-ties-to-even. A finite float64 beyond the
// binary32 range traps 22003 (the §3 finite-overflow rule). NaN/±Inf convert unchanged (no trap).
func float64ToFloat32(f float64) (float32, error) {
	r := float32(f)
	if math.IsInf(float64(r), 0) && isFinite(f) {
		return 0, overflowFloatErr()
	}
	return r, nil
}

// --- the order-independent canonical-order SUM/AVG fold (spec/design/float.md §7) ----------
//
// Naive float summation is non-associative → order-dependent → not cross-core deterministic.
// jed defines float SUM/AVG as a CANONICAL-ORDER fold: special values resolved first, then the
// finite inputs are -0-canonicalized, SORTED by the total order, and folded left with
// width-correct IEEE add. Identical regardless of row/partition order, bit-identical cross-core.

// floatSumAcc accumulates the inputs of a float SUM/AVG so they can be folded in canonical order
// at finalize. is32 selects the fold width (float32 vs float64). It records the special-value
// flags and collects the finite inputs (as float64 — float32 widens losslessly for sorting; the
// FOLD re-narrows per step when is32).
type floatSumAcc struct {
	is32      bool
	sawNaN    bool
	sawPosInf bool
	sawNegInf bool
	finite    []float64 // -0-canonicalized finite inputs (at f64; narrowed in the fold if is32)
	count     int64     // non-NULL input count (for AVG)
}

func newFloatSumAcc(is32 bool) *floatSumAcc { return &floatSumAcc{is32: is32} }

// add folds one non-NULL float input into the accumulator (NULLs are skipped by the caller).
func (a *floatSumAcc) add(v Value) {
	a.count++
	f := v.asF64()
	switch {
	case math.IsNaN(f):
		a.sawNaN = true
	case math.IsInf(f, 1):
		a.sawPosInf = true
	case math.IsInf(f, -1):
		a.sawNegInf = true
	default:
		a.finite = append(a.finite, canonicalizeFloat64(f))
	}
}

// sumF64 resolves the SUM as a float64 result (the special-value rules, then the canonical fold).
// ok=false means the group was empty (→ NULL). A running total that overflows to ±Inf traps 22003.
func (a *floatSumAcc) sumF64() (float64, bool, error) {
	if a.count == 0 {
		return 0, false, nil
	}
	if special, isSpecial := a.specialSum(); isSpecial {
		return special, true, nil
	}
	// Sort finite inputs by the total order, then fold left with f64 add (overflow → 22003).
	xs := append([]float64(nil), a.finite...)
	sort.Slice(xs, func(i, j int) bool { return floatTotalCmp(xs[i], xs[j]) < 0 })
	total := 0.0
	for _, x := range xs {
		total += x
		if math.IsInf(total, 0) {
			return 0, false, overflowFloatErr()
		}
	}
	return total, true, nil
}

// sumF32 resolves the SUM as a float32 result — the same canonical fold, but each add rounds to
// binary32 (the §7 width-correct fold for float32).
func (a *floatSumAcc) sumF32() (float32, bool, error) {
	if a.count == 0 {
		return 0, false, nil
	}
	if special, isSpecial := a.specialSum(); isSpecial {
		return float32(special), true, nil
	}
	xs := append([]float64(nil), a.finite...)
	sort.Slice(xs, func(i, j int) bool { return floatTotalCmp(xs[i], xs[j]) < 0 })
	var total float32 = 0
	for _, x := range xs {
		total += float32(x)
		if math.IsInf(float64(total), 0) {
			return 0, false, overflowFloatErr()
		}
	}
	return total, true, nil
}

// specialSum applies the §7 special-value precedence (NaN dominates; then both ±Inf → NaN; then
// +Inf; then -Inf). isSpecial=false means all inputs were finite (fall through to the fold).
func (a *floatSumAcc) specialSum() (float64, bool) {
	switch {
	case a.sawNaN:
		return math.NaN(), true
	case a.sawPosInf && a.sawNegInf:
		return math.NaN(), true
	case a.sawPosInf:
		return math.Inf(1), true
	case a.sawNegInf:
		return math.Inf(-1), true
	default:
		return 0, false
	}
}

// avgF64 resolves AVG as a float64: SUM / count, the division rounded once (empty → NULL). A NaN
// or ±Inf sum carries through the division (NaN/n = NaN, ±Inf/n = ±Inf).
func (a *floatSumAcc) avgF64() (float64, bool, error) {
	s, ok, err := a.sumF64()
	if err != nil || !ok {
		return 0, ok, err
	}
	return s / float64(a.count), true, nil
}

// avgF32 resolves AVG as a float32 (sum at f32 / count, one rounding).
func (a *floatSumAcc) avgF32() (float32, bool, error) {
	s, ok, err := a.sumF32()
	if err != nil || !ok {
		return 0, ok, err
	}
	return s / float32(a.count), true, nil
}

// --- the float scalar functions (spec/design/float.md §8) ---------------------------------
//
// EXACT / correctly-rounded (in-contract): abs, ceil, floor, trunc, round (half away, 1- & 2-arg),
// sqrt. TRANSCENDENTAL (exempted, native math pkg): exp, ln, log10, pow, sin, cos, tan. Result
// width is the call's `result` (Float32 only for abs over a float32 arg; float64 for the rest, per
// the catalog). Domain errors follow PG: ln(0) → 22003, sqrt/ln of a negative → 22003, exp/pow
// overflow → 22003 — keeping NaN an INPUT-ONLY value (a finite call never RETURNS NaN/±Inf).

// evalFloatFunc evaluates a float scalar function over its already-evaluated, non-NULL args. fn is
// the float scalar function; result is the call's width (Float32/Float64). vals[0] is the float
// operand (either width — widened to f64 for the kernel); a 2-arg form carries vals[1].
func evalFloatFunc(fn scalarFunc, vals []Value, result ScalarType) (Value, error) {
	x := vals[0].asF64()
	// wrap returns the result at the call's width (sfFloatAbs may be float32; the rest are float64).
	wrap := func(r float64) Value {
		if result.IsFloat32() {
			return Float32Value(float32(r))
		}
		return Float64Value(r)
	}
	switch fn {
	case sfFloatAbs:
		return wrap(math.Abs(x)), nil // abs preserves the arg width (Float32 if the arg was)
	case sfCeil:
		return Float64Value(math.Ceil(x)), nil
	case sfFloor:
		return Float64Value(math.Floor(x)), nil
	case sfTrunc:
		return Float64Value(math.Trunc(x)), nil
	case sfFloatRound:
		places := int64(0)
		if len(vals) > 1 {
			places = vals[1].Int
		}
		return Float64Value(roundFloatPlaces(x, places)), nil
	case sfSqrt:
		// sqrt is IEEE-correctly-rounded (in-contract). Negative → 22003 (NaN is input-only).
		if x < 0 {
			return Value{}, NewError(NumericValueOutOfRange, "cannot take square root of a negative number")
		}
		return Float64Value(math.Sqrt(x)), nil
	case sfExp:
		r := math.Exp(x)
		if math.IsInf(r, 0) && isFinite(x) {
			return Value{}, overflowFloatErr() // exp(710) overflows → 22003
		}
		return Float64Value(r), nil
	case sfLn:
		if x == 0 {
			return Value{}, NewError(NumericValueOutOfRange, "cannot take logarithm of zero")
		}
		if x < 0 {
			return Value{}, NewError(NumericValueOutOfRange, "cannot take logarithm of a negative number")
		}
		return Float64Value(math.Log(x)), nil
	case sfLog10:
		if x == 0 {
			return Value{}, NewError(NumericValueOutOfRange, "cannot take logarithm of zero")
		}
		if x < 0 {
			return Value{}, NewError(NumericValueOutOfRange, "cannot take logarithm of a negative number")
		}
		return Float64Value(math.Log10(x)), nil
	case sfPow:
		y := vals[1].asF64()
		r := math.Pow(x, y)
		if math.IsInf(r, 0) && isFinite(x) && isFinite(y) {
			return Value{}, overflowFloatErr() // finite^finite overflow → 22003
		}
		return Float64Value(r), nil
	case sfSin:
		return Float64Value(math.Sin(x)), nil
	case sfCos:
		return Float64Value(math.Cos(x)), nil
	case sfTan:
		return Float64Value(math.Tan(x)), nil
	default:
		panic("BUG: evalFloatFunc on a non-float scalar function")
	}
}

// roundFloatPlaces rounds f to n decimal places, HALF AWAY FROM ZERO (jed's one rounding mode —
// spec/design/float.md §8). For n ≤ 0 it rounds to the corresponding power of ten. NaN/±Inf pass
// through unchanged. Computed via scale-by-10^n; the scaling is approximate (this is float), which
// is fine — the value is within the `R` tag's tolerance and a documented exempted surface.
func roundFloatPlaces(f float64, n int64) float64 {
	if !isFinite(f) {
		return f
	}
	if n == 0 {
		return math.Round(f) // math.Round is round-half-away-from-zero
	}
	scale := math.Pow(10, float64(n))
	if math.IsInf(scale, 0) || scale == 0 {
		// n far out of range: rounding has no effect (10^n overflows or underflows).
		if scale == 0 {
			return f
		}
		return f
	}
	return math.Round(f*scale) / scale
}
