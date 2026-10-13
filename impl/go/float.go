package jed

// IEEE 754 binary floating point — f32 / f64 (spec/design/float.md). The engine's
// APPROXIMATE numeric, the deliberate opposite of decimal: inexact, base-2, and admitting
// NaN/±Infinity. It is the FIRST type partially exempted from cross-core byte-identity
// (determinism.md §6), but the exemption is NARROW — storage, the total order, the
// +-*/ /sqrt kernel, MIN/MAX/COUNT and int/decimal SUM/AVG, and cost all stay fully
// deterministic and cross-core, and so does the text (PostgreSQL's float8out — §9).
// TRANSCENDENTAL function values and the float SUM/AVG fold ORDER (streaming scan-order — §7) are
// exempt (absorbed by the conformance `R` tag). This file holds the float-specific value logic: the total order, the kernel, the
// casts, and the streaming scan-order fold.

import (
	"math"
	"strconv"
	"strings"
)

// radiansPerDegree is PG's exact RADIANS_PER_DEGREE literal (float.c), shared by radians/degrees
// so the single IEEE multiply/divide is byte-identical cross-core and matches PG (in-contract).
const radiansPerDegree = 0.0174532925199432957692

// --- the total order (spec/design/float.md §3) --------------------------------------------
//
// IEEE comparison is a PARTIAL order (NaN unordered; -0 == +0). SQL needs a TOTAL order for
// ORDER BY / DISTINCT / GROUP BY / MIN / MAX / =. jed adopts PostgreSQL's float8 btree order:
//
//	-Infinity  <  (finite, numerically)  <  +Infinity  <  NaN
//
// with -0 = +0 and NaN = NaN (all NaN bit patterns are ONE equivalence class). So NaN is the
// single largest value and `NaN = NaN` is TRUE. A documented divergence from raw IEEE.

// canonicalNaN64Bits is the single quiet-NaN bit pattern jed materializes for a f64 NaN
// (spec/design/float.md §3/§10). Go's math.NaN() is 0x7FF8000000000001 — its low payload bit
// differs from the canonical 0x7FF8000000000000 the Rust/TS cores produce, so a NaN LITERAL uses
// THIS pattern to stay cross-core byte-identical in memory (and the storage codec re-canonicalizes
// any other NaN, e.g. hardware Inf-Inf, on the way to disk). f32's canonical NaN is 0x7FC00000.
const canonicalNaN64Bits uint64 = 0x7FF8000000000000

// canonicalNaN64 is the f64 NaN jed materializes (see canonicalNaN64Bits).
func canonicalNaN64() float64 { return math.Float64frombits(canonicalNaN64Bits) }

// floatTotalRank maps a f64 to a totally-ordered class rank: every NaN → the largest class,
// everything else compares numerically with -0 folded to +0. Used only as a tie-break gate.
//
// floatTotalCmp is the total-order comparison of two f64 values (the §3 order), returning
// <0, 0, >0. NaN is the largest (NaN vs NaN is 0; NaN vs anything finite/Inf is +1). -0 and +0
// compare equal because Go's < / > already treat them equal and 0 == 0. Mixed widths reach
// here already widened to f64 (lossless — §2).
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

// --- order-preserving key encoding (spec/design/encoding.md §2.8) -------------------------
//
// encodeFloat64Key is the float-order-preserving KEY body for an f64: canonicalize (-0 → +0, every
// NaN → the one quiet pattern 0x7FF8…000), take the bits big-endian, then if the sign bit is set flip
// ALL 64 bits else flip just the sign bit — mapping the binary64 TOTAL order (§3, -Inf < finite <
// +Inf < NaN) onto unsigned byte order. Fixed 8 bytes (self-delimiting by width). -0/+0 and any two
// NaNs collapse to one key, so a UNIQUE float key treats them as one. (The stored VALUE codec keeps
// the bits verbatim — only a NaN is canonicalized — since a value never sorts; format.go.)
func encodeFloat64Key(bits uint64) []byte {
	switch f := math.Float64frombits(bits); {
	case math.IsNaN(f):
		bits = canonicalNaN64Bits
	case f == 0:
		bits = 0 // both -0 and +0
	}
	if bits>>63 == 1 {
		bits ^= 0xFFFFFFFFFFFFFFFF
	} else {
		bits ^= 0x8000000000000000
	}
	return []byte{
		byte(bits >> 56), byte(bits >> 48), byte(bits >> 40), byte(bits >> 32),
		byte(bits >> 24), byte(bits >> 16), byte(bits >> 8), byte(bits),
	}
}

// encodeFloat32Key is encodeFloat64Key at binary32 width (4 bytes; canonical NaN 0x7FC00000).
func encodeFloat32Key(bits uint32) []byte {
	switch f := math.Float32frombits(bits); {
	case math.IsNaN(float64(f)):
		bits = 0x7FC00000
	case f == 0:
		bits = 0
	}
	if bits>>31 == 1 {
		bits ^= 0xFFFFFFFF
	} else {
		bits ^= 0x80000000
	}
	return []byte{byte(bits >> 24), byte(bits >> 16), byte(bits >> 8), byte(bits)}
}

// --- the PostgreSQL float8out / float4out text (spec/design/float.md §9) -------------------
//
// The text of a float wherever it is rendered: a result cell, CAST(float AS text), an array or
// composite's text, a JSON document, EXPLAIN. Its digits are those of PostgreSQL's Ryu (src/common/d2s.c / f2s.c, built with STRICTLY_SHORTEST 0):
// the fewest significant digits of a decimal STRICTLY inside the float's rounding interval, the
// one nearest the exact value, a tie to the even last digit. Its layout is Ryu's to_chars:
// fixed-point when the decimal exponent is in [-4, 15) for an f64 or [-4, 6) for an f32, otherwise
// d[.ddd]e±XX with at least two exponent digits.

// floatShortest is PostgreSQL's shortest decimal of a finite, nonzero float at bitSize width: its
// sign, its significant digits (no leading or trailing zeros), and the decimal exponent of the
// first digit. An f64 takes the native shortest round trip when floatNativeShortestAgrees proves
// it equal; otherwise (and for every f32) the digits are computed exactly, from the native length:
// a native formatter accepts every decimal Ryu does, so its shortest is never longer.
func floatShortest(f float64, bitSize int) (neg bool, digits string, exp int) {
	var m uint64
	var e int
	var lowerTight bool
	if bitSize == 32 {
		bits := math.Float32bits(float32(f))
		neg = bits>>31 != 0
		be, frac := int((bits>>23)&0xff), uint64(bits&(1<<23-1))
		m, e, lowerTight = frac|1<<23, be-150, frac == 0 && be > 1
		if be == 0 {
			m, e = frac, -149
		}
	} else {
		bits := math.Float64bits(f)
		neg = bits>>63 != 0
		be, frac := int((bits>>52)&0x7ff), bits&(1<<52-1)
		m, e, lowerTight = frac|1<<52, be-1075, frac == 0 && be > 1
		if be == 0 {
			m, e = frac, -1074
		}
	}
	s := strconv.FormatFloat(math.Abs(f), 'e', -1, bitSize) // "d.ddde±XX"
	i := strings.IndexByte(s, 'e')
	exp, _ = strconv.Atoi(s[i+1:])
	digits = strings.Replace(s[:i], ".", "", 1)
	if bitSize == 64 && floatNativeShortestAgrees(m, e, lowerTight) {
		return neg, digits, exp
	}
	digits, exp = floatShortestExact(m, e, lowerTight, len(digits))
	return neg, digits, exp
}

// floatNativeShortestAgrees reports that the binary64 m·2^e (m > 0) cannot be a case where a native
// shortest round trip (≤ 17 digits, the nearest of its length) differs from PostgreSQL's: neither
// an exact tie between two shortest candidates nor a candidate ON a bound of the rounding interval,
// which a native formatter may accept and Ryu does not. lowerTight says the interval's lower half
// is a quarter ulp (m is a power of two above the lowest binade).
//
// An integer below 2^53 has an interval at most one unit wide, whose bounds have more digits than
// the integer, so the integer is the only candidate of its length or shorter. Otherwise both
// failures need a value with few significant digits. A tie needs the exact value to have at most 18,
// so (with the odd part o and exponent k of the value o·2^k) k is in [-25, -1]: a negative k gives
// o·5^-k significant digits, at least 5^25's 18 for k ≤ -25, and an integer cannot tie (its
// shortest candidates lie a whole unit of their last digit apart, wider than the interval). A bound
// (2m±1)·2^(e-1), or (4m-1)·2^(e-2), is a candidate only if it has at most 17 significant digits:
// a negative exponent k needs k ≥ -24 by the same count; an integer bound needs to be under 10^17
// or to end in a zero, which needs a factor 5 in its odd part.
func floatNativeShortestAgrees(m uint64, e int, lowerTight bool) bool {
	o, k := m, e
	for o&1 == 0 {
		o >>= 1
		k++
	}
	if k >= 0 && k < 53 && o < 1<<(53-k) {
		return true
	}
	if k >= -25 && k <= -1 {
		return false
	}
	boundMayBeShort := func(o uint64, k int) bool {
		if k < 0 {
			return k >= -25
		}
		return k < 57 && o < 1<<(57-k) || o%5 == 0
	}
	if boundMayBeShort(2*m+1, e-1) {
		return false
	}
	if lowerTight {
		return !boundMayBeShort(4*m-1, e-2)
	}
	return !boundMayBeShort(2*m-1, e-1)
}

// floatShortestExact is floatShortest's digits and exponent by exact decimal arithmetic over the
// rounding interval of m·2^e: for each length n from minLen, the n-digit decimals just below and
// just above the value, kept if strictly inside the interval; the first length with one wins, the
// nearer of two, a tie to the even last digit. minLen is at most the answer's length.
func floatShortestExact(m uint64, e int, lowerTight bool, minLen int) (string, int) {
	lower := exactDyadic(2*m-1, e-1)
	if lowerTight {
		lower = exactDyadic(4*m-1, e-2)
	}
	upper := exactDyadic(2*m+1, e-1)
	o, k := m, e
	for o&1 == 0 {
		o >>= 1
		k++
	}
	_, xd, xs := exactDyadic(o, k).canonical()
	top := len(xd) - 1 - int(xs) // the decimal exponent of the value's first digit
	for n := minLen; ; n++ {
		q := top - n + 1 // the decimal exponent of an n-digit candidate's last digit
		lo, rest := xd, ""
		if len(xd) > n {
			lo, rest = xd[:n], xd[n:]
		} else {
			lo += strings.Repeat("0", n-len(xd))
		}
		hi := incrementDigits(lo)
		loIn := decimalTimesPow10(lo, q).CmpValue(lower) > 0
		hiIn := decimalTimesPow10(hi, q).CmpValue(upper) < 0
		var pick string
		switch {
		case loIn && hiIn:
			// The digits after the n-th against one half of the n-th digit's unit.
			r := strings.TrimRight(rest, "0")
			switch {
			case r == "" || r[0] < '5':
				pick = lo
			case r == "5" && (lo[len(lo)-1]-'0')%2 == 0:
				pick = lo
			default:
				pick = hi
			}
		case loIn:
			pick = lo
		case hiIn:
			pick = hi
		default:
			continue
		}
		return strings.TrimRight(pick, "0"), q + len(pick) - 1
	}
}

// exactDyadic is the exact decimal of the nonnegative o·2^k, multiplying by 2^30 or 5^13 a step
// (magMulSmall takes a factor up to about 1.8e10).
func exactDyadic(o uint64, k int) Decimal {
	mag := magFromUint64(o)
	if k >= 0 {
		for ; k >= 30; k -= 30 {
			mag = magMulSmall(mag, 1<<30)
		}
		return newDecimal(false, 0, magMulSmall(mag, 1<<k))
	}
	scale := uint32(-k)
	for k = -k; k >= 13; k -= 13 {
		mag = magMulSmall(mag, 1220703125) // 5^13
	}
	p := uint64(1)
	for ; k > 0; k-- {
		p *= 5
	}
	return newDecimal(false, scale, magMulSmall(mag, p))
}

// decimalTimesPow10 is the nonnegative decimal digits·10^q.
func decimalTimesPow10(digits string, q int) Decimal {
	if q >= 0 {
		return decimalFromDigitsScale(false, digits+strings.Repeat("0", q), 0)
	}
	return decimalFromDigitsScale(false, digits, uint32(-q))
}

// incrementDigits adds one to a decimal digit string, growing it by a digit on a carry out.
func incrementDigits(digits string) string {
	b := []byte(digits)
	for i := len(b) - 1; i >= 0; i-- {
		if b[i] < '9' {
			b[i]++
			return string(b)
		}
		b[i] = '0'
	}
	return "1" + string(b)
}

// renderFloatPG is PostgreSQL's float8out (bitSize 64) or float4out (bitSize 32) text of f, at the
// oracle's extra_float_digits = 1 (the shortest round trip).
func renderFloatPG(f float64, bitSize int) string {
	switch {
	case math.IsNaN(f):
		return "NaN"
	case math.IsInf(f, 1):
		return "Infinity"
	case math.IsInf(f, -1):
		return "-Infinity"
	case f == 0 && math.Signbit(f):
		return "-0"
	case f == 0:
		return "0"
	}
	neg, digits, exp := floatShortest(f, bitSize)
	fixedLimit := 15
	if bitSize == 32 {
		fixedLimit = 6
	}
	var b strings.Builder
	if neg {
		b.WriteByte('-')
	}
	switch {
	case exp < -4 || exp >= fixedLimit:
		b.WriteByte(digits[0])
		if len(digits) > 1 {
			b.WriteByte('.')
			b.WriteString(digits[1:])
		}
		b.WriteByte('e')
		if exp < 0 {
			b.WriteByte('-')
			exp = -exp
		} else {
			b.WriteByte('+')
		}
		if exp < 10 {
			b.WriteByte('0')
		}
		b.WriteString(strconv.Itoa(exp))
	case exp < 0:
		b.WriteString("0.")
		b.WriteString(strings.Repeat("0", -exp-1))
		b.WriteString(digits)
	case exp >= len(digits)-1:
		b.WriteString(digits)
		b.WriteString(strings.Repeat("0", exp-len(digits)+1))
	default:
		b.WriteString(digits[:exp+1])
		b.WriteByte('.')
		b.WriteString(digits[exp+1:])
	}
	return b.String()
}

// floatJSONNumber is the jsonb number of a finite float: PostgreSQL's numeric_in over the
// renderFloatPG text, whose display scale is the count of digits after the point in fixed notation.
// -0 is 0 (a numeric has no negative zero).
func floatJSONNumber(f float64, bitSize int) Decimal {
	if f == 0 {
		return decimalZero(0)
	}
	neg, digits, exp := floatShortest(f, bitSize)
	shift := exp - len(digits) + 1
	if shift >= 0 {
		return decimalFromDigitsScale(neg, digits+strings.Repeat("0", shift), 0)
	}
	return decimalFromDigitsScale(neg, digits, uint32(-shift))
}

// floatOf is a float value as a float64 (an f32 widens exactly) and its width in bits.
func floatOf(v Value) (float64, int) {
	if v.Kind == ValFloat32 {
		return float64(v.F32()), 32
	}
	return v.F64(), 64
}

// --- the arithmetic kernel (spec/design/float.md §5) --------------------------------------
//
// float ⊕ float → float for + - * / % and unary -. Each is the IEEE 754 correctly-rounded
// operation (round-ties-to-even), ONE operator per node (the tree-walk guarantees no FMA
// contraction — float.md §5). FINITE arithmetic NEVER produces Inf/NaN: a finite result that
// overflows traps 22003; x/0 traps 22012. An operand that is ALREADY Inf/NaN propagates per
// IEEE (no trap). Mixed widths promote to f64 first (so the result kind is f64).

// evalFloatArith evaluates one float arithmetic op. a and b are the operand VALUES (each
// ValFloat32 or ValFloat64). resultIs32 says the static result type is f32 (both operands
// f32); otherwise f64 (either operand f64 — the promotion). Returns a float Value
// of the result width, or a trap (22003 finite-overflow, 22012 division by zero).
func evalFloatArith(op binaryOp, a, b Value, resultIs32 bool) (Value, error) {
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
// pair whose result overflows to ±Inf traps 22003; / or % by zero traps 22012 for EVERY numerator
// except NaN (matching PG — `Inf/0` and `0/0` trap, only `NaN/0` propagates to NaN); an Inf/NaN
// operand otherwise propagates without trapping.
func float64Op(op binaryOp, x, y float64) (float64, error) {
	var r float64
	switch op {
	case opAdd:
		r = x + y
	case opSub:
		r = x - y
	case opMul:
		r = x * y
	case opDiv:
		// x / 0 traps 22012 (PG) for every numerator — finite, ±Inf, and 0/0 — EXCEPT NaN, which
		// propagates (NaN/0 = NaN). The strict type system keeps the trap, not a silent ±Inf (§5).
		if y == 0 && !math.IsNaN(x) {
			return 0, newError(DivisionByZero, "division by zero")
		}
		r = x / y
	default: // OpMod — IEEE fmod; the zero divisor follows the SAME rule as division.
		if y == 0 && !math.IsNaN(x) {
			return 0, newError(DivisionByZero, "division by zero")
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

// float32Op is float64Op at binary32 width — every op rounds to binary32 (Go f32 arithmetic),
// matching the §2/§5 "compute at the input width" rule. The finite-overflow check is against the
// binary32 range (a finite f32 pair whose true result exceeds f32 max → ±Inf → 22003).
func float32Op(op binaryOp, x, y float32) (float32, error) {
	var r float32
	switch op {
	case opAdd:
		r = x + y
	case opSub:
		r = x - y
	case opMul:
		r = x * y
	case opDiv:
		// Same zero-divisor rule as f64: traps for every numerator except NaN (Inf/0 traps).
		if y == 0 && !math.IsNaN(float64(x)) {
			return 0, newError(DivisionByZero, "division by zero")
		}
		r = x / y
	default: // OpMod
		if y == 0 && !math.IsNaN(float64(x)) {
			return 0, newError(DivisionByZero, "division by zero")
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
	return newError(NumericValueOutOfRange, "value out of range: overflow")
}

// --- casts (spec/design/float.md §6, ../types/casts.toml) ---------------------------------

// intToFloat64 converts an integer (any width, carried in i64) to the nearest binary64,
// round-ties-to-even (the IEEE conversion; never traps — §6).
func intToFloat64(n int64) float64 { return float64(n) }

// intToFloat32 converts an integer to the nearest binary32, round-ties-to-even (never traps).
func intToFloat32(n int64) float32 { return float32(n) }

// floatToInt converts a float value to an integer of target type, rounding HALF AWAY FROM ZERO
// (jed's one rounding mode — a documented divergence from PG's half-to-even rint), then
// range-checking (22003). NaN / ±Inf → 22003 (no integer representation). spec/design/float.md §6.
func floatToInt(f float64, target scalarType) (int64, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return 0, overflowErr(target)
	}
	r := math.Round(f) // Go's math.Round is round-half-AWAY-from-zero — exactly jed's mode
	// math.Round of a huge value stays huge; the i64 conversion below must be range-guarded
	// BEFORE the Go float→int conversion (which is undefined for out-of-i64-range values).
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
func floatToDecimal(f float64, typmod *decimalTypmod) (Value, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return Value{}, newError(NumericValueOutOfRange, "cannot convert a non-finite float to decimal")
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
		return decimalZero(0) // +0 and -0 both → exact 0
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
	return decimalFromDigitsScale(dneg, digits, scale)
}

// decimalFromFloat64DblDig is PostgreSQL's float8_numeric: the binary64 value printed with %.15g
// (DBL_DIG significant digits) and read back as a numeric. Used by the jsonpath .double() method
// (spec/design/jsonpath.md §3.1) and a float PASSING value; the caller has already rejected NaN and
// ±Infinity.
func decimalFromFloat64DblDig(f float64) Decimal { return decimalFromFloatSigDigits(f, 15) }

// decimalFromFloat32FltDig is PostgreSQL's float4_numeric: %.6g (FLT_DIG) of the binary32 value.
func decimalFromFloat32FltDig(f float32) Decimal { return decimalFromFloatSigDigits(float64(f), 6) }

// decimalFromFloatSigDigits is a finite float printed with %.<sig>g and read back as a numeric. The
// digits come from the EXACT decimal expansion (exactDecimalFromFloat64; an f32 widens exactly),
// rounded to sig significant digits half-to-even (glibc's printf rounds the exact value to nearest,
// ties to even), with trailing fractional zeros trimmed — %g drops them, and an exponent form reads
// back at display scale max(0, digits − exponent). Pure integer arithmetic, so every core agrees
// bit-for-bit.
func decimalFromFloatSigDigits(f float64, sig uint32) Decimal {
	exact := exactDecimalFromFloat64(f)
	digits := exact.Precision()
	if digits <= sig {
		return exact
	}
	drop := digits - sig
	pow := magPow10(drop)
	q, r := magDivMod(exact.Limbs, pow)
	roundUp := false
	switch magCmp(magAdd(r, r), pow) {
	case 1:
		roundUp = true
	case 0:
		roundUp = len(q) > 0 && q[0]%2 == 1
	}
	if roundUp {
		q = magAdd(q, []uint32{1})
	}
	var rounded Decimal
	if exact.Scale >= drop {
		rounded = newDecimal(exact.Neg, exact.Scale-drop, q)
	} else {
		rounded = newDecimal(exact.Neg, 0, magMulPow10(q, drop-exact.Scale))
	}
	neg, cdigits, cscale := rounded.canonical()
	return decimalFromDigitsScale(neg, cdigits, cscale)
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

// float64ToFloat32 narrows a f64 to f32, round-ties-to-even. A finite f64 beyond the
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
// jed defines float SUM/AVG as a STREAMING SCAN-ORDER fold: special-value flags tracked, then each
// finite input is -0-canonicalized and folded into a single running total in the order the scan
// produces rows (float.md §7). O(1) memory. The fold ORDER is ledgered non-deterministic
// (determinism_exceptions.toml `float-sum-order`, class A∩P): cross-core-identical on today's serial
// executor (every core folds the same PK-scan order with the same IEEE add) but not promised against
// a future parallel plan. The special-value result and the 22003 overflow trap stay order-independent.

// floatSumAcc folds the inputs of a float SUM/AVG into a running total. is32 selects the fold width
// (f32 vs f64): when is32 the running total is re-rounded to binary32 each add (the §7 width-correct
// f32 fold). It records the special-value flags and the running finite total; NaN/±Inf never enter
// the total (they are resolved by precedence at finalize).
type floatSumAcc struct {
	is32      bool
	sawNaN    bool
	sawPosInf bool
	sawNegInf bool
	total     float64 // running finite sum (at f64; re-rounded to binary32 each add when is32)
	count     int64   // non-NULL input count (for AVG)
}

func newFloatSumAcc(is32 bool) *floatSumAcc { return &floatSumAcc{is32: is32} }

// add folds one non-NULL float input into the running total (NULLs are skipped by the caller). A
// special value only sets its flag; a finite value is -0-canonicalized and added in scan order.
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
		x := canonicalizeFloat64(f)
		if a.is32 {
			// Width-correct f32 fold: recover the f32-valued running total, add at binary32, re-widen.
			a.total = float64(float32(a.total) + float32(x))
		} else {
			a.total += x
		}
	}
}

// sumF64 resolves the SUM as a f64 result (the special-value precedence, then the running total).
// ok=false means the group was empty (→ NULL). A running total that reached ±Inf from finite inputs
// is a finite-overflow 22003 — a final isInf test is equivalent to a per-add one, since finite + ±Inf
// cannot recover to finite.
func (a *floatSumAcc) sumF64() (float64, bool, error) {
	if a.count == 0 {
		return 0, false, nil
	}
	if special, isSpecial := a.specialSum(); isSpecial {
		return special, true, nil
	}
	if math.IsInf(a.total, 0) {
		return 0, false, overflowFloatErr()
	}
	return a.total, true, nil
}

// sumF32 resolves the SUM as a f32 result — the running total already folded at binary32 width, so
// float32(total) recovers it exactly.
func (a *floatSumAcc) sumF32() (float32, bool, error) {
	if a.count == 0 {
		return 0, false, nil
	}
	if special, isSpecial := a.specialSum(); isSpecial {
		return float32(special), true, nil
	}
	t := float32(a.total)
	if math.IsInf(float64(t), 0) {
		return 0, false, overflowFloatErr()
	}
	return t, true, nil
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

// avgF64 resolves AVG as a f64: SUM / count, the division rounded once (empty → NULL). A NaN
// or ±Inf sum carries through the division (NaN/n = NaN, ±Inf/n = ±Inf).
func (a *floatSumAcc) avgF64() (float64, bool, error) {
	s, ok, err := a.sumF64()
	if err != nil || !ok {
		return 0, ok, err
	}
	return s / float64(a.count), true, nil
}

// avgF32 resolves AVG as a f32 (sum at f32 / count, one rounding).
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
// width is the call's `result` (Float32 only for abs over a f32 arg; f64 for the rest, per
// the catalog). Domain errors follow PG: ln(0) → 22003, sqrt/ln of a negative → 22003, exp/pow
// overflow → 22003 — keeping NaN an INPUT-ONLY value (a finite call never RETURNS NaN/±Inf).

// evalFloatFunc evaluates a float scalar function over its already-evaluated, non-NULL args. fn is
// the float scalar function; result is the call's width (Float32/Float64). vals[0] is the float
// operand (either width — widened to f64 for the kernel); a 2-arg form carries vals[1].
func evalFloatFunc(fn scalarFunc, vals []Value, result scalarType) (Value, error) {
	x := vals[0].asF64()
	// wrap returns the result at the call's width (sfFloatAbs may be f32; the rest are f64).
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
			return Value{}, newError(NumericValueOutOfRange, "cannot take square root of a negative number")
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
			return Value{}, newError(NumericValueOutOfRange, "cannot take logarithm of zero")
		}
		if x < 0 {
			return Value{}, newError(NumericValueOutOfRange, "cannot take logarithm of a negative number")
		}
		return Float64Value(math.Log(x)), nil
	case sfLog10:
		if x == 0 {
			return Value{}, newError(NumericValueOutOfRange, "cannot take logarithm of zero")
		}
		if x < 0 {
			return Value{}, newError(NumericValueOutOfRange, "cannot take logarithm of a negative number")
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
	case sfCbrt:
		// cbrt has no domain restriction: cbrt(-8) = -2, cbrt(±Inf) = ±Inf, cbrt(NaN) = NaN.
		return Float64Value(math.Cbrt(x)), nil
	case sfRadians:
		// radians/degrees — a single correctly-rounded IEEE op (multiply/divide) by PG's exact
		// RADIANS_PER_DEGREE literal (float.c), so byte-identical cross-core (in-contract).
		return Float64Value(x * radiansPerDegree), nil
	case sfDegrees:
		return Float64Value(x / radiansPerDegree), nil
	case sfAsin:
		// asin domain is [-1, 1]: a finite |x| > 1 (and ±Inf, magnitude > 1) is out of range →
		// 22003, exactly PG; a NaN operand propagates (no trap).
		if !math.IsNaN(x) && (x < -1 || x > 1) {
			return Value{}, newError(NumericValueOutOfRange, "input is out of range")
		}
		return Float64Value(math.Asin(x)), nil
	case sfAcos:
		// acos shares asin's domain [-1, 1]: |x| > 1 (or ±Inf) → 22003, NaN propagates.
		if !math.IsNaN(x) && (x < -1 || x > 1) {
			return Value{}, newError(NumericValueOutOfRange, "input is out of range")
		}
		return Float64Value(math.Acos(x)), nil
	case sfAtan:
		// atan is defined on all of ℝ (no domain trap); atan(±Inf) = ±π/2, atan(NaN) = NaN.
		return Float64Value(math.Atan(x)), nil
	case sfAtan2:
		// atan2(y, x): y is vals[0] (x here), x is vals[1]. Quadrant-aware; no domain trap.
		return Float64Value(math.Atan2(x, vals[1].asF64())), nil
	case sfCot:
		// cot(x) = 1/tan(x) (no math.Cot; 1/tan bit-matches PG). cot(0) = +Inf (no trap).
		return Float64Value(1.0 / math.Tan(x)), nil
	case sfSinh:
		// sinh/cosh overflow to ±Inf with NO trap (PG-faithful, unlike exp/pow). NaN propagates.
		return Float64Value(math.Sinh(x)), nil
	case sfCosh:
		return Float64Value(math.Cosh(x)), nil
	case sfTanh:
		return Float64Value(math.Tanh(x)), nil
	case sfAsinh:
		return Float64Value(math.Asinh(x)), nil
	case sfAcosh:
		// acosh domain [1, ∞): a finite x < 1 → 22003 (a NaN propagates, acosh(+Inf) = +Inf).
		if !math.IsNaN(x) && x < 1 {
			return Value{}, newError(NumericValueOutOfRange, "input is out of range")
		}
		return Float64Value(math.Acosh(x)), nil
	case sfAtanh:
		// atanh domain [-1, 1]: a finite |x| > 1 (and ±Inf) → 22003; atanh(±1) = ±Inf is admissible.
		if !math.IsNaN(x) && (x < -1 || x > 1) {
			return Value{}, newError(NumericValueOutOfRange, "input is out of range")
		}
		return Float64Value(math.Atanh(x)), nil
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
