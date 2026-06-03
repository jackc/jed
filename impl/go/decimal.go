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
	// MaxPrecision is the max total significant digits — spec/types/scalars.toml max_precision.
	MaxPrecision = 1000
	// MaxScale is the max digits after the point — spec/types/scalars.toml max_scale.
	MaxScale = 1000
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

// IsZero reports whether the value is zero.
func (d Decimal) IsZero() bool { return len(d.Limbs) == 0 }

// Precision is the number of significant digits in the coefficient (0 for zero).
func (d Decimal) Precision() uint32 { return magDigitCount(d.Limbs) }

// CheckCap traps 22003 if this (unconstrained) value exceeds the precision/scale caps.
func (d Decimal) CheckCap() (Decimal, error) {
	if d.Precision() > MaxPrecision || d.Scale > MaxScale {
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

// Add is exact addition, result scale max(s1,s2); traps 22003 at the cap.
func (d Decimal) Add(o Decimal) (Decimal, error) {
	s := d.Scale
	if o.Scale > s {
		s = o.Scale
	}
	a := magMulPow10(d.Limbs, s-d.Scale)
	b := magMulPow10(o.Limbs, s-o.Scale)
	var r Decimal
	if d.Neg == o.Neg {
		r = newDecimal(d.Neg, s, magAdd(a, b))
	} else {
		switch magCmp(a, b) {
		case 0:
			r = DecimalZero(s)
		case 1:
			r = newDecimal(d.Neg, s, magSub(a, b))
		default:
			r = newDecimal(o.Neg, s, magSub(b, a))
		}
	}
	return r.CheckCap()
}

// Sub is d - o (= d + (-o)).
func (d Decimal) Sub(o Decimal) (Decimal, error) { return d.Add(o.Negate()) }

// Mul is exact multiplication, result scale s1+s2; traps 22003 at the cap.
func (d Decimal) Mul(o Decimal) (Decimal, error) {
	return newDecimal(d.Neg != o.Neg, d.Scale+o.Scale, magMul(d.Limbs, o.Limbs)).CheckCap()
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

// ToInt64Round rounds to an integer (scale 0, half away) and returns it as int64 if it fits,
// else ok=false (the decimal→int cast; the caller range-checks the target int type).
func (d Decimal) ToInt64Round() (int64, bool) {
	r := d.RoundToScale(0)
	if len(r.Limbs) > 3 { // > 27 digits, far beyond int64
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
	if rscale > MaxScale {
		rscale = MaxScale
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
