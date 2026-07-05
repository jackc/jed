package jed

// Cross-check: the Go range key codec (encodeRangeKey, spec/design/encoding.md §2.11) must produce
// the byte-exact, order-preserving vectors the Rust/TS cores and the Ruby reference reproduce
// (CLAUDE.md §8). Range is the first container key — empty/±∞/inclusivity framing around the
// element's own key. The behavioral side (a range PRIMARY KEY/index/UNIQUE/FK works) lives in
// types/range.test; this is the encoding contract. Test-only.

import (
	"bytes"
	"encoding/hex"
	"testing"
)

// a canonical i32range from optional finite bounds (discrete [) form: lower inclusive, upper
// exclusive — what the engine stores); nil is an infinite bound.
func i32rangeVal(lo, hi *int64) *RangeVal {
	rv := &RangeVal{}
	if lo != nil {
		v := IntValue(*lo)
		rv.Lower = &v
		rv.LowerInc = true
	}
	if hi != nil {
		v := IntValue(*hi)
		rv.Upper = &v
	}
	return rv
}

func TestEncodeRangeKeyI32ByteExact(t *testing.T) {
	t.Parallel()
	p := func(n int64) *int64 { return &n }
	enc := func(rv *RangeVal) string { return hex.EncodeToString(encodeRangeKey(scalarInt32, rv)) }
	if got := enc(emptyRangeVal()); got != "00" {
		t.Fatalf("empty: got %s want 00", got)
	}
	cases := []struct {
		rv   *RangeVal
		want string
	}{
		{i32rangeVal(nil, p(5)), "0100018000000500"},            // (,5)
		{i32rangeVal(nil, nil), "010002"},                       // (,)
		{i32rangeVal(p(1), p(5)), "01018000000100018000000500"}, // [1,5) — §2.11 worked example
		{i32rangeVal(p(2), nil), "0101800000020002"},            // [2,)
	}
	for _, c := range cases {
		if got := enc(c.rv); got != c.want {
			t.Fatalf("encode: got %s want %s", got, c.want)
		}
	}
}

func TestEncodeRangeKeyOrderPreserving(t *testing.T) {
	t.Parallel()
	p := func(n int64) *int64 { return &n }
	// a strictly ascending sequence under rangeTotalCmp
	ranges := []*RangeVal{
		emptyRangeVal(),
		i32rangeVal(nil, p(5)),  // (,5)
		i32rangeVal(nil, nil),   // (,)
		i32rangeVal(p(1), p(5)), // [1,5)
		i32rangeVal(p(1), p(10)),
		i32rangeVal(p(2), p(4)),
		i32rangeVal(p(2), nil), // [2,)
	}
	for i := 1; i < len(ranges); i++ {
		a := encodeRangeKey(scalarInt32, ranges[i-1])
		b := encodeRangeKey(scalarInt32, ranges[i])
		if bytes.Compare(a, b) >= 0 {
			t.Fatalf("keys not strictly ascending at %d: %x !< %x", i, a, b)
		}
	}
}

func TestEncodeRangeKeyInclusivityAndScale(t *testing.T) {
	t.Parallel()
	dec := func(digits string, scale uint32) *Value {
		v := DecimalValue(decimalFromDigitsScale(false, digits, scale))
		return &v
	}
	numr := func(lo, hi *Value, loInc, hiInc bool) *RangeVal {
		return &RangeVal{Lower: lo, Upper: hi, LowerInc: loInc, UpperInc: hiInc}
	}
	encN := func(rv *RangeVal) []byte { return encodeRangeKey(scalarDecimal, rv) }
	one, two := dec("1", 0), dec("2", 0)
	// [1,2) < (1,2)  (inclusive lower before exclusive lower)
	if bytes.Compare(encN(numr(one, two, true, false)), encN(numr(one, two, false, false))) >= 0 {
		t.Fatal("[1,2) must sort before (1,2)")
	}
	// (1,2) < (1,2]  (exclusive upper before inclusive upper)
	if bytes.Compare(encN(numr(one, two, false, false)), encN(numr(one, two, false, true))) >= 0 {
		t.Fatal("(1,2) must sort before (1,2]")
	}
	// decimal scale-independence: [1.5,2) and [1.50,2) share a key (§2.5 wrinkle)
	a := encN(numr(dec("15", 1), two, true, false))
	b := encN(numr(dec("150", 2), two, true, false))
	if !bytes.Equal(a, b) {
		t.Fatalf("scale-equal bounds must share a key: %x != %x", a, b)
	}
}
