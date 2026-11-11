package jed

import (
	"fmt"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

// The 128-bit fast paths (decimal.go) must give exactly what the limb routines give — same
// sign, scale and limbs, nil for zero — for every operation that has one, across the magnitudes
// where a path is taken, declined, or overflows part-way.

// smallTestMags are coefficient digit strings around every boundary the fast paths care about:
// limb counts (10^9, 10^18, 10^27, 10^36), the co-allocated box (10^18), a uint64 (2^64), the codec's
// one-integer decode (10^16), and a u128 (10^38, 2^127, 2^128), plus values well past it.
func smallTestMags() []string {
	mags := []string{
		"0", "1", "5", "9", "10", "49", "50", "51", "99", "100", "9999", "10000", "10001",
		"999999999", "1000000000", "1000000001",
		"9999999999999999", "10000000000000000", "10000000000000001",
		"999999999999999999", "1000000000000000000", "1000000000000000001",
		"18446744073709551615", "18446744073709551616", "18446744073709551617",
		"999999999999999999999999999", "1000000000000000000000000000",
		"999999999999999999999999999999999999", "1000000000000000000000000000000000000",
		"99999999999999999999999999999999999999", "100000000000000000000000000000000000000",
		"170141183460469231731687303715884105727", "170141183460469231731687303715884105728",
		"340282366920938463463374607431768211455", "340282366920938463463374607431768211456",
		"340282366920938463463374607431768211457",
		"1000000000000000000000000000000000000000", "123456789012345678901234567890123456789012345",
		"5000000000000000000", "4999999999999999999", "5000000000000000001",
		// Multiplicands whose 128-bit product overflows only in the final carry.
		"36893488147419103231", "36893488147419103232", "79228162514264337593543950335",
		"1329227995784915872903807060280344575", "3402823669209384634", "34028236692093846346337460743176821",
		"123456789", "1234567890123", "9007199254740993", "1" + strings.Repeat("0", 60),
	}
	return mags
}

var smallTestScales = []uint32{0, 1, 2, 4, 9, 17, 18, 19, 20, 36, 37, 38, 39, 40}

func smallTestValues() []Decimal {
	var out []Decimal
	for _, m := range smallTestMags() {
		limbs := magFromDecimalStr(m)
		for _, s := range smallTestScales {
			out = append(out, newDecimal(false, s, limbs), newDecimal(true, s, limbs))
		}
	}
	return out
}

func sameDecimal(t *testing.T, what string, got, want Decimal) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("%s: fast path %#v, limb path %#v", what, got, want)
	}
	if len(got.Limbs) == 0 && (got.Neg || got.Limbs != nil) {
		t.Fatalf("%s: zero is not canonical: %#v", what, got)
	}
	if n := len(got.Limbs); n > 0 && got.Limbs[n-1] == 0 {
		t.Fatalf("%s: a high zero limb: %#v", what, got)
	}
}

func TestSmallU128RoundTripsLimbs(t *testing.T) {
	for _, m := range smallTestMags() {
		limbs := magFromDecimalStr(m)
		v, ok := magToU128(limbs)
		if ok != (len(limbs) <= smallMaxLimbs) {
			t.Fatalf("%s: magToU128 ok=%v for %d limbs", m, ok, len(limbs))
		}
		if !ok {
			continue
		}
		if back := u128ToMag(v); !reflect.DeepEqual(back, limbs) {
			t.Fatalf("%s: u128ToMag(magToU128) = %v, want %v", m, back, limbs)
		}
	}
	// Every u128, not only those of four limbs, lowers to canonical limbs.
	for _, v := range []u128{{0, 0}, {0, 1}, {0, ^uint64(0)}, {1, 0}, {1 << 63, 0}, {^uint64(0), ^uint64(0)}} {
		limbs := u128ToMag(v)
		want := magAdd(magMul(magFromUint64(v.hi), magFromDecimalStr("18446744073709551616")), magFromUint64(v.lo))
		if !reflect.DeepEqual(limbs, want) {
			t.Fatalf("u128ToMag(%v) = %v, want %v", v, limbs, want)
		}
	}
	for e := range pow10U128 {
		if got, want := u128ToMag(pow10U128[e]), magPow10(uint32(e)); !reflect.DeepEqual(got, want) {
			t.Fatalf("pow10U128[%d] = %v, want %v", e, got, want)
		}
	}
}

func TestSmallAddAndCompareMatchLimbPaths(t *testing.T) {
	vals := smallTestValues()
	taken, declined := 0, 0
	for i, a := range vals {
		// Every value against a spread of the others (the full square is ~1.5M pairs).
		for j := i % 7; j < len(vals); j += 7 {
			b := vals[j]
			what := fmt.Sprintf("%s , %s", a.Render(), b.Render())
			if got, ok := addSmall(a, b); ok {
				taken++
				sameDecimal(t, "add "+what, got, a.addLimbs(b))
			} else {
				declined++
			}
			sameDecimal(t, "AddUncapped "+what, a.AddUncapped(b), a.addLimbs(b))
			if got, ok := cmpSmall(a, b); ok && got != a.cmpMagLimbs(b) {
				t.Fatalf("cmp %s: fast path %d, limb path %d", what, got, a.cmpMagLimbs(b))
			}
			// CmpValue by the limb path alone.
			want := a.cmpMagLimbs(b)
			switch {
			case a.Neg != b.Neg && a.Neg:
				want = -1
			case a.Neg != b.Neg:
				want = 1
			case a.Neg:
				want = -want
			}
			if got := a.CmpValue(b); got != want {
				t.Fatalf("CmpValue %s: %d, want %d", what, got, want)
			}
		}
	}
	if taken == 0 || declined == 0 {
		t.Fatalf("the battery must take and decline the fast path: taken %d, declined %d", taken, declined)
	}
}

func TestSmallMulMatchesLimbPath(t *testing.T) {
	vals := smallTestValues()
	taken, declined := 0, 0
	for i, a := range vals {
		for j := i % 11; j < len(vals); j += 11 {
			b := vals[j]
			want := magMul(a.Limbs, b.Limbs)
			if got, ok := mulSmall(a, b); ok {
				taken++
				if !reflect.DeepEqual(got, want) {
					t.Fatalf("mul %s × %s: fast path %v, limb path %v", a.Render(), b.Render(), got, want)
				}
			} else {
				declined++
			}
			// Mul end to end against its definition over the limb product (round past the scale cap,
			// then the format cap).
			exact := newDecimal(a.Neg != b.Neg, a.Scale+b.Scale, want)
			if exact.Scale > maxScale {
				exact = exact.RoundToScale(maxScale)
			}
			wantD, wantErr := exact.CheckCap()
			gotD, gotErr := a.Mul(b)
			if (gotErr == nil) != (wantErr == nil) {
				t.Fatalf("Mul %s × %s: err %v, want %v", a.Render(), b.Render(), gotErr, wantErr)
			}
			if gotErr == nil {
				sameDecimal(t, "Mul", gotD, wantD)
			}
		}
	}
	if taken == 0 || declined == 0 {
		t.Fatalf("the battery must take and decline the fast path: taken %d, declined %d", taken, declined)
	}
}

func TestSmallCodecMatchesLimbPath(t *testing.T) {
	for _, m := range smallTestMags() {
		limbs := magFromDecimalStr(m)
		if got, want := magDigitCount(limbs), uint32(len(strings.TrimLeft(m, "0"))); got != want {
			t.Fatalf("%s: magDigitCount %d, want %d", m, got, want)
		}
		groups := magToNbase4Limbs(limbs)
		if len(limbs) == 0 {
			groups = nil
		}
		if got := magToNbase4(limbs); !reflect.DeepEqual(got, groups) {
			t.Fatalf("%s: magToNbase4 %v, limb path %v", m, got, groups)
		}
		if got := magFromNbase4(groups); !reflect.DeepEqual(got, limbs) {
			t.Fatalf("%s: magFromNbase4 %v, want %v", m, got, limbs)
		}
		if len(groups) > 0 {
			if got, want := magFromNbase4(groups), magFromNbase4Limbs(groups); !reflect.DeepEqual(got, want) {
				t.Fatalf("%s: magFromNbase4 %v, limb path %v", m, got, want)
			}
		}
		// The stored body, both ways round: the one-integer decode (at most four groups), the stack
		// groups (at most nine), and the general path must all rebuild the value its codec form names.
		for _, s := range smallTestScales {
			for _, neg := range []bool{false, true} {
				d := newDecimal(neg, s, limbs)
				body := encodeDecimalBody(d, nil)
				pos := 0
				v, err := decodeDecimalBody(body, &pos, decodeConstruct)
				if err != nil || pos != len(body) {
					t.Fatalf("%s: decode: %v (consumed %d of %d)", d.Render(), err, pos, len(body))
				}
				cn, cs, cg := d.ToCodec()
				sameDecimal(t, "decode "+d.Render(), *v.decimal(), newDecimal(cn, cs, magFromNbase4Limbs(orEmptyGroups(cg))))
				sameDecimal(t, "codec round trip "+d.Render(), *v.decimal(), d)
			}
		}
	}
}

// orEmptyGroups keeps magFromNbase4Limbs (which indexes its first group) off the empty zero form.
func orEmptyGroups(g []uint16) []uint16 {
	if len(g) == 0 {
		return []uint16{0}
	}
	return g
}

func TestSmallIntegerAndBoxConstructorsMatch(t *testing.T) {
	for _, v := range []int64{
		0, 1, -1, 9, 999999999, 1000000000, -1000000000, 999999999999999999, 1000000000000000000,
		9223372036854775807, -9223372036854775807, -9223372036854775808,
	} {
		want := newDecimal(v < 0, 0, magFromDecimalStr(strings.TrimPrefix(strconv.FormatInt(v, 10), "-")))
		sameDecimal(t, fmt.Sprint("decimalFromInt64 ", v), decimalFromInt64(v), want)
		var buf [3]uint32
		sameDecimal(t, fmt.Sprint("decimalFromInt64Buf ", v), decimalFromInt64Buf(v, &buf), want)
	}
	for _, u := range []uint64{0, 1, 999999999, 1000000000, 1000000001, 9999999999999999, 999999999999999999} {
		for _, s := range []uint32{0, 2, 18} {
			for _, neg := range []bool{false, true} {
				want := newDecimal(neg, s, magFromDecimalStr(strconv.FormatUint(u, 10)))
				sameDecimal(t, fmt.Sprint("decimalValueU64 ", u), *decimalValueU64(neg, s, u).decimal(), want)
			}
		}
	}
}
