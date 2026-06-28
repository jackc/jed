package jed

// f32 / f64 — the IEEE 754 binary float types (spec/design/float.md). These per-feature
// Go tests stand in for the (not-yet-authored) shared float conformance suite this slice precedes:
// the value codec, the total order, the trap model, the promotion tower, the casts, the
// order-independent canonical-fold SUM/AVG, and one transcendental.

import (
	"math"
	"testing"
)

// --- value codec round-trip (both widths, incl -0 / NaN / ±Inf) ---------------------------

func TestFloatValueCodecRoundTrip(t *testing.T) {
	cases64 := []float64{
		0, math.Copysign(0, -1), 1, -1, 1.5, -2.25, 1e308, -1e-308,
		math.MaxFloat64, math.SmallestNonzeroFloat64,
		math.Inf(1), math.Inf(-1), math.NaN(),
	}
	for _, f := range cases64 {
		v := Float64Value(f)
		enc := encodeValue(scalarColType(scalarFloat64), v)
		pos := 0
		got, err := readInlineBody(scalarColType(scalarFloat64), enc[1:], &pos) // skip the 0x00 presence tag
		if err != nil {
			t.Fatalf("f64 decode %v: %v", f, err)
		}
		if got.Kind != ValFloat64 {
			t.Fatalf("f64 %v: kind %v", f, got.Kind)
		}
		// Compare by BITS. Storage preserves the original pattern verbatim (incl -0's sign bit) EXCEPT
		// for NaN, which canonicalizes to the single quiet pattern 0x7FF8…000 (float.md §10).
		// Float64Value stashes bits in Int, so == on Int is a bit compare.
		want := math.Float64bits(f)
		if math.IsNaN(f) {
			want = 0x7FF8000000000000
		}
		if uint64(got.Int) != want {
			t.Errorf("f64 %v: bits %#016x != %#016x", f, uint64(got.Int), want)
		}
	}
	cases32 := []float32{
		0, float32(math.Copysign(0, -1)), 1, -1, 1.5, -2.25,
		math.MaxFloat32, float32(math.Inf(1)), float32(math.Inf(-1)), float32(math.NaN()),
	}
	for _, f := range cases32 {
		v := Float32Value(f)
		enc := encodeValue(scalarColType(scalarFloat32), v)
		pos := 0
		got, err := readInlineBody(scalarColType(scalarFloat32), enc[1:], &pos)
		if err != nil {
			t.Fatalf("f32 decode %v: %v", f, err)
		}
		want := math.Float32bits(f)
		if math.IsNaN(float64(f)) {
			want = 0x7FC00000 // NaN canonicalizes on store (float.md §10)
		}
		if got.Kind != ValFloat32 || uint32(got.Int) != want {
			t.Errorf("f32 %v: bits %#08x != %#08x", f, uint32(got.Int), want)
		}
	}
}

func TestFloatImageRoundTrip(t *testing.T) {
	// The on-disk single-file image (the §8 cross-core round-trip contract) preserves float bits
	// verbatim — incl -0 / ±Inf — across a serialize + reload (a NaN canonicalizes to one quiet
	// pattern but stays a NaN; float.md §10).
	db := dbWith(
		t,
		"CREATE TABLE f (id i32 PRIMARY KEY, a f32, b f64)",
		"INSERT INTO f VALUES (1, '1.5', 'NaN')",
		"INSERT INTO f VALUES (2, '-0', '-Infinity')",
		"INSERT INTO f VALUES (3, 'Infinity', '3.141592653589793')",
	)
	img, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatal(err)
	}
	db2, err := loadEngine(img)
	if err != nil {
		t.Fatal(err)
	}
	rows := query(t, db2, "SELECT a, b FROM f ORDER BY id")
	if len(rows) != 3 {
		t.Fatalf("got %d rows", len(rows))
	}
	if rows[0][0].F32() != 1.5 || !math.IsNaN(rows[0][1].F64()) {
		t.Errorf("row 1: a=%v b=%v", rows[0][0].Render(), rows[0][1].Render())
	}
	if rows[1][0].F32() != 0 || !math.Signbit(float64(rows[1][0].F32())) || !math.IsInf(rows[1][1].F64(), -1) {
		t.Errorf("row 2: a=%v b=%v (want -0, -Infinity)", rows[1][0].Render(), rows[1][1].Render())
	}
	if !math.IsInf(float64(rows[2][0].F32()), 1) {
		t.Errorf("row 3: a=%v, want Infinity", rows[2][0].Render())
	}
}

func TestFloatStoredNegZeroPreservesBits(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE f (id i32 PRIMARY KEY, x f64)",
		"INSERT INTO f VALUES (1, '-0')",
		"INSERT INTO f VALUES (2, '0')",
	)
	// -0 renders "-0", +0 renders "0" — the bits round-tripped through storage.
	if got := query(t, db, "SELECT x FROM f WHERE id = 1")[0][0].Render(); got != "-0" {
		t.Errorf("stored -0 rendered %q, want -0", got)
	}
	if got := query(t, db, "SELECT x FROM f WHERE id = 2")[0][0].Render(); got != "0" {
		t.Errorf("stored +0 rendered %q, want 0", got)
	}
}

// --- the total order (NaN largest, -0 == +0, NaN = NaN TRUE) -------------------------------

func TestFloatTotalOrderComparator(t *testing.T) {
	nan := math.NaN()
	ninf, pinf := math.Inf(-1), math.Inf(1)
	negz, posz := math.Copysign(0, -1), 0.0
	checks := []struct {
		a, b float64
		want int
	}{
		{ninf, -1, -1},
		{-1, pinf, -1},
		{pinf, nan, -1}, // -inf < finite < +inf < NaN
		{nan, nan, 0},   // all NaNs equal
		{negz, posz, 0}, // -0 == +0
		{nan, pinf, 1},  // NaN largest
		{1.0, 1.0, 0},
		{-2.0, -1.0, -1},
	}
	for _, c := range checks {
		if got := floatTotalCmp(c.a, c.b); got != c.want {
			t.Errorf("floatTotalCmp(%v,%v) = %d, want %d", c.a, c.b, got, c.want)
		}
	}
}

func TestFloatDistinctCollapsesNaNAndZeroSigns(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE f (id i32 PRIMARY KEY, x f64)",
		"INSERT INTO f VALUES (1, 'NaN')",
		"INSERT INTO f VALUES (2, 'NaN')",
		"INSERT INTO f VALUES (3, '-0')",
		"INSERT INTO f VALUES (4, '0')",
	)
	rows := query(t, db, "SELECT DISTINCT x FROM f")
	// Two NaNs collapse to one bucket; -0 and +0 collapse to one bucket → 2 distinct values.
	if len(rows) != 2 {
		t.Errorf("DISTINCT over {NaN,NaN,-0,+0} = %d rows, want 2", len(rows))
	}
}

// --- the promotion tower: f32 + f64 → f64 -------------------------------------

func TestFloatMixedWidthArithmeticPromotes(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE f (id i32 PRIMARY KEY, a f32, b f64)",
		"INSERT INTO f VALUES (1, '1.5', '2.25')",
	)
	out, err := execute(db, "SELECT a + b FROM f")
	if err != nil {
		t.Fatal(err)
	}
	if out.ColumnTypes[0] != "f64" {
		t.Errorf("f32 + f64 result type = %s, want f64", out.ColumnTypes[0])
	}
	if got := out.Rows[0][0]; got.Kind != ValFloat64 || got.F64() != 3.75 {
		t.Errorf("1.5 + 2.25 = %v, want 3.75 (f64)", got.Render())
	}
}

func TestFloat32ImplicitWidenStoresIntoFloat64Column(t *testing.T) {
	// f32 → f64 is the implicit, lossless widen (the tower): a f32 VALUE stores into a
	// f64 column (storeValue widens), and assignableTo permits the family pairing.
	got, err := storeValue(Float32Value(1.5), scalarFloat64, nil, nil, false, "x")
	if err != nil {
		t.Fatalf("f32 → f64 column store: %v", err)
	}
	if got.Kind != ValFloat64 || got.F64() != 1.5 {
		t.Errorf("f32 1.5 widened = %v (kind %v)", got.Render(), got.Kind)
	}
	if !assignableTo(resolvedType{kind: rtFloat32}, scalarFloat64) {
		t.Errorf("f32 should be assignable to a f64 column")
	}
	// f64 → f32 column is NOT implicitly assignable (explicit CAST only).
	if assignableTo(resolvedType{kind: rtFloat64}, scalarFloat32) {
		t.Errorf("f64 should NOT be implicitly assignable to a f32 column")
	}
	if _, err := storeValue(Float64Value(1.5), scalarFloat32, nil, nil, false, "x"); err == nil {
		t.Errorf("storing a f64 value into a f32 column should be a 42804")
	}
}

// --- casts: int/decimal→float exact-decimal expansion (value-level) -----------------------

func TestExactDecimalFromFloatMatchesParseRoundTrip(t *testing.T) {
	// The exact decimal of a binary64 must re-parse to the same binary64 (it is the EXACT value).
	for _, f := range []float64{0.1, 0.5, 1.0 / 3.0, 123456.789, -2.25, 1e-10} {
		d := exactDecimalFromFloat64(f)
		back, err := decimalToFloat64(d)
		if err != nil {
			t.Fatalf("exact decimal of %v failed to reparse: %v", f, err)
		}
		if back != f {
			t.Errorf("exactDecimalFromFloat64(%v) = %s → reparsed %v", f, d.Render(), back)
		}
	}
}

// --- SUM/AVG: the order-independent canonical-order fold ----------------------------------

func TestFloatSumDirectFoldOrderIndependent(t *testing.T) {
	// Drive the accumulator directly to assert the canonical fold is independent of add order.
	xs := []float64{1e16, 1, -1e16, 2.5, -0.5, 3e-8}
	forward := newFloatSumAcc(false)
	for _, x := range xs {
		forward.add(Float64Value(x))
	}
	reverse := newFloatSumAcc(false)
	for i := len(xs) - 1; i >= 0; i-- {
		reverse.add(Float64Value(xs[i]))
	}
	fa, _, err := forward.sumF64()
	if err != nil {
		t.Fatal(err)
	}
	rb, _, err := reverse.sumF64()
	if err != nil {
		t.Fatal(err)
	}
	if fa != rb {
		t.Errorf("canonical fold not order-independent: %v vs %v", fa, rb)
	}
}

func TestFloatSumSpecialValues(t *testing.T) {
	// Any NaN → NaN; both ±Inf → NaN; +Inf alone → +Inf.
	withNaN := newFloatSumAcc(false)
	withNaN.add(Float64Value(1))
	withNaN.add(Float64Value(math.NaN()))
	if s, _, _ := withNaN.sumF64(); !math.IsNaN(s) {
		t.Errorf("SUM with a NaN should be NaN, got %v", s)
	}
	both := newFloatSumAcc(false)
	both.add(Float64Value(math.Inf(1)))
	both.add(Float64Value(math.Inf(-1)))
	if s, _, _ := both.sumF64(); !math.IsNaN(s) {
		t.Errorf("SUM with +Inf and -Inf should be NaN, got %v", s)
	}
	pos := newFloatSumAcc(false)
	pos.add(Float64Value(math.Inf(1)))
	pos.add(Float64Value(5))
	if s, _, _ := pos.sumF64(); !math.IsInf(s, 1) {
		t.Errorf("SUM with +Inf should be +Inf, got %v", s)
	}
}

// --- literal parsing + rendering spellings ------------------------------------------------

func TestFloatLiteralParsing(t *testing.T) {
	db := dbWith(t)
	good := map[string]float64{
		"f64 '1.5'":       1.5,
		"f64 '-3E-7'":     -3e-7,
		"f64 '1.5e10'":    1.5e10,
		"f64 'Infinity'":  math.Inf(1),
		"f64 '-Infinity'": math.Inf(-1),
		"f64 'inf'":       math.Inf(1),
	}
	for sql, want := range good {
		got := query(t, db, "SELECT "+sql)[0][0].F64()
		if got != want && !(math.IsInf(want, 0) && got == want) {
			t.Errorf("%s = %v, want %v", sql, got, want)
		}
	}
	if r := query(t, db, "SELECT f64 'NaN'"); !math.IsNaN(r[0][0].F64()) {
		t.Errorf("f64 'NaN' should parse to NaN")
	}
	// Malformed → 22P02; out of range → 22003.
	if code := errCode(t, db, "SELECT f64 'abc'"); code != "22P02" {
		t.Errorf("malformed float literal should be 22P02, got %s", code)
	}
	if code := errCode(t, db, "SELECT f64 '1e400'"); code != "22003" {
		t.Errorf("out-of-range float literal should be 22003, got %s", code)
	}
	if code := errCode(t, db, "SELECT f32 '1e40'"); code != "22003" {
		t.Errorf("out-of-range f32 literal should be 22003, got %s", code)
	}
}

func TestFloatRenderSpellings(t *testing.T) {
	cases := map[float64]string{
		math.Inf(1):          "Infinity",
		math.Inf(-1):         "-Infinity",
		math.NaN():           "NaN",
		math.Copysign(0, -1): "-0",
		0:                    "0",
		1.5:                  "1.5",
	}
	for f, want := range cases {
		if got := renderFloat64(f); got != want {
			t.Errorf("renderFloat64(%v) = %q, want %q", f, got, want)
		}
	}
}
