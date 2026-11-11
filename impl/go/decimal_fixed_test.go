package jed

import (
	"bytes"
	"math"
	"path/filepath"
	"testing"
)

// Fixed-width stored numeric(p,s) (spec/fileformat/format.md v51, spec/design/decimal.md §6a): the
// parts the corpus cannot reach — the column class a typmod selects, the slot bytes, the helpers'
// edges, and the forms that stay on the single-value codec. The SQL behavior is
// types/decimal_fixed_width.test; the on-disk bytes are the decimal_fixed_table.jed golden.

// A numeric(p,s) column's stored class follows its precision; every other decimal stays variable.
func TestFixedDecimalColumnClass(t *testing.T) {
	col := func(p, s uint16) *catColumn {
		c := &catColumn{Name: "c", Type: scalarT(scalarDecimal)}
		if p != 0 {
			c.Decimal = &decimalTypmod{Precision: p, Scale: s}
		}
		return c
	}
	for _, tc := range []struct {
		p, s  uint16
		width int
	}{
		{0, 0, 0}, {1, 0, 2}, {4, 4, 2}, {5, 0, 4}, {9, 3, 4}, {10, 2, 8}, {18, 18, 8}, {19, 2, 0}, {1000, 0, 0},
	} {
		ct := storeColType(col(tc.p, tc.s), nil)
		w, fixed := fixedValueWidth(ct)
		if w != tc.width || fixed != (tc.width != 0) {
			t.Fatalf("numeric(%d,%d): width %d fixed %v, want width %d", tc.p, tc.s, w, fixed, tc.width)
		}
		if fixed == isSpillable(ct) {
			t.Fatalf("numeric(%d,%d): fixed-width and spillable must be complements", tc.p, tc.s)
		}
		if !ct.Scalar.IsDecimal() || ct.Composite || ct.Elem != nil || ct.RangeElem != nil {
			t.Fatalf("numeric(%d,%d): the stored type is no longer a scalar decimal: %+v", tc.p, tc.s, ct)
		}
		if fixed && ct.DecScale != tc.s {
			t.Fatalf("numeric(%d,%d): slot scale %d", tc.p, tc.s, ct.DecScale)
		}
		// The single-value form (a statistics value, format.md "Value codec") is never a slot.
		if _, f := fixedValueWidth(ct.valueType()); f {
			t.Fatalf("numeric(%d,%d): the single-value type is fixed-width", tc.p, tc.s)
		}
	}
	// A non-decimal column never takes a slot from a stray typmod, and a width or scale change is a
	// different leaf codec.
	if ct := storeColType(&catColumn{Name: "c", Type: scalarT(scalarInt32)}, nil); ct.DecSlot != 0 {
		t.Fatalf("an i32 column carries a decimal slot: %+v", ct)
	}
	a, b, c := storeColType(col(12, 2), nil), storeColType(col(12, 3), nil), storeColType(col(9, 2), nil)
	if !sameLeafShape([]colType{a}, []colType{storeColType(col(18, 2), nil)}) {
		t.Fatal("numeric(12,2) and numeric(18,2) share a slot width and scale")
	}
	if sameLeafShape([]colType{a}, []colType{b}) || sameLeafShape([]colType{a}, []colType{c}) ||
		sameLeafShape([]colType{a}, []colType{scalarColType(scalarDecimal)}) {
		t.Fatal("a different scale, width, or the variable form is a different leaf codec")
	}
}

// The slot is the unscaled integer in the integer body of the slot's width, and reads back at the
// column's scale; every bit pattern is a value.
func TestFixedDecimalSlotCodec(t *testing.T) {
	for _, tc := range []struct {
		width uint8
		scale uint16
		in    string
		slot  []byte
		out   string
	}{
		{2, 2, "99.99", []byte{0xA7, 0x0F}, "99.99"},
		{2, 2, "-99.99", []byte{0x58, 0xF1}, "-99.99"},
		{2, 2, "0", []byte{0x80, 0x00}, "0.00"},
		{2, 2, "1.5", []byte{0x80, 0x96}, "1.50"}, // a lower scale is scaled up exactly
		{4, 0, "-42", []byte{0x7F, 0xFF, 0xFF, 0xD6}, "-42"},
		{4, 3, "999999.999", []byte{0xBB, 0x9A, 0xC9, 0xFF}, "999999.999"},
		{8, 6, "-1.5", []byte{0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xE9, 0x1C, 0xA0}, "-1.500000"},
		{8, 0, "999999999999999999", []byte{0x8D, 0xE0, 0xB6, 0xB3, 0xA7, 0x63, 0xFF, 0xFF}, "999999999999999999"},
	} {
		ct := colType{Scalar: scalarDecimal, DecSlot: tc.width, DecScale: tc.scale}
		enc := encodeValue(ct, DecimalValue(dec(tc.in)))
		if enc[0] != 0x00 || !bytes.Equal(enc[1:], tc.slot) {
			t.Fatalf("%s at numeric slot %d/%d: encoded % x, want 00 % x", tc.in, tc.width, tc.scale, enc, tc.slot)
		}
		pos := 0
		v, err := readInlineBody(ct, tc.slot, &pos, decodeConstruct)
		if err != nil || pos != len(tc.slot) || v.Kind != ValDecimal || v.decimal().Render() != tc.out {
			t.Fatalf("%s: decoded %v (pos %d, err %v), want %s", tc.in, v, pos, err, tc.out)
		}
		if got := encodeValue(ct, v); !bytes.Equal(got, enc) {
			t.Fatalf("%s: re-encoded % x, want % x", tc.in, got, enc)
		}
	}
	if got := encodeValue(colType{Scalar: scalarDecimal, DecSlot: 4, DecScale: 1}, NullValue()); !bytes.Equal(got, []byte{0x01}) {
		t.Fatalf("NULL encoded % x", got)
	}
	// Any bit pattern reads as a value, a magnitude past 10^18 (three limbs) included.
	for _, u := range []int64{math.MinInt64, math.MinInt64 + 1, -1_000_000_000_000_000_000, 1_000_000_000_000_000_000, math.MaxInt64} {
		v := decimalValueInt64(u, 3)
		back, ok := v.decimal().unscaledInt64(3)
		if !ok || back != u || v.decimal().Scale != 3 {
			t.Fatalf("slot %d: read back %d (ok %v) at scale %d", u, back, ok, v.decimal().Scale)
		}
		want := decimalFromInt64(u)
		want.Scale = 3
		if v.decimal().CmpValue(want) != 0 || v.decimal().Neg != (u < 0) {
			t.Fatalf("slot %d decoded to %s", u, v.decimal().Render())
		}
	}
}

// unscaledInt64 refuses what store-coercion has not brought to the column's typmod, and the encoder
// fails loudly instead of truncating.
func TestFixedDecimalRejectsUncoercedValues(t *testing.T) {
	for _, tc := range []struct {
		in    string
		scale uint32
	}{
		{"1.234", 2},                             // more fractional digits than the scale
		{"9223372036854775808", 0},               // one past MaxInt64
		{"-9223372036854775809", 0},              // one past MinInt64
		{"922337203685477580.8", 2},              // overflows only once scaled up
		{"1" + "000000000000000000000000000", 0}, // four limbs
	} {
		if u, ok := dec(tc.in).unscaledInt64(tc.scale); ok {
			t.Fatalf("%s at scale %d: accepted as %d", tc.in, tc.scale, u)
		}
	}
	if u, ok := dec("-9223372036854775808").unscaledInt64(0); !ok || u != math.MinInt64 {
		t.Fatalf("MinInt64: %d %v", u, ok)
	}
	if u, ok := dec("0.000").unscaledInt64(5); !ok || u != 0 {
		t.Fatalf("zero: %d %v", u, ok)
	}
	mustPanic := func(what string, ct colType, v Value) {
		t.Helper()
		defer func() {
			if recover() == nil {
				t.Fatalf("%s: the encoder wrote a value outside the column's typmod", what)
			}
		}()
		encodeValue(ct, v)
	}
	mustPanic("scale", colType{Scalar: scalarDecimal, DecSlot: 8, DecScale: 2}, DecimalValue(dec("1.234")))
	mustPanic("slot range", colType{Scalar: scalarDecimal, DecSlot: 2, DecScale: 2}, DecimalValue(dec("400.00")))
	mustPanic("kind", colType{Scalar: scalarDecimal, DecSlot: 4, DecScale: 0}, IntValue(7))
}

// A persisted statistics value of a fixed-width decimal column is a single value, so it keeps the
// self-describing decimal codec (format.md "Value codec") and survives a reopen; the leaf beside it
// holds slots.
func TestFixedDecimalStatisticsStaySingleValue(t *testing.T) {
	path := filepath.Join(t.TempDir(), "stats.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	run(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, a numeric(4,2), b numeric(9,0), c numeric(18,6))")
	run(t, s, "INSERT INTO t SELECT g, (g % 7)::numeric / 4, g % 5, (g % 11)::numeric / 8 FROM generate_series(1, 400) AS g")
	run(t, s, "ANALYZE t")
	const q = "SELECT sum(a)::text, sum(b)::text, sum(c)::text FROM t"
	want := queryRows(t, s, q)
	stats := queryRows(t, s, "SELECT count(*) FROM jed_statistics")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = OpenDatabase(path)
	if err != nil {
		t.Fatalf("reopen after ANALYZE: %v", err)
	}
	defer db.Close()
	s = db.Session(SessionOptions{})
	if got := queryRows(t, s, q); got[0][0].str() != want[0][0].str() || got[0][2].str() != want[0][2].str() {
		t.Fatalf("after reopen: %v, want %v", got, want)
	}
	if got := queryRows(t, s, "SELECT count(*) FROM jed_statistics"); got[0][0].Int != stats[0][0].Int || got[0][0].Int == 0 {
		t.Fatalf("statistics after reopen: %v, before %v", got, stats)
	}
	// The stored statistics values are the variable-width codec's bytes, not slots.
	ct := colType{Scalar: scalarDecimal, DecSlot: 2, DecScale: 2}
	v := DecimalValue(dec("1.25"))
	if single, slot := encodeValue(ct.valueType(), v), encodeValue(ct, v); bytes.Equal(single, slot) ||
		!bytes.Equal(single, encodeValue(scalarColType(scalarDecimal), v)) {
		t.Fatalf("single-value form % x, slot form % x", single, slot)
	}
}
