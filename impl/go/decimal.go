package jed

import (
	"math"
	"strconv"
	"strings"
)

// Exact base-10 decimal / numeric (spec/design/decimal.md). A value is
// (neg, coefficient, scale) = (-1)^neg · coefficient · 10^(-scale). The coefficient is a
// hand-rolled big integer in base 10^9 limbs, least-significant-first (the order is internal —
// only the rendered value and on-disk bytes are cross-core contracts, CLAUDE.md §2). No bignum
// library (math/big is permitted only as a test oracle, never on the value path), so the limb
// algorithm is the spec, identical across cores. Always finite (no NaN/±Infinity) and
// normalized (no high zero limbs, no negative zero). Rounding is half away from zero (§3).

const (
	decBase       = uint64(1_000_000_000) // 10^9: a uint32 limb holds 9 digits
	decBaseDigits = 9
	// MaxPrecision is the max DECLARABLE precision of numeric(p,s), and the division
	// display-scale clamp — spec/types/scalars.toml max_precision (PG NUMERIC_MAX_PRECISION,
	// which is also its NUMERIC_MAX_DISPLAY_SCALE). NOT a cap on what a value may carry.
	MaxPrecision = 1000
	// MaxIntDigits is the max integer-part digits ANY value may carry — spec/types/
	// scalars.toml max_int_digits (PG (NUMERIC_WEIGHT_MAX+1)*DEC_DIGITS; decimal.md §2).
	MaxIntDigits = 131072
	// MaxScale is the max digits after the point ANY value may carry — spec/types/
	// scalars.toml max_scale (PG NUMERIC_DSCALE_MAX; decimal.md §2).
	MaxScale = 16383
	// expLimit is the magnitude clamp for a decimal literal's scientific e-notation exponent,
	// tied to the format caps so lexing/parsing stays bounded — 1e9999999999 must not
	// materialize a gigabyte of coefficient zeros — without changing any outcome: an exponent
	// this large already drives the value past the caps (so it traps 22003 at resolve), and a
	// zero coefficient still normalizes to 0 (spec/design/grammar.md §14). Callers clamp the
	// exponent magnitude to ±expLimit while scanning (both to honor this bound and to keep the
	// accumulation inside i64).
	expLimit = int64(MaxIntDigits) + int64(MaxScale) + 2
)

// Decimal is an exact base-10 decimal. Neg is the sign (always false for zero — no negative
// zero); Scale is the display scale; Limbs is the coefficient magnitude (base 10^9, LSB-first,
// no high zero limbs; nil/empty == zero).
type Decimal struct {
	Neg   bool
	Scale uint32
	Limbs []uint32
}

func decimalOverflow() error {
	return NewError(NumericValueOutOfRange, "value out of range for type decimal")
}

func decimalDivByZero() error {
	return NewError(DivisionByZero, "division by zero")
}

// newDecimal constructs from raw parts, normalizing (trim high zero limbs; force Neg=false for
// zero). The single choke-point every constructor ends with.
func newDecimal(neg bool, scale uint32, limbs []uint32) Decimal {
	limbs = magTrim(limbs)
	if len(limbs) == 0 {
		neg = false
	}
	return Decimal{Neg: neg, Scale: scale, Limbs: limbs}
}

// DecimalZero is zero at the given display scale.
func DecimalZero(scale uint32) Decimal { return Decimal{Neg: false, Scale: scale, Limbs: nil} }

// DecimalFromInt64 is the exact decimal of an integer (the lossless int→decimal cast, scale 0).
func DecimalFromInt64(v int64) Decimal {
	neg := v < 0
	u := uint64(v)
	if neg {
		u = -u // unsigned negation = |v| (handles MinInt64)
	}
	return newDecimal(neg, 0, magFromUint64(u))
}

// DecimalFromDigitsScale builds from a sign, an unscaled coefficient as a decimal-digit string
// (leading zeros allowed), and a scale. The literal/parse entry point — it does NOT enforce the
// precision/scale caps (the caller checks them at resolve, trapping 22003).
func DecimalFromDigitsScale(neg bool, digits string, scale uint32) Decimal {
	return newDecimal(neg, scale, magFromDecimalStr(digits))
}

// decimalFromParts is the canonical (coefficient digits, scale) for a decimal literal, from its
// mantissa (intPart+frac) and an optional scientific exponent (already clamped to ±expLimit by the
// caller's scanner; hasExp false means no exponent). The display scale is max(0, fracLen-exp); when
// the exponent drives it below zero the coefficient absorbs the surplus as trailing zeros at
// scale 0, so the value still reads coefficient × 10^(-scale). Shared by the lexer (bare 1.5e3)
// and the text→decimal coercion (numeric '1.5e3') so both spell the SAME value
// (spec/design/grammar.md §14); the result is fed to DecimalFromDigitsScale and cap-checked at
// resolve.
func decimalFromParts(intPart, frac string, hasExp bool, exp int64) (string, uint32) {
	fracLen := int64(len(frac))
	if !hasExp {
		return intPart + frac, uint32(fracLen)
	}
	effScale := fracLen - exp
	if effScale >= 0 {
		return intPart + frac, uint32(effScale)
	}
	zeros := int(-effScale)
	digits := make([]byte, 0, len(intPart)+len(frac)+zeros)
	digits = append(digits, intPart...)
	digits = append(digits, frac...)
	for k := 0; k < zeros; k++ {
		digits = append(digits, '0')
	}
	return string(digits), 0
}

// IsZero reports whether the value is zero.
func (d Decimal) IsZero() bool { return len(d.Limbs) == 0 }

// Precision is the number of significant digits in the coefficient (0 for zero).
func (d Decimal) Precision() uint32 { return magDigitCount(d.Limbs) }

// CheckCap traps 22003 if this (unconstrained) value exceeds the numeric-format caps
// (spec/design/decimal.md §2): more than MaxIntDigits integer-part digits or a scale over
// MaxScale — PG's make_result / numeric_in checks.
func (d Decimal) CheckCap() (Decimal, error) {
	intDigits := uint32(0)
	if p := d.Precision(); p > d.Scale {
		intDigits = p - d.Scale
	}
	if intDigits > MaxIntDigits || d.Scale > MaxScale {
		return Decimal{}, decimalOverflow()
	}
	return d, nil
}

// canonical returns the value-canonical (neg, coeff-digits, scale) with trailing fractional
// zeros stripped: 1.50 → ("15",1), 10.0 → ("10",0), 100 → ("100",0). Two values equal in value
// share this form — the key for equality and DISTINCT/GROUP BY (spec/design/decimal.md §5).
func (d Decimal) canonical() (bool, string, uint32) {
	if len(d.Limbs) == 0 {
		return false, "0", 0
	}
	digits := magToDecimalStr(d.Limbs)
	scale := d.Scale
	for scale > 0 && strings.HasSuffix(digits, "0") {
		digits = digits[:len(digits)-1]
		scale--
	}
	return d.Neg, digits, scale
}

// CanonicalString is a collision-free string of the value-canonical form, for DISTINCT dedup.
func (d Decimal) CanonicalString() string {
	neg, digits, scale := d.canonical()
	sign := "+"
	if neg {
		sign = "-"
	}
	return sign + digits + "e" + strconv.FormatUint(uint64(scale), 10)
}

// CmpValue is the total order over finite decimals by value: <0, 0, >0.
func (d Decimal) CmpValue(o Decimal) int {
	if d.Neg != o.Neg {
		if d.Neg { // neg sorts below non-neg; zero is non-neg
			return -1
		}
		return 1
	}
	s := d.Scale
	if o.Scale > s {
		s = o.Scale
	}
	a := magMulPow10(d.Limbs, s-d.Scale)
	b := magMulPow10(o.Limbs, s-o.Scale)
	m := magCmp(a, b)
	if d.Neg {
		return -m
	}
	return m
}

// Render is the canonical decimal string (spec/design/decimal.md §6): optional '-', the integer
// digits, and — iff Scale > 0 — '.' and exactly Scale fractional digits.
func (d Decimal) Render() string {
	digits := magToDecimalStr(d.Limbs) // "0" for zero
	var b strings.Builder
	if d.Neg {
		b.WriteByte('-')
	}
	if d.Scale == 0 {
		b.WriteString(digits)
		return b.String()
	}
	scale := int(d.Scale)
	want := scale + 1
	if len(digits) < want {
		digits = strings.Repeat("0", want-len(digits)) + digits
	}
	point := len(digits) - scale
	b.WriteString(digits[:point])
	b.WriteByte('.')
	b.WriteString(digits[point:])
	return b.String()
}

// Negate flips the sign (zero stays +0).
func (d Decimal) Negate() Decimal {
	return newDecimal(!d.Neg, d.Scale, d.Limbs)
}

// AddUncapped is exact addition, result scale max(s1,s2), WITHOUT the §2 format-cap check —
// the running form for the SUM/AVG accumulator, which (like PG) checks the cap only on the
// FINAL result, not each intermediate (spec/design/decimal.md §2, determinism.md §7). That
// makes the trap order-independent: whether a fold overflows no longer depends on the order
// rows are summed. Standalone arithmetic uses Add (capped).
func (d Decimal) AddUncapped(o Decimal) Decimal {
	s := d.Scale
	if o.Scale > s {
		s = o.Scale
	}
	a := magMulPow10(d.Limbs, s-d.Scale)
	b := magMulPow10(o.Limbs, s-o.Scale)
	if d.Neg == o.Neg {
		return newDecimal(d.Neg, s, magAdd(a, b))
	}
	switch magCmp(a, b) {
	case 0:
		return DecimalZero(s)
	case 1:
		return newDecimal(d.Neg, s, magSub(a, b))
	default:
		return newDecimal(o.Neg, s, magSub(b, a))
	}
}

// Add is exact addition, result scale max(s1,s2); traps 22003 at the cap.
func (d Decimal) Add(o Decimal) (Decimal, error) {
	return d.AddUncapped(o).CheckCap()
}

// Sub is d - o (= d + (-o)).
func (d Decimal) Sub(o Decimal) (Decimal, error) { return d.Add(o.Negate()) }

// Mul is exact multiplication, result scale s1+s2; traps 22003 at the integer-digit cap.
// A product scale over MaxScale ROUNDS to it instead of trapping (PG numeric_mul rounds the
// exact product — spec/design/decimal.md §2).
func (d Decimal) Mul(o Decimal) (Decimal, error) {
	scale := d.Scale + o.Scale
	exact := newDecimal(d.Neg != o.Neg, scale, magMul(d.Limbs, o.Limbs))
	if scale > MaxScale {
		exact = exact.RoundToScale(MaxScale)
	}
	return exact.CheckCap()
}

// Div is d / o with PG's select_div_scale result scale, rounded half away from zero
// (spec/design/decimal.md §4). Traps 22012 on a zero divisor, 22003 at the cap.
func (d Decimal) Div(o Decimal) (Decimal, error) {
	if o.IsZero() {
		return Decimal{}, decimalDivByZero()
	}
	rscale := selectDivScale(d, o)
	if d.IsZero() {
		return DecimalZero(rscale), nil
	}
	e := int64(rscale) + int64(o.Scale) - int64(d.Scale) // >= 0 since rscale >= s1
	numer := magMulPow10(d.Limbs, uint32(e))
	q, r := magDivMod(numer, o.Limbs)
	// Round half away from zero: if 2*r >= |divisor|, round the magnitude up.
	if magCmp(magAdd(r, r), o.Limbs) >= 0 {
		q = magAdd(q, []uint32{1})
	}
	return newDecimal(d.Neg != o.Neg, rscale, q).CheckCap()
}

// Rem is d % o — remainder of truncated division; result scale max(s1,s2), sign of the
// dividend (matches the integer %). Traps 22012 on a zero divisor.
func (d Decimal) Rem(o Decimal) (Decimal, error) {
	if o.IsZero() {
		return Decimal{}, decimalDivByZero()
	}
	s := d.Scale
	if o.Scale > s {
		s = o.Scale
	}
	a := magMulPow10(d.Limbs, s-d.Scale)
	b := magMulPow10(o.Limbs, s-o.Scale)
	_, r := magDivMod(a, b)
	return newDecimal(d.Neg, s, r), nil
}

// RoundToScale rounds to target scale, half away from zero (spec/design/decimal.md §3).
// Increasing the scale only appends zeros (exact).
func (d Decimal) RoundToScale(target uint32) Decimal {
	if target >= d.Scale {
		return newDecimal(d.Neg, target, magMulPow10(d.Limbs, target-d.Scale))
	}
	pow := magPow10(d.Scale - target)
	q, r := magDivMod(d.Limbs, pow)
	if magCmp(magAdd(r, r), pow) >= 0 {
		q = magAdd(q, []uint32{1})
	}
	return newDecimal(d.Neg, target, q)
}

// Abs is the magnitude, preserving scale — the abs(numeric) scalar function
// (spec/design/functions.md §9). Cannot overflow.
func (d Decimal) Abs() Decimal {
	return newDecimal(false, d.Scale, append([]uint32(nil), d.Limbs...))
}

// RoundPlaces is PG round(numeric, n) (spec/design/functions.md §9): round half away from zero
// to n fractional places. n >= 0 rounds to scale n (delegating to RoundToScale, with n clamped
// at MaxScale like PG numeric_round); n < 0 rounds to the LEFT of the point — result scale 0,
// value a multiple of 10^-n (round(150, -2) = 200). RoundPlaces(0) is round(x). Traps 22003
// when the round-up carry pushes a value at the integer-digit cap over it (decimal.md §4).
func (d Decimal) RoundPlaces(n int64) (Decimal, error) {
	if n >= 0 {
		target := uint32(MaxScale)
		if n < int64(MaxScale) {
			target = uint32(n)
		}
		return d.RoundToScale(target).CheckCap()
	}
	// Drop d.Scale + k digits of the magnitude (rounding half away), then re-append the k
	// integer zeros. k is capped at the digit count + 1: beyond that every value rounds to 0
	// (or a single carried 1), so the clamp changes no result but bounds the work.
	mag := uint64(-n) // two's-complement: the correct magnitude even for MinInt64
	k := d.Precision() + 1
	if mag < uint64(k) {
		k = uint32(mag)
	}
	pow := magPow10(d.Scale + k)
	q, r := magDivMod(d.Limbs, pow)
	if magCmp(magAdd(r, r), pow) >= 0 {
		q = magAdd(q, []uint32{1})
	}
	return newDecimal(d.Neg, 0, magMulPow10(q, k)).CheckCap()
}

// CoerceToTypmod coerces into numeric(precision, scale): round to scale (half away), then trap
// 22003 if the integer-part digits exceed precision-scale (spec/design/decimal.md §2).
func (d Decimal) CoerceToTypmod(precision, scale uint32) (Decimal, error) {
	rounded := d.RoundToScale(scale)
	sig := rounded.Precision()
	intDigits := uint32(0)
	if sig > scale {
		intDigits = sig - scale
	}
	if intDigits > precision-scale {
		return Decimal{}, decimalOverflow()
	}
	return rounded, nil
}

// ToInt64Round rounds to an integer (scale 0, half away) and returns it as i64 if it fits,
// else ok=false (the decimal→int cast; the caller range-checks the target int type).
func (d Decimal) ToInt64Round() (int64, bool) {
	r := d.RoundToScale(0)
	if len(r.Limbs) > 3 { // > 27 digits, far beyond i64
		return 0, false
	}
	var u uint64
	for i := len(r.Limbs) - 1; i >= 0; i-- {
		hi := u * decBase
		if u != 0 && hi/decBase != u { // multiply overflow
			return 0, false
		}
		next := hi + uint64(r.Limbs[i])
		if next < hi { // add overflow
			return 0, false
		}
		u = next
	}
	if r.Neg {
		const minMag = uint64(1) << 63 // |math.MinInt64|
		if u > minMag {
			return 0, false
		}
		if u == minMag {
			return math.MinInt64, true
		}
		return -int64(u), true
	}
	if u > uint64(math.MaxInt64) {
		return 0, false
	}
	return int64(u), true
}

// ToCodec returns (neg, scale, base-10^4 coefficient groups MS-first) for the value codec.
// Zero → no groups (spec/fileformat/format.md).
func (d Decimal) ToCodec() (bool, uint32, []uint16) {
	return d.Neg, d.Scale, magToNbase4(d.Limbs)
}

// DecimalFromCodec is the inverse of ToCodec (used on load).
func DecimalFromCodec(neg bool, scale uint32, groups []uint16) Decimal {
	return newDecimal(neg, scale, magFromNbase4(groups))
}

// EncodeKey returns the order-preserving KEY body for a decimal (method
// decimal-order-preserving, spec/design/encoding.md §2.5). Self-delimiting; sorts byte-for-byte
// under bytes.Compare identically to numeric value, INDEPENDENT of display scale — 1.5 and 1.50
// produce identical bytes (they are equal, so a UNIQUE decimal index treats them as one). A PK is
// NOT NULL, so the stored key is this bare body; the §2.2 nullable slot prepends the presence tag
// and §2.3 descending inverts the whole component (both at the caller).
//
// Normalize the value to (sign, base-100 mantissa pairs, E) with value = 0.<pairs> × 100^E, then
// emit: a sign/class byte (0x03 neg < 0x04 zero < 0x05 pos); the exponent E as a 4-byte
// order-preserving int-be-signflip i32 (§2.1 — larger E sorts later for positives); the mantissa
// pairs most-significant first, each as pair+1 ∈ [0x01, 0x64] (0x00 reserved for the terminator);
// and a 0x00 terminator (a shorter mantissa sorts before one that extends it). For NEGATIVE values
// the exponent, mantissa, and terminator are bitwise-complemented so "more negative" sorts first.
func (d Decimal) EncodeKey() []byte {
	// Zero is the single class byte 0x04 (neg 0x03 < zero 0x04 < pos 0x05).
	if d.IsZero() {
		return []byte{0x04}
	}
	// Significant digits (no leading zeros) and the base-10 decimal-point exponent:
	// value = 0.<digits> × 10^decpt, with decpt = precision − scale.
	digits := []byte(magToDecimalStr(d.Limbs))
	decpt := int32(d.Precision()) - int32(d.Scale)
	// Drop trailing zero digits (the least-significant ones): the mantissa keeps only its
	// significant part and decpt is unchanged, so 1.50 ("150") collapses onto 1.5 ("15").
	for len(digits) > 0 && digits[len(digits)-1] == '0' {
		digits = digits[:len(digits)-1]
	}
	// Base-100 exponent E (value = 0.<pairs> × 100^E) and pair alignment: prepend a '0' when
	// decpt is odd so the leading base-100 pair is "0 d1", then pad right to an even length.
	e := floorDiv2(decpt + 1)
	grouped := make([]byte, 0, len(digits)+2)
	if mod2(decpt) == 1 {
		grouped = append(grouped, '0')
	}
	grouped = append(grouped, digits...)
	if len(grouped)%2 == 1 {
		grouped = append(grouped, '0')
	}
	// Body: 4-byte order-preserving exponent ‖ mantissa pairs (pair+1) ‖ 0x00 terminator.
	body := make([]byte, 0, 4+len(grouped)/2+1)
	body = append(body, EncodeInt(Int32, int64(e))...)
	for i := 0; i < len(grouped); i += 2 {
		v := (grouped[i]-'0')*10 + (grouped[i+1] - '0')
		body = append(body, v+1)
	}
	body = append(body, 0x00)
	// Assemble with the sign/class byte; negatives complement the body (E+mantissa+terminator).
	out := make([]byte, 0, 1+len(body))
	if d.Neg {
		out = append(out, 0x03)
		for _, b := range body {
			out = append(out, ^b)
		}
	} else {
		out = append(out, 0x05)
		out = append(out, body...)
	}
	return out
}

// floorDiv2 is floor(n/2) for any int32 (Go's / truncates toward zero, so negative odd n needs
// the adjustment) — the order-preserving base-100 exponent math in EncodeKey.
func floorDiv2(n int32) int32 {
	if n >= 0 {
		return n / 2
	}
	return -((-n + 1) / 2)
}

// mod2 is the Euclidean n mod 2 ∈ {0,1} for any int32 (so a negative odd decpt reads as odd).
func mod2(n int32) int32 {
	return ((n % 2) + 2) % 2
}

// selectDivScale is PG's select_div_scale (spec/design/decimal.md §4): >=16 significant
// quotient digits, no fewer fractional digits than either input, in PG's base-10^4 units.
func selectDivScale(a, b Decimal) uint32 {
	w1, f1 := nbase4WeightLead(a)
	w2, f2 := nbase4WeightLead(b)
	qweight := w1 - w2
	if f1 <= f2 {
		qweight--
	}
	rscale := 16 - 4*qweight
	if int64(a.Scale) > rscale {
		rscale = int64(a.Scale)
	}
	if int64(b.Scale) > rscale {
		rscale = int64(b.Scale)
	}
	if rscale < 0 {
		rscale = 0
	}
	// PG's display-scale clamp: NUMERIC_MAX_DISPLAY_SCALE = NUMERIC_MAX_PRECISION (1000),
	// deliberately NOT the MaxScale value cap (spec/design/decimal.md §4).
	if rscale > MaxPrecision {
		rscale = MaxPrecision
	}
	return uint32(rscale)
}

// nbase4WeightLead returns a decimal value's PG base-10^4 weight (the power of 10^4 of the
// most-significant digit group) and the leading group f (0..9999). Zero → (0, 0).
func nbase4WeightLead(d Decimal) (int64, int64) {
	if d.IsZero() {
		return 0, 0
	}
	digits := int64(d.Precision())
	e := digits - 1 - int64(d.Scale) // exponent of the leading significant digit
	w := floorDiv4(e)                // floor(e / 4)
	g := int(e - 4*w + 1)            // 1..4 leading-group decimal digits
	s := magToDecimalStr(d.Limbs)
	var f int64
	for i := 0; i < g; i++ {
		digit := int64(0)
		if i < len(s) {
			digit = int64(s[i] - '0')
		}
		f = f*10 + digit
	}
	return w, f
}

// floorDiv4 is floor(e / 4) toward negative infinity.
func floorDiv4(e int64) int64 {
	if e >= 0 {
		return e / 4
	}
	return -((-e + 3) / 4)
}

// ============================================================================
// decimal_work group counts (spec/design/cost.md §3 "decimal_work") — an operation's work W
// in base-10^4 digit groups, computed from LOGICAL significant-digit counts, never from this
// core's internal limb count (the cross-core contract, decimal.md §7 #11). All return W >= 1;
// the evaluator charges decimal_work × (W − 1).
// ============================================================================

// decGroups is max(1, ceil(n/4)) — the base-10^4 group count of an n-digit coefficient.
func decGroups(n uint32) int64 {
	g := (int64(n) + 3) / 4
	if g < 1 {
		g = 1
	}
	return g
}

// alignedDigits is both operands' digit counts after aligning to the common scale
// max(s1, s2) (the digit count once the lower-scale coefficient is multiplied up — exactly
// the add/sub/cmp work).
func alignedDigits(a, b Decimal) (uint32, uint32) {
	s := a.Scale
	if b.Scale > s {
		s = b.Scale
	}
	return a.Precision() + (s - a.Scale), b.Precision() + (s - b.Scale)
}

// WorkLinear is W for add/sub/compare: the larger aligned operand.
func WorkLinear(a, b Decimal) int64 {
	a1, a2 := alignedDigits(a, b)
	g1, g2 := decGroups(a1), decGroups(a2)
	if g1 > g2 {
		return g1
	}
	return g2
}

// WorkMul is W for mul: the product of the (unaligned) operand group counts —
// schoolbook-quadratic.
func WorkMul(a, b Decimal) int64 {
	return decGroups(a.Precision()) * decGroups(b.Precision())
}

// WorkDiv is W for div: numerator groups (dividend digits + the rescale shift E) × divisor
// groups, E = rscale + s2 − s1 with the same selectDivScale as the result. A zero divisor
// returns 1 — the operation traps 22012 before any work (cost.md §3).
func WorkDiv(a, b Decimal) int64 {
	if b.IsZero() {
		return 1
	}
	rscale := selectDivScale(a, b)
	e := int64(rscale) + int64(b.Scale) - int64(a.Scale) // >= 0 since rscale >= s1
	return decGroups(a.Precision()+uint32(e)) * decGroups(b.Precision())
}

// WorkMod is W for mod: the aligned divmod — the product of the aligned group counts. A zero
// divisor returns 1.
func WorkMod(a, b Decimal) int64 {
	if b.IsZero() {
		return 1
	}
	a1, a2 := alignedDigits(a, b)
	return decGroups(a1) * decGroups(a2)
}

// ============================================================================
// Magnitude helpers — base 10^9, LSB-first, normalized (no high zero limbs).
// ============================================================================

func magTrim(limbs []uint32) []uint32 {
	n := len(limbs)
	for n > 0 && limbs[n-1] == 0 {
		n--
	}
	if n == 0 {
		return nil // a zero magnitude is always nil (so reflect.DeepEqual treats zeros equally)
	}
	return limbs[:n]
}

func magFromUint64(v uint64) []uint32 {
	var out []uint32
	for v != 0 {
		out = append(out, uint32(v%decBase))
		v /= decBase
	}
	return out
}

// magFromDecimalStr parses a decimal-digit string (leading zeros allowed) into LSB-first
// base-10^9 limbs.
func magFromDecimalStr(s string) []uint32 {
	digits := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		if s[i] >= '0' && s[i] <= '9' {
			digits = append(digits, s[i])
		}
	}
	var out []uint32
	end := len(digits)
	for end > 0 {
		start := end - decBaseDigits
		if start < 0 {
			start = 0
		}
		var limb uint32
		for _, c := range digits[start:end] {
			limb = limb*10 + uint32(c-'0')
		}
		out = append(out, limb)
		end = start
	}
	return magTrim(out)
}

// magToDecimalStr renders LSB-first limbs to a decimal-digit string with no leading zeros
// ("0" for zero).
func magToDecimalStr(limbs []uint32) string {
	if len(limbs) == 0 {
		return "0"
	}
	var b strings.Builder
	b.WriteString(strconv.FormatUint(uint64(limbs[len(limbs)-1]), 10))
	for i := len(limbs) - 2; i >= 0; i-- {
		s := strconv.FormatUint(uint64(limbs[i]), 10)
		b.WriteString(strings.Repeat("0", decBaseDigits-len(s)))
		b.WriteString(s)
	}
	return b.String()
}

func magDigitCount(limbs []uint32) uint32 {
	if len(limbs) == 0 {
		return 0
	}
	high := len(strconv.FormatUint(uint64(limbs[len(limbs)-1]), 10))
	return uint32(high) + uint32(len(limbs)-1)*decBaseDigits
}

func magCmp(a, b []uint32) int {
	if len(a) != len(b) {
		if len(a) < len(b) {
			return -1
		}
		return 1
	}
	for i := len(a) - 1; i >= 0; i-- {
		if a[i] != b[i] {
			if a[i] < b[i] {
				return -1
			}
			return 1
		}
	}
	return 0
}

func magAdd(a, b []uint32) []uint32 {
	n := len(a)
	if len(b) > n {
		n = len(b)
	}
	out := make([]uint32, 0, n+1)
	var carry uint64
	for i := 0; i < n; i++ {
		var x, y uint64
		if i < len(a) {
			x = uint64(a[i])
		}
		if i < len(b) {
			y = uint64(b[i])
		}
		sum := x + y + carry
		out = append(out, uint32(sum%decBase))
		carry = sum / decBase
	}
	if carry != 0 {
		out = append(out, uint32(carry))
	}
	return magTrim(out)
}

// magSub is a - b assuming a >= b.
func magSub(a, b []uint32) []uint32 {
	out := make([]uint32, 0, len(a))
	var borrow int64
	for i := 0; i < len(a); i++ {
		x := int64(a[i])
		var y int64
		if i < len(b) {
			y = int64(b[i])
		}
		diff := x - y - borrow
		if diff < 0 {
			diff += int64(decBase)
			borrow = 1
		} else {
			borrow = 0
		}
		out = append(out, uint32(diff))
	}
	return magTrim(out)
}

func magMul(a, b []uint32) []uint32 {
	if len(a) == 0 || len(b) == 0 {
		return nil
	}
	out := make([]uint64, len(a)+len(b))
	for i, ai := range a {
		var carry uint64
		for j, bj := range b {
			cur := out[i+j] + uint64(ai)*uint64(bj) + carry
			out[i+j] = cur % decBase
			carry = cur / decBase
		}
		out[i+len(b)] += carry
	}
	res := make([]uint32, len(out))
	for i, x := range out {
		res[i] = uint32(x)
	}
	return magTrim(res)
}

// magMulSmall multiplies a magnitude by a single small factor s (0 <= s < BASE).
func magMulSmall(a []uint32, s uint64) []uint32 {
	if s == 0 || len(a) == 0 {
		return nil
	}
	out := make([]uint32, 0, len(a)+1)
	var carry uint64
	for _, ai := range a {
		cur := uint64(ai)*s + carry
		out = append(out, uint32(cur%decBase))
		carry = cur / decBase
	}
	for carry != 0 {
		out = append(out, uint32(carry%decBase))
		carry /= decBase
	}
	return magTrim(out)
}

// magMulPow10 multiplies by 10^e.
func magMulPow10(a []uint32, e uint32) []uint32 {
	if len(a) == 0 || e == 0 {
		return append([]uint32(nil), a...)
	}
	full := int(e / decBaseDigits)
	rem := e % decBaseDigits
	shifted := make([]uint32, full, full+len(a)+1)
	shifted = append(shifted, a...) // prepend `full` zero limbs = *BASE^full
	if rem > 0 {
		shifted = magMulSmall(shifted, pow10(rem))
	}
	return magTrim(shifted)
}

// magPow10 is 10^e as a magnitude.
func magPow10(e uint32) []uint32 { return magMulPow10([]uint32{1}, e) }

func pow10(n uint32) uint64 {
	r := uint64(1)
	for i := uint32(0); i < n; i++ {
		r *= 10
	}
	return r
}

// magDivMod is long division: (quotient, remainder) of num / den (den != 0). Each quotient
// limb is found by binary search in [0, BASE) — boring and identical across cores.
func magDivMod(num, den []uint32) ([]uint32, []uint32) {
	if magCmp(num, den) < 0 {
		return nil, append([]uint32(nil), num...)
	}
	quo := make([]uint32, len(num))
	var rem []uint32
	for i := len(num) - 1; i >= 0; i-- {
		// rem = rem*BASE + num[i] (shift up one limb, set the low limb).
		rem = append([]uint32{num[i]}, rem...)
		rem = magTrim(rem)
		lo, hi := uint64(0), decBase-1
		for lo < hi {
			mid := (lo + hi + 1) / 2
			if magCmp(magMulSmall(den, mid), rem) <= 0 {
				lo = mid
			} else {
				hi = mid - 1
			}
		}
		quo[i] = uint32(lo)
		rem = magSub(rem, magMulSmall(den, lo))
	}
	return magTrim(quo), rem
}

// magToNbase4 converts LSB-first base-10^9 limbs to MS-first base-10^4 groups. Zero → empty.
func magToNbase4(limbs []uint32) []uint16 {
	if len(limbs) == 0 {
		return nil
	}
	s := magToDecimalStr(limbs)
	pad := (4 - len(s)%4) % 4
	padded := strings.Repeat("0", pad) + s
	out := make([]uint16, 0, len(padded)/4)
	for i := 0; i < len(padded); i += 4 {
		var g uint16
		for _, c := range padded[i : i+4] {
			g = g*10 + uint16(c-'0')
		}
		out = append(out, g)
	}
	return out
}

// magFromNbase4 converts MS-first base-10^4 groups to LSB-first base-10^9 limbs.
func magFromNbase4(groups []uint16) []uint32 {
	if len(groups) == 0 {
		return nil
	}
	var b strings.Builder
	b.WriteString(strconv.FormatUint(uint64(groups[0]), 10))
	for _, g := range groups[1:] {
		s := strconv.FormatUint(uint64(g), 10)
		b.WriteString(strings.Repeat("0", 4-len(s)))
		b.WriteString(s)
	}
	return magFromDecimalStr(b.String())
}
