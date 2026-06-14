package jed

// float32 / float64 — the IEEE 754 binary float types (spec/design/float.md). These per-feature
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
		enc := encodeValue(Float64, v)
		pos := 0
		got, err := readInlineBody(Float64, enc[1:], &pos) // skip the 0x00 presence tag
		if err != nil {
			t.Fatalf("float64 decode %v: %v", f, err)
		}
		if got.Kind != ValFloat64 {
			t.Fatalf("float64 %v: kind %v", f, got.Kind)
		}
		// Compare by BITS. Storage preserves the original pattern verbatim (incl -0's sign bit) EXCEPT
		// for NaN, which canonicalizes to the single quiet pattern 0x7FF8…000 (float.md §10).
		// Float64Value stashes bits in Int, so == on Int is a bit compare.
		want := math.Float64bits(f)
		if math.IsNaN(f) {
			want = 0x7FF8000000000000
		}
		if uint64(got.Int) != want {
			t.Errorf("float64 %v: bits %#016x != %#016x", f, uint64(got.Int), want)
		}
	}
	cases32 := []float32{
		0, float32(math.Copysign(0, -1)), 1, -1, 1.5, -2.25,
		math.MaxFloat32, float32(math.Inf(1)), float32(math.Inf(-1)), float32(math.NaN()),
	}
	for _, f := range cases32 {
		v := Float32Value(f)
		enc := encodeValue(Float32, v)
		pos := 0
		got, err := readInlineBody(Float32, enc[1:], &pos)
		if err != nil {
			t.Fatalf("float32 decode %v: %v", f, err)
		}
		want := math.Float32bits(f)
		if math.IsNaN(float64(f)) {
			want = 0x7FC00000 // NaN canonicalizes on store (float.md §10)
		}
		if got.Kind != ValFloat32 || uint32(got.Int) != want {
			t.Errorf("float32 %v: bits %#08x != %#08x", f, uint32(got.Int), want)
		}
	}
}

func TestFloatImageRoundTrip(t *testing.T) {
	// The on-disk single-file image (the §8 cross-core round-trip contract) preserves float bits
	// verbatim — incl -0 / ±Inf — across a serialize + reload (a NaN canonicalizes to one quiet
	// pattern but stays a NaN; float.md §10).
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, a float32, b float64)",
		"INSERT INTO f VALUES (1, '1.5', 'NaN')",
		"INSERT INTO f VALUES (2, '-0', '-Infinity')",
		"INSERT INTO f VALUES (3, 'Infinity', '3.141592653589793')",
	)
	img, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatal(err)
	}
	db2, err := LoadDatabase(img)
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
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
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
		{ninf, -1, -1}, {-1, pinf, -1}, {pinf, nan, -1}, // -inf < finite < +inf < NaN
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

func TestFloatNaNEqualsNaNIsTrue(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, 'NaN')",
		"INSERT INTO f VALUES (2, 'NaN')",
	)
	// NaN = NaN is TRUE in jed's total order (PG float8 =), so both rows match.
	got := queryIDs(t, db, "SELECT id FROM f WHERE x = float64 'NaN' ORDER BY id")
	if len(got) != 2 || got[0] != 1 || got[1] != 2 {
		t.Errorf("NaN = NaN should select both rows, got %v", got)
	}
	// -0 = +0 is TRUE.
	if r := query(t, db, "SELECT float64 '-0' = float64 '0'"); !r[0][0].Bool {
		t.Errorf("-0 = +0 should be TRUE")
	}
}

func TestFloatOrderByPlacesNaNLast(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, 'NaN')",
		"INSERT INTO f VALUES (2, '-Infinity')",
		"INSERT INTO f VALUES (3, '0')",
		"INSERT INTO f VALUES (4, 'Infinity')",
	)
	got := queryIDs(t, db, "SELECT id FROM f ORDER BY x")
	want := []int64{2, 3, 4, 1} // -inf, 0, +inf, NaN
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("ORDER BY x = %v, want %v", got, want)
		}
	}
}

func TestFloatDistinctCollapsesNaNAndZeroSigns(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
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

// --- traps: finite overflow 22003, division by zero 22012 ---------------------------------

func TestFloatFiniteOverflowTraps22003(t *testing.T) {
	db := dbWith(t, "CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, '1e308')")
	if code := errCode(t, db, "SELECT x * float64 '10' FROM f"); code != "22003" {
		t.Errorf("finite overflow should trap 22003, got %s", code)
	}
}

func TestFloatDivByZeroTraps22012(t *testing.T) {
	db := dbWith(t, "CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, '1.5')")
	if code := errCode(t, db, "SELECT x / float64 '0' FROM f"); code != "22012" {
		t.Errorf("x/0 should trap 22012, got %s", code)
	}
	if code := errCode(t, db, "SELECT float64 '0' / float64 '0'"); code != "22012" {
		t.Errorf("0/0 should trap 22012, got %s", code)
	}
}

func TestFloatInfNaNOperandPropagateNoTrap(t *testing.T) {
	// An already-Inf/NaN operand propagates by IEEE — it does NOT trap (finite arith only traps).
	if r := query(t, dbWith(t), "SELECT float64 'Infinity' + float64 '1'"); r[0][0].Render() != "Infinity" {
		t.Errorf("Inf + 1 should be Infinity, got %s", r[0][0].Render())
	}
	if r := query(t, dbWith(t), "SELECT float64 'Infinity' - float64 'Infinity'"); r[0][0].Render() != "NaN" {
		t.Errorf("Inf - Inf should be NaN, got %s", r[0][0].Render())
	}
}

// --- the promotion tower: float32 + float64 → float64 -------------------------------------

func TestFloatMixedWidthArithmeticPromotes(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, a float32, b float64)",
		"INSERT INTO f VALUES (1, '1.5', '2.25')",
	)
	out, err := Execute(db, "SELECT a + b FROM f")
	if err != nil {
		t.Fatal(err)
	}
	if out.ColumnTypes[0] != "float64" {
		t.Errorf("float32 + float64 result type = %s, want float64", out.ColumnTypes[0])
	}
	if got := out.Rows[0][0]; got.Kind != ValFloat64 || got.F64() != 3.75 {
		t.Errorf("1.5 + 2.25 = %v, want 3.75 (float64)", got.Render())
	}
}

func TestFloat32ImplicitWidenStoresIntoFloat64Column(t *testing.T) {
	// float32 → float64 is the implicit, lossless widen (the tower): a float32 VALUE stores into a
	// float64 column (storeValue widens), and assignableTo permits the family pairing.
	got, err := storeValue(Float32Value(1.5), Float64, nil, false, "x")
	if err != nil {
		t.Fatalf("float32 → float64 column store: %v", err)
	}
	if got.Kind != ValFloat64 || got.F64() != 1.5 {
		t.Errorf("float32 1.5 widened = %v (kind %v)", got.Render(), got.Kind)
	}
	if !assignableTo(resolvedType{kind: rtFloat32}, Float64) {
		t.Errorf("float32 should be assignable to a float64 column")
	}
	// float64 → float32 column is NOT implicitly assignable (explicit CAST only).
	if assignableTo(resolvedType{kind: rtFloat64}, Float32) {
		t.Errorf("float64 should NOT be implicitly assignable to a float32 column")
	}
	if _, err := storeValue(Float64Value(1.5), Float32, nil, false, "x"); err == nil {
		t.Errorf("storing a float64 value into a float32 column should be a 42804")
	}
}

func TestFloatStrictIslandRejectsCrossFamily(t *testing.T) {
	db := dbWith(t, "CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, '1.5')")
	// integer ⊕ float and integer = float are 42804 — float is a strict island (no implicit cast).
	if code := errCode(t, db, "SELECT id + x FROM f"); code != "42804" {
		t.Errorf("integer + float64 should be 42804, got %s", code)
	}
	if code := errCode(t, db, "SELECT id = x FROM f"); code != "42804" {
		t.Errorf("integer = float64 should be 42804, got %s", code)
	}
}

// --- casts: float→int (half away), NaN→int 22003, int/decimal→float, narrowing ------------

func TestFloatToIntRoundsHalfAway(t *testing.T) {
	db := dbWith(t)
	cases := map[string]int64{
		"CAST(float64 '2.5' AS int32)":  3,
		"CAST(float64 '-2.5' AS int32)": -3, // half AWAY from zero (not half-to-even like PG)
		"CAST(float64 '2.4' AS int32)":  2,
		"CAST(float64 '-2.6' AS int32)": -3,
	}
	for sql, want := range cases {
		if got := query(t, db, "SELECT "+sql)[0][0].Int; got != want {
			t.Errorf("%s = %d, want %d", sql, got, want)
		}
	}
}

func TestFloatNaNInfToIntTraps22003(t *testing.T) {
	db := dbWith(t)
	for _, sql := range []string{
		"CAST(float64 'NaN' AS int64)",
		"CAST(float64 'Infinity' AS int64)",
		"CAST(float64 '-Infinity' AS int32)",
		"CAST(float64 '1e30' AS int32)", // finite but out of int32 range
	} {
		if code := errCode(t, db, "SELECT "+sql); code != "22003" {
			t.Errorf("%s should trap 22003, got %s", sql, code)
		}
	}
}

func TestIntAndDecimalToFloatCasts(t *testing.T) {
	db := dbWith(t)
	if got := query(t, db, "SELECT CAST(7 AS float64)")[0][0]; got.Kind != ValFloat64 || got.F64() != 7 {
		t.Errorf("CAST(7 AS float64) = %v", got.Render())
	}
	if got := query(t, db, "SELECT CAST(1.5 AS float32)")[0][0]; got.Kind != ValFloat32 || got.F32() != 1.5 {
		t.Errorf("CAST(1.5 AS float32) = %v", got.Render())
	}
	// No IMPLICIT int/decimal → float: a bare comparison without a cast is 42804 (covered above).
}

func TestFloatToDecimalExact(t *testing.T) {
	db := dbWith(t)
	// 0.5 is exactly representable in binary, so its exact decimal is 0.5.
	if got := query(t, db, "SELECT CAST(float64 '0.5' AS decimal)")[0][0].Render(); got != "0.5" {
		t.Errorf("CAST(0.5 AS decimal) = %q, want 0.5", got)
	}
	if code := errCode(t, db, "SELECT CAST(float64 'NaN' AS decimal)"); code != "22003" {
		t.Errorf("NaN → decimal should trap 22003, got %s", code)
	}
}

func TestFloat64ToFloat32NarrowOverflowTraps(t *testing.T) {
	db := dbWith(t)
	if code := errCode(t, db, "SELECT CAST(float64 '1e300' AS float32)"); code != "22003" {
		t.Errorf("float64 1e300 → float32 should trap 22003, got %s", code)
	}
	// In range narrows fine.
	if got := query(t, db, "SELECT CAST(float64 '1.5' AS float32)")[0][0]; got.Kind != ValFloat32 || got.F32() != 1.5 {
		t.Errorf("float64 1.5 → float32 = %v", got.Render())
	}
}

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

func TestFloatSumIsOrderIndependent(t *testing.T) {
	// Classic non-associative case: 1e16 + 1 + -1e16. Naive left-fold in input order loses the 1;
	// the canonical-order fold sorts first, so it is order-independent and identical regardless of
	// the INSERT order. Both orders must agree (the in-contract cross-core guarantee).
	dbA := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, '1e16')",
		"INSERT INTO f VALUES (2, '1')",
		"INSERT INTO f VALUES (3, '-1e16')",
	)
	dbB := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, '-1e16')",
		"INSERT INTO f VALUES (2, '1e16')",
		"INSERT INTO f VALUES (3, '1')",
	)
	a := query(t, dbA, "SELECT SUM(x) FROM f")[0][0].F64()
	b := query(t, dbB, "SELECT SUM(x) FROM f")[0][0].F64()
	if a != b {
		t.Errorf("SUM must be order-independent: %v vs %v", a, b)
	}
	// Result type stays the input width (float64).
	out, _ := Execute(dbA, "SELECT SUM(x) FROM f")
	if out.ColumnTypes[0] != "float64" {
		t.Errorf("SUM(float64) type = %s, want float64", out.ColumnTypes[0])
	}
}

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

func TestFloatAvgEmptyIsNull(t *testing.T) {
	db := dbWith(t, "CREATE TABLE f (id int32 PRIMARY KEY, x float64)")
	if got := query(t, db, "SELECT AVG(x) FROM f")[0][0]; got.Kind != ValNull {
		t.Errorf("AVG over empty should be NULL, got %v", got.Render())
	}
}

func TestFloatMinMaxTotalOrder(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE f (id int32 PRIMARY KEY, x float64)",
		"INSERT INTO f VALUES (1, 'NaN')",
		"INSERT INTO f VALUES (2, '-5')",
		"INSERT INTO f VALUES (3, '10')",
	)
	// MIN ignores NaN-as-largest only insofar as MAX should pick NaN (largest), MIN picks -5.
	if got := query(t, db, "SELECT MIN(x) FROM f")[0][0].Render(); got != "-5" {
		t.Errorf("MIN = %q, want -5", got)
	}
	if got := query(t, db, "SELECT MAX(x) FROM f")[0][0].Render(); got != "NaN" {
		t.Errorf("MAX = %q, want NaN (the largest in the total order)", got)
	}
}

// --- one transcendental + an exact function ----------------------------------------------

func TestFloatSqrtAndTranscendental(t *testing.T) {
	db := dbWith(t)
	if got := query(t, db, "SELECT sqrt(float64 '4')")[0][0].F64(); got != 2 {
		t.Errorf("sqrt(4) = %v, want 2", got)
	}
	if code := errCode(t, db, "SELECT sqrt(float64 '-1')"); code != "22003" {
		t.Errorf("sqrt(-1) should trap 22003, got %s", code)
	}
	// exp(0) = 1 exactly across libms (a transcendental, but this input is exact).
	if got := query(t, db, "SELECT exp(float64 '0')")[0][0].F64(); got != 1 {
		t.Errorf("exp(0) = %v, want 1", got)
	}
	// ln(0) and ln(-1) trap 22003 (domain errors — NaN stays input-only).
	if code := errCode(t, db, "SELECT ln(float64 '0')"); code != "22003" {
		t.Errorf("ln(0) should trap 22003, got %s", code)
	}
	if code := errCode(t, db, "SELECT ln(float64 '-1')"); code != "22003" {
		t.Errorf("ln(-1) should trap 22003, got %s", code)
	}
}

func TestFloatExactFunctionsWidthAndRound(t *testing.T) {
	db := dbWith(t)
	// abs over float32 stays float32 (the catalog's "promoted"); ceil/floor/round are float64.
	out, _ := Execute(db, "SELECT abs(float32 '-1.5')")
	if out.ColumnTypes[0] != "float32" {
		t.Errorf("abs(float32) type = %s, want float32", out.ColumnTypes[0])
	}
	if got := query(t, db, "SELECT round(float64 '2.5')")[0][0].F64(); got != 3 {
		t.Errorf("round(2.5) = %v, want 3 (half away)", got)
	}
	if got := query(t, db, "SELECT ceil(float64 '1.1')")[0][0].F64(); got != 2 {
		t.Errorf("ceil(1.1) = %v, want 2", got)
	}
}

// --- literal parsing + rendering spellings ------------------------------------------------

func TestFloatLiteralParsing(t *testing.T) {
	db := dbWith(t)
	good := map[string]float64{
		"float64 '1.5'":       1.5,
		"float64 '-3E-7'":     -3e-7,
		"float64 '1.5e10'":    1.5e10,
		"float64 'Infinity'":  math.Inf(1),
		"float64 '-Infinity'": math.Inf(-1),
		"float64 'inf'":       math.Inf(1),
	}
	for sql, want := range good {
		got := query(t, db, "SELECT "+sql)[0][0].F64()
		if got != want && !(math.IsInf(want, 0) && got == want) {
			t.Errorf("%s = %v, want %v", sql, got, want)
		}
	}
	if r := query(t, db, "SELECT float64 'NaN'"); !math.IsNaN(r[0][0].F64()) {
		t.Errorf("float64 'NaN' should parse to NaN")
	}
	// Malformed → 22P02; out of range → 22003.
	if code := errCode(t, db, "SELECT float64 'abc'"); code != "22P02" {
		t.Errorf("malformed float literal should be 22P02, got %s", code)
	}
	if code := errCode(t, db, "SELECT float64 '1e400'"); code != "22003" {
		t.Errorf("out-of-range float literal should be 22003, got %s", code)
	}
	if code := errCode(t, db, "SELECT float32 '1e40'"); code != "22003" {
		t.Errorf("out-of-range float32 literal should be 22003, got %s", code)
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

func TestFloatPrimaryKeyRejected(t *testing.T) {
	db := NewDatabase()
	if code := codeOfErr(t, db, "CREATE TABLE f (x float64 PRIMARY KEY)"); code != "0A000" {
		t.Errorf("float PRIMARY KEY should be 0A000, got %s", code)
	}
	if code := codeOfErr(t, db, "CREATE TABLE f (id int32 PRIMARY KEY, x float32)"); code != "" {
		t.Errorf("a non-key float column should be fine, got %s", code)
	}
}

// codeOfErr returns the engine error code for sql, or "" on success (local helper to avoid the
// t.Fatal in errCode for the success case).
func codeOfErr(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		return ""
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected *EngineError for %q, got %T", sql, err)
	}
	return ee.Code()
}
