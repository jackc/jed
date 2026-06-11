package jed

// Phase 3: the exact decimal / numeric type — unit tests on the Decimal type and end-to-end
// tests through Execute (spec/design/decimal.md). End-to-end assertions are on RENDERED output
// (the cross-core contract), since decimal value-equality (1.5 == 1.50) is scale-insensitive.

import (
	"strings"
	"testing"
)

// dec parses "[-]int[.frac]" into a Decimal (mirrors the lexer/parser).
func dec(s string) Decimal {
	neg := false
	if rest, ok := strings.CutPrefix(s, "-"); ok {
		neg, s = true, rest
	}
	intPart, frac, _ := strings.Cut(s, ".")
	return DecimalFromDigitsScale(neg, intPart+frac, uint32(len(frac)))
}

// mustRender renders the result of an arithmetic op, panicking on an unexpected error (so it
// can be called directly on a (Decimal, error) return — Go spreads the pair into the params).
func mustRender(d Decimal, err error) string {
	if err != nil {
		panic(err)
	}
	return d.Render()
}

func TestDecimalRenderPreservesScale(t *testing.T) {
	cases := map[string]string{
		"1.50": "1.50", "1.5": "1.5", "0.00": "0.00", "0": "0",
		"-0.013": "-0.013", "123": "123", ".5": "0.5", "100": "100",
	}
	for in, want := range cases {
		if got := dec(in).Render(); got != want {
			t.Errorf("dec(%q).Render() = %q, want %q", in, got, want)
		}
	}
}

func TestDecimalNoNegativeZero(t *testing.T) {
	for _, s := range []string{"0", "-0", "-0.00"} {
		if dec(s).Neg {
			t.Errorf("dec(%q) should not be negative", s)
		}
	}
	r, _ := dec("1.0").Sub(dec("1.0"))
	if r.Render() != "0.0" || r.Neg {
		t.Errorf("1.0 - 1.0 = %q neg=%v, want 0.0 +0", r.Render(), r.Neg)
	}
}

func TestDecimalValueEqualityIgnoresScale(t *testing.T) {
	if dec("1.5").CmpValue(dec("1.50")) != 0 {
		t.Error("1.5 should equal 1.50 by value")
	}
	if dec("10").CmpValue(dec("10.0")) != 0 {
		t.Error("10 should equal 10.0 by value")
	}
	if dec("1.5").CmpValue(dec("1.6")) == 0 {
		t.Error("1.5 != 1.6")
	}
}

func TestDecimalOrdering(t *testing.T) {
	asc := []string{"-10", "-1", "0", "0.5", "1", "10"}
	for i := 0; i+1 < len(asc); i++ {
		if dec(asc[i]).CmpValue(dec(asc[i+1])) >= 0 {
			t.Errorf("%s should be < %s", asc[i], asc[i+1])
		}
	}
	if dec("1.23").CmpValue(dec("1.2")) <= 0 {
		t.Error("1.23 should be > 1.2")
	}
}

func TestDecimalAddSubMul(t *testing.T) {
	check := func(got, want string) {
		t.Helper()
		if got != want {
			t.Errorf("got %q, want %q", got, want)
		}
	}
	check(mustRender(dec("1.50").Add(dec("1.5"))), "3.00")
	check(mustRender(dec("1.234").Sub(dec("1.2"))), "0.034")
	check(mustRender(dec("1.50").Mul(dec("1.5"))), "2.250")
	check(mustRender(dec("2.0").Mul(dec("3.000"))), "6.0000")
}

func TestDecimalDivisionScaleAndRounding(t *testing.T) {
	cases := []struct{ a, b, want string }{
		{"1", "3", "0.33333333333333333333"},
		{"2", "3", "0.66666666666666666667"},
		{"1", "7", "0.14285714285714285714"},
		{"10.0", "4.0", "2.5000000000000000"},
		{"1.0", "8.0", "0.12500000000000000000"},
		{"100", "7", "14.2857142857142857"},
	}
	for _, c := range cases {
		if got := mustRender(dec(c.a).Div(dec(c.b))); got != c.want {
			t.Errorf("%s / %s = %q, want %q", c.a, c.b, got, c.want)
		}
	}
}

func TestDecimalModulo(t *testing.T) {
	cases := []struct{ a, b, want string }{
		{"5.5", "2", "1.5"},
		{"-5.5", "2", "-1.5"},
		{"5.50", "2.0", "1.50"},
	}
	for _, c := range cases {
		if got := mustRender(dec(c.a).Rem(dec(c.b))); got != c.want {
			t.Errorf("%s %% %s = %q, want %q", c.a, c.b, got, c.want)
		}
	}
}

func TestDecimalRoundingHalfAway(t *testing.T) {
	cases := []struct {
		in     string
		scale  uint32
		render string
	}{
		{"0.125", 2, "0.13"},
		{"-0.125", 2, "-0.13"},
		{"2.5", 0, "3"},
		{"-2.5", 0, "-3"},
		{"2.45", 1, "2.5"},
		{"9.5", 0, "10"},
	}
	for _, c := range cases {
		if got := dec(c.in).RoundToScale(c.scale).Render(); got != c.render {
			t.Errorf("round(%s, %d) = %q, want %q", c.in, c.scale, got, c.render)
		}
	}
}

func TestDecimalDivZeroTraps(t *testing.T) {
	if _, err := dec("1").Div(dec("0")); err == nil || err.(*EngineError).Code() != "22012" {
		t.Errorf("1/0 should trap 22012, got %v", err)
	}
	if _, err := dec("1").Rem(dec("0")); err == nil || err.(*EngineError).Code() != "22012" {
		t.Errorf("1%%0 should trap 22012, got %v", err)
	}
}

func TestDecimalToInt64Round(t *testing.T) {
	cases := []struct {
		in   string
		want int64
		ok   bool
	}{
		{"2.5", 3, true},
		{"-2.5", -3, true},
		{"2.4", 2, true},
		{"100", 100, true},
		{"100000000000000000000000000000", 0, false},
	}
	for _, c := range cases {
		got, ok := dec(c.in).ToInt64Round()
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("%s.ToInt64Round() = (%d,%v), want (%d,%v)", c.in, got, ok, c.want, c.ok)
		}
	}
}

func TestDecimalCodecRoundTrip(t *testing.T) {
	for _, s := range []string{"0", "1.50", "-12345.6789", "100000000.000001", "999999999999"} {
		d := dec(s)
		neg, scale, groups := d.ToCodec()
		back := DecimalFromCodec(neg, scale, groups)
		if back.Render() != d.Render() {
			t.Errorf("codec round trip %s = %q", s, back.Render())
		}
	}
	if _, _, groups := dec("0.00").ToCodec(); len(groups) != 0 {
		t.Error("zero should carry no groups")
	}
}

func TestDecimalBigMultiplicationExact(t *testing.T) {
	// 38-digit * 38-digit (76 digits) fits no int128; the limb path is exact.
	a := dec("12345678901234567890123456789012345678")
	b := dec("99999999999999999999999999999999999999")
	r, err := a.Mul(b)
	if err != nil || r.Precision() != 76 {
		t.Errorf("product precision = %d (err %v), want 76", r.Precision(), err)
	}
}

// --- end-to-end through Execute ---------------------------------------------

func decExec(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func decDB(t *testing.T, stmts ...string) *Database {
	t.Helper()
	db := NewDatabase()
	for _, s := range stmts {
		decExec(t, db, s)
	}
	return db
}

// decOne runs a query expected to return a single cell, rendered.
func decOne(t *testing.T, db *Database, sql string) string {
	t.Helper()
	rows := query(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%q: expected one cell, got %v", sql, rows)
	}
	return rows[0][0].Render()
}

// decErr runs a statement expected to fail and returns its SQLSTATE.
func decErr(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%q should have failed", sql)
	}
	return err.(*EngineError).Code()
}

func TestDecimalStorageAndScaleEndToEnd(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
		"INSERT INTO t VALUES (1, 1.50), (2, 1.5), (3, 0.00), (4, -0.013), (5, 123), (6, NULL)",
	)
	want := map[string]string{"1": "1.50", "2": "1.5", "3": "0.00", "4": "-0.013", "5": "123", "6": "NULL"}
	for id, w := range want {
		if got := decOne(t, db, "SELECT v FROM t WHERE id = "+id); got != w {
			t.Errorf("id %s: got %q, want %q", id, got, w)
		}
	}
}

func TestDecimalTypmodRoundsOnStore(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, money numeric(10,2))",
		"INSERT INTO t VALUES (1, 1.5), (2, 1.555), (3, 1.554), (4, 5), (5, -2.5)",
	)
	want := map[string]string{"1": "1.50", "2": "1.56", "3": "1.55", "4": "5.00", "5": "-2.50"}
	for id, w := range want {
		if got := decOne(t, db, "SELECT money FROM t WHERE id = "+id); got != w {
			t.Errorf("id %s: got %q, want %q", id, got, w)
		}
	}
}

func TestDecimalErrorsEndToEnd(t *testing.T) {
	db := decDB(t, "CREATE TABLE t (id int32 PRIMARY KEY, v numeric(3,2))")
	if c := decErr(t, db, "INSERT INTO t VALUES (1, 12.34)"); c != "22003" {
		t.Errorf("precision overflow = %s, want 22003", c)
	}
	if c := decErr(t, NewDatabase(), "CREATE TABLE a (x numeric(0))"); c != "22023" {
		t.Errorf("numeric(0) = %s, want 22023", c)
	}
	if c := decErr(t, NewDatabase(), "CREATE TABLE c (x numeric(5,7))"); c != "22023" {
		t.Errorf("numeric(5,7) = %s, want 22023", c)
	}
	if c := decErr(t, NewDatabase(), "CREATE TABLE d (x int32(5))"); c != "0A000" {
		t.Errorf("int32(5) typmod = %s, want 0A000", c)
	}
	if c := decErr(t, NewDatabase(), "CREATE TABLE t (k numeric PRIMARY KEY)"); c != "0A000" {
		t.Errorf("decimal PK = %s, want 0A000", c)
	}
	db2 := decDB(t, "CREATE TABLE u (id int32 PRIMARY KEY, n numeric, i int32, s text)",
		"INSERT INTO u VALUES (1, 1.5, 2, 'x')")
	if c := decErr(t, db2, "SELECT id FROM u WHERE n = 'x'"); c != "42804" {
		t.Errorf("decimal vs text = %s, want 42804", c)
	}
	if c := decErr(t, db2, "INSERT INTO u VALUES (2, 1.0, 1.5, 'y')"); c != "42804" {
		t.Errorf("decimal into int column = %s, want 42804", c)
	}
}

func TestDecimalArithmeticAndComparisonEndToEnd(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a numeric, b numeric)",
		"INSERT INTO t VALUES (1, 1.50, 1.5), (2, 1, 3), (3, -5.5, 2)",
	)
	if got := decOne(t, db, "SELECT a + b FROM t WHERE id = 1"); got != "3.00" {
		t.Errorf("a+b = %q", got)
	}
	if got := decOne(t, db, "SELECT a / b FROM t WHERE id = 2"); got != "0.33333333333333333333" {
		t.Errorf("1/3 = %q", got)
	}
	if got := decOne(t, db, "SELECT a % b FROM t WHERE id = 3"); got != "-1.5" {
		t.Errorf("-5.5%%2 = %q", got)
	}
	if c := decErr(t, db, "SELECT a / 0 FROM t WHERE id = 1"); c != "22012" {
		t.Errorf("div by zero = %s", c)
	}
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE a = 1.5"); len(ids) != 1 || ids[0] != 1 {
		t.Errorf("a = 1.5 matched %v, want [1]", ids)
	}
}

func TestDecimalCastsEndToEnd(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, i int32, d numeric)",
		"INSERT INTO t VALUES (1, 7, 2.5)",
	)
	if got := decOne(t, db, "SELECT CAST(i AS numeric(10,2)) FROM t WHERE id = 1"); got != "7.00" {
		t.Errorf("int->numeric(10,2) = %q", got)
	}
	if got := decOne(t, db, "SELECT CAST(d AS int32) FROM t WHERE id = 1"); got != "3" {
		t.Errorf("2.5::int32 = %q, want 3 (half away)", got)
	}
	if got := decOne(t, db, "SELECT CAST(-2.5 AS int32) FROM t WHERE id = 1"); got != "-3" {
		t.Errorf("-2.5::int32 = %q", got)
	}
}

func TestDecimalOnDiskRoundTripEndToEnd(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, money numeric(10,2), free numeric)",
		"INSERT INTO t VALUES (1, 1.5, -12345.6789), (2, 0, 0.00), (3, 100, NULL)",
	)
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatal(err)
	}
	again, _ := loaded.ToImage(8192, 1)
	if string(again) != string(image) {
		t.Error("re-serialization is not byte-identical")
	}
	if got := decOne(t, loaded, "SELECT free FROM t WHERE id = 1"); got != "-12345.6789" {
		t.Errorf("reloaded free = %q", got)
	}
	// the reloaded numeric(10,2) typmod still coerces a new insert
	decExec(t, loaded, "INSERT INTO t VALUES (4, 9.999, 9.999)")
	if got := decOne(t, loaded, "SELECT money FROM t WHERE id = 4"); got != "10.00" {
		t.Errorf("typmod not persisted: money = %q", got)
	}
}

func TestDecimalDistinctCollapsesEqualValues(t *testing.T) {
	db := decDB(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
		"INSERT INTO t VALUES (1, 1.5), (2, 1.50), (3, 1.500), (4, 2.0)",
	)
	rows := query(t, db, "SELECT DISTINCT v FROM t ORDER BY v")
	got := make([]string, len(rows))
	for i, r := range rows {
		got[i] = r[0].Render()
	}
	if len(got) != 2 || got[0] != "1.5" || got[1] != "2.0" {
		t.Errorf("DISTINCT v = %v, want [1.5 2.0]", got)
	}
}

// TestDecimalFormatCaps pins the PG numeric-format caps (spec/design/decimal.md §2): the
// original 1000/1000 absolute cap is lifted; the bounds are 131072 integer digits and scale
// 16383, 22003 past either; a value AT both caps stores (spilling to overflow chains) and
// round-trips. Mirrors impl/rust/tests/decimal.rs and impl/ts/tests/decimal.test.ts.
func TestDecimalFormatCaps(t *testing.T) {
	db := decDB(t, "CREATE TABLE t (id int32 PRIMARY KEY, v numeric)")
	overOld := "0." + strings.Repeat("0", 1001) + "1" // scale 1002: legal now
	decExec(t, db, "INSERT INTO t VALUES (1, "+overOld+")")
	if got := decOne(t, db, "SELECT v FROM t WHERE id = 1"); got != overOld {
		t.Errorf("over-old-cap value did not round-trip")
	}
	// scale > 16383 traps 22003 at resolve (PG numeric_in).
	overScale := "0." + strings.Repeat("0", 16383) + "1"
	if c := decErr(t, db, "INSERT INTO t VALUES (2, "+overScale+")"); c != "22003" {
		t.Errorf("over-scale literal = %s, want 22003", c)
	}
	// integer digits > 131072 trap 22003 at resolve. (Dotted: a dot-less literal is an
	// INTEGER literal, 42601 past i64 — types.md §6; the decimal path needs the `.`.)
	overInt := "1" + strings.Repeat("0", 131072) + ".0"
	if c := decErr(t, db, "INSERT INTO t VALUES (2, "+overInt+")"); c != "22003" {
		t.Errorf("over-int-digits literal = %s, want 22003", c)
	}
	// exactly AT both caps is legal, stores, and round-trips.
	atCaps := "1" + strings.Repeat("0", 131071) + "." + strings.Repeat("0", 16382) + "1"
	decExec(t, db, "INSERT INTO t VALUES (3, "+atCaps+")")
	if got := decOne(t, db, "SELECT v FROM t WHERE id = 3"); got != atCaps {
		t.Errorf("at-caps value did not round-trip")
	}
}

// TestDecimalMulRoundsAtMaxScale pins PG numeric_mul's rounding: an exact product whose scale
// exceeds max_scale (16383) ROUNDS to it, half away from zero, instead of trapping
// (spec/design/decimal.md §2).
func TestDecimalMulRoundsAtMaxScale(t *testing.T) {
	db := decDB(t, "CREATE TABLE t (id int32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	tiny1 := "0." + strings.Repeat("0", 8191) + "1" // 1e-8192 (scale 8192)
	tiny5 := "0." + strings.Repeat("0", 8191) + "5" // 5e-8192
	// 1e-8192 * 1e-8192 = 1e-16384: the dropped digit is 1 -> rounds DOWN to 0 at scale 16383.
	if got := decOne(t, db, "SELECT "+tiny1+" * "+tiny1+" = 0 FROM t"); got != "true" {
		t.Errorf("1e-16384 should round to zero, got %s", got)
	}
	// 5e-8192 * 1e-8192 = 5e-16384: the dropped digit is 5 -> rounds UP to 1e-16383, nonzero.
	if got := decOne(t, db, "SELECT "+tiny5+" * "+tiny1+" = 0 FROM t"); got != "false" {
		t.Errorf("5e-16384 should round up to 1e-16383, got %s", got)
	}
}

// TestDecimalCostCeilingAbortsAheadOfBigMultiply: decimal_work is charged and GUARDED before
// the limb work runs (spec/design/cost.md §3/§6), so a ceiling aborts a pathological multiply
// up front (CLAUDE.md §13). ~20000 digits is ~5000 groups; the mul W is ~25,000,000.
func TestDecimalCostCeilingAbortsAheadOfBigMultiply(t *testing.T) {
	db := decDB(t, "CREATE TABLE t (id int32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	big := strings.Repeat("9", 20000) + ".5"
	db.SetMaxCost(1000)
	_, err := Execute(db, "SELECT "+big+" * "+big+" FROM t")
	if err == nil {
		t.Fatal("expected the cost ceiling to abort the multiply")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("want 54P01, got %v", err)
	}
}
