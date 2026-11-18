package jed

// Phase 3: the exact decimal / numeric type — unit tests on the Decimal type and end-to-end
// tests through the query seam (spec/design/decimal.md). End-to-end assertions are on RENDERED output
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
	return decimalFromDigitsScale(neg, intPart+frac, uint32(len(frac)))
}

// mustRender renders the result of an arithmetic op, panicking on an unexpected error (so it
// can be called directly on a (Decimal, error) return — Go spreads the pair into the params).
func mustRender(d Decimal, err error) string {
	if err != nil {
		panic(err)
	}
	return d.String()
}

func TestDecimalRenderPreservesScale(t *testing.T) {
	t.Parallel()
	cases := map[string]string{
		"1.50": "1.50", "1.5": "1.5", "0.00": "0.00", "0": "0",
		"-0.013": "-0.013", "123": "123", ".5": "0.5", "100": "100",
	}
	for in, want := range cases {
		if got := dec(in).String(); got != want {
			t.Errorf("dec(%q).Render() = %q, want %q", in, got, want)
		}
	}
}

func TestDecimalNoNegativeZero(t *testing.T) {
	t.Parallel()
	for _, s := range []string{"0", "-0", "-0.00"} {
		if dec(s).neg {
			t.Errorf("dec(%q) should not be negative", s)
		}
	}
	r, _ := dec("1.0").subtract(dec("1.0"))
	if r.String() != "0.0" || r.neg {
		t.Errorf("1.0 - 1.0 = %q neg=%v, want 0.0 +0", r.String(), r.neg)
	}
}

func TestDecimalValueEqualityIgnoresScale(t *testing.T) {
	t.Parallel()
	if dec("1.5").Cmp(dec("1.50")) != 0 {
		t.Error("1.5 should equal 1.50 by value")
	}
	if dec("10").Cmp(dec("10.0")) != 0 {
		t.Error("10 should equal 10.0 by value")
	}
	if dec("1.5").Cmp(dec("1.6")) == 0 {
		t.Error("1.5 != 1.6")
	}
}

func TestDecimalOrdering(t *testing.T) {
	t.Parallel()
	asc := []string{"-10", "-1", "0", "0.5", "1", "10"}
	for i := 0; i+1 < len(asc); i++ {
		if dec(asc[i]).Cmp(dec(asc[i+1])) >= 0 {
			t.Errorf("%s should be < %s", asc[i], asc[i+1])
		}
	}
	if dec("1.23").Cmp(dec("1.2")) <= 0 {
		t.Error("1.23 should be > 1.2")
	}
}

func TestDecimalAddSubMul(t *testing.T) {
	t.Parallel()
	check := func(got, want string) {
		t.Helper()
		if got != want {
			t.Errorf("got %q, want %q", got, want)
		}
	}
	check(mustRender(dec("1.50").add(dec("1.5"))), "3.00")
	check(mustRender(dec("1.234").subtract(dec("1.2"))), "0.034")
	check(mustRender(dec("1.50").mul(dec("1.5"))), "2.250")
	check(mustRender(dec("2.0").mul(dec("3.000"))), "6.0000")
}

func TestDecimalDivisionScaleAndRounding(t *testing.T) {
	t.Parallel()
	cases := []struct{ a, b, want string }{
		{"1", "3", "0.33333333333333333333"},
		{"2", "3", "0.66666666666666666667"},
		{"1", "7", "0.14285714285714285714"},
		{"10.0", "4.0", "2.5000000000000000"},
		{"1.0", "8.0", "0.12500000000000000000"},
		{"100", "7", "14.2857142857142857"},
	}
	for _, c := range cases {
		if got := mustRender(dec(c.a).div(dec(c.b))); got != c.want {
			t.Errorf("%s / %s = %q, want %q", c.a, c.b, got, c.want)
		}
	}
}

func TestDecimalModulo(t *testing.T) {
	t.Parallel()
	cases := []struct{ a, b, want string }{
		{"5.5", "2", "1.5"},
		{"-5.5", "2", "-1.5"},
		{"5.50", "2.0", "1.50"},
	}
	for _, c := range cases {
		if got := mustRender(dec(c.a).rem(dec(c.b))); got != c.want {
			t.Errorf("%s %% %s = %q, want %q", c.a, c.b, got, c.want)
		}
	}
}

func TestDecimalRoundingHalfAway(t *testing.T) {
	t.Parallel()
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
		if got := dec(c.in).roundToScale(c.scale).String(); got != c.render {
			t.Errorf("round(%s, %d) = %q, want %q", c.in, c.scale, got, c.render)
		}
	}
}

func TestDecimalDivZeroTraps(t *testing.T) {
	t.Parallel()
	if _, err := dec("1").div(dec("0")); err == nil || err.(*EngineError).Code() != "22012" {
		t.Errorf("1/0 should trap 22012, got %v", err)
	}
	if _, err := dec("1").rem(dec("0")); err == nil || err.(*EngineError).Code() != "22012" {
		t.Errorf("1%%0 should trap 22012, got %v", err)
	}
}

func TestDecimalToInt64Round(t *testing.T) {
	t.Parallel()
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
		got, ok := dec(c.in).toInt64Round()
		if ok != c.ok || (ok && got != c.want) {
			t.Errorf("%s.ToInt64Round() = (%d,%v), want (%d,%v)", c.in, got, ok, c.want, c.ok)
		}
	}
}

func TestDecimalCodecRoundTrip(t *testing.T) {
	t.Parallel()
	for _, s := range []string{"0", "1.50", "-12345.6789", "100000000.000001", "999999999999"} {
		d := dec(s)
		neg, scale, groups := d.toCodec()
		back := decimalFromCodec(neg, scale, groups)
		if back.String() != d.String() {
			t.Errorf("codec round trip %s = %q", s, back.String())
		}
	}
	if _, _, groups := dec("0.00").toCodec(); len(groups) != 0 {
		t.Error("zero should carry no groups")
	}
}

func TestDecimalBigMultiplicationExact(t *testing.T) {
	t.Parallel()
	// 38-digit * 38-digit (76 digits) fits no int128; the limb path is exact.
	a := dec("12345678901234567890123456789012345678")
	b := dec("99999999999999999999999999999999999999")
	r, err := a.mul(b)
	if err != nil || r.precision() != 76 {
		t.Errorf("product precision = %d (err %v), want 76", r.precision(), err)
	}
}

// --- end-to-end through the query seam ---------------------------------------------

func decExec(t *testing.T, db dbHandle, sql string) {
	t.Helper()
	if _, err := queryOutcome(db, sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func decDB(t *testing.T, stmts ...string) *Session {
	t.Helper()
	db := mustSession(memDB(), SessionOptions{})
	for _, s := range stmts {
		decExec(t, db, s)
	}
	return db
}

// decOne runs a query expected to return a single cell, rendered.
func decOne(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	rows := query(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%q: expected one cell, got %v", sql, rows)
	}
	return rows[0][0].String()
}

func TestDecimalOnDiskRoundTripEndToEnd(t *testing.T) {
	t.Parallel()
	db := decDB(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, money numeric(10,2), free numeric)",
		"INSERT INTO t VALUES (1, 1.5, -12345.6789), (2, 0, 0.00), (3, 100, NULL)",
	)
	image, err := db.toImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := loadEngine(image, NewEnvironment())
	if err != nil {
		t.Fatal(err)
	}
	again, _ := loaded.toImage(8192, 1)
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

// TestSumAccumulatorChecksOnlyFinalCap pins the SUM/AVG accumulator's AddUncapped path
// (spec/design/decimal.md §2, determinism.md §7): the running sum may cross the §2 format cap
// mid-fold without trapping; only the FINAL result is cap-checked — the order-independent-trap
// fix. Too large to reach through SQL literals (a 131072-digit value is ~74 KB), so pinned here.
// a is exactly at the cap (131072 nines); a + a is one digit over it.
func TestSumAccumulatorChecksOnlyFinalCap(t *testing.T) {
	t.Parallel()
	a := decimalFromDigitsScale(false, strings.Repeat("9", decimalMaxIntDigits), 0)
	if _, err := a.checkCap(); err != nil {
		t.Fatalf("a should be exactly at the cap, got %v", err)
	}
	// Capped Add (standalone arithmetic) still traps at the cap — unchanged contract.
	if _, err := a.add(a); err == nil || err.(*EngineError).Code() != "22003" {
		t.Fatalf("a + a (capped) = %v, want 22003", err)
	}
	// Uncapped fold may exceed the cap intermediately and NOT trap...
	over := a.addUncapped(a) // 2·a, one digit over the cap
	// ...then come back in range, so the FINAL check passes and the value is exact.
	back, err := over.addUncapped(a.negate()).checkCap()
	if err != nil {
		t.Fatalf("sum back in range should pass the final cap, got %v", err)
	}
	if back.Cmp(a) != 0 {
		t.Errorf("folded value = %s, want a", back.String())
	}
	// A final result genuinely over the cap still traps 22003 (PG's make_result).
	if _, err := over.checkCap(); err == nil || err.(*EngineError).Code() != "22003" {
		t.Fatalf("over-cap final = %v, want 22003", err)
	}
}

// TestDecimalMulRoundsAtMaxScale pins PG numeric_mul's rounding: an exact product whose scale
// exceeds max_scale (16383) ROUNDS to it, half away from zero, instead of trapping
// (spec/design/decimal.md §2).
func TestDecimalMulRoundsAtMaxScale(t *testing.T) {
	t.Parallel()
	db := decDB(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
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
	t.Parallel()
	db := decDB(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	big := strings.Repeat("9", 20000) + ".5"
	db.setMaxCostRaw(1000)
	_, err := queryOutcome(db, "SELECT "+big+" * "+big+" FROM t", nil)
	if err == nil {
		t.Fatal("expected the cost ceiling to abort the multiply")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("want 54P01, got %v", err)
	}
}

// TestDecimalEncodeKeyOrderPreserving checks that EncodeKey produces order-preserving keys
// (byte-comparable order == numeric order) and is scale-independent (1.5 and 1.50 coincide).
// A per-core byte-level test — the corpus cannot assert encoded key bytes (encoding.md §2.5).
func TestDecimalEncodeKeyOrderPreserving(t *testing.T) {
	t.Parallel()
	ss := []string{
		"-12345.6789", "-100", "-10", "-1.5", "-1", "-0.5", "-0.05", "-0.001",
		"0", "0.001", "0.05", "0.5", "1", "1.5", "1.50", "5", "10", "12", "50",
		"100", "101", "123", "1000", "12345.6789", "99999999999999999999",
	}
	// Values in ascending numeric order (ss is authored sorted, except the 1.5/1.50 duplicate).
	byKey := make([]Decimal, len(ss))
	for i, s := range ss {
		byKey[i] = dec(s)
	}
	sortDecimalsByKey(byKey)
	// The expected order is ss with the duplicate (1.5 == 1.50) collapsed in place; assert each
	// adjacent pair is non-decreasing by CmpValue and that key order agrees.
	for i := 1; i < len(byKey); i++ {
		if byKey[i-1].Cmp(byKey[i]) > 0 {
			t.Fatalf("key order disagrees with value order at %d: %s before %s",
				i, byKey[i-1].String(), byKey[i].String())
		}
	}
	// Scale-independence: equal values produce identical key bytes.
	for _, p := range [][2]string{{"1.5", "1.50"}, {"100", "100.00"}, {"0", "0.000"}} {
		if !bytesEqual(dec(p[0]).encodeKey(), dec(p[1]).encodeKey()) {
			t.Fatalf("scale-independence broken: %s vs %s", p[0], p[1])
		}
	}
	// Zero is the single class byte; negatives sort below it, positives above.
	if z := dec("0").encodeKey(); len(z) != 1 || z[0] != 0x04 {
		t.Fatalf("zero key = %v, want [4]", z)
	}
	if !(bytesCompare(dec("-1").encodeKey(), dec("0").encodeKey()) < 0) ||
		!(bytesCompare(dec("0").encodeKey(), dec("1").encodeKey()) < 0) {
		t.Fatalf("sign-class ordering broken")
	}
}

func sortDecimalsByKey(ds []Decimal) {
	for i := 1; i < len(ds); i++ {
		for j := i; j > 0 && bytesCompare(ds[j-1].encodeKey(), ds[j].encodeKey()) > 0; j-- {
			ds[j-1], ds[j] = ds[j], ds[j-1]
		}
	}
}

func bytesEqual(a, b []byte) bool { return bytesCompare(a, b) == 0 }

func bytesCompare(a, b []byte) int {
	for i := 0; i < len(a) && i < len(b); i++ {
		if a[i] != b[i] {
			if a[i] < b[i] {
				return -1
			}
			return 1
		}
	}
	switch {
	case len(a) < len(b):
		return -1
	case len(a) > len(b):
		return 1
	}
	return 0
}

// TestDecimalEncodeKeyExactBytes pins the exact key bytes — the cross-core contract (the same
// literals are asserted in Rust and re-derived by spec/encoding/verify.rb).
func TestDecimalEncodeKeyExactBytes(t *testing.T) {
	t.Parallel()
	want := map[string][]byte{
		"0":    {0x04},
		"1.5":  {0x05, 0x80, 0x00, 0x00, 0x01, 0x02, 0x33, 0x00},
		"1.50": {0x05, 0x80, 0x00, 0x00, 0x01, 0x02, 0x33, 0x00},
		"100":  {0x05, 0x80, 0x00, 0x00, 0x02, 0x02, 0x00},
		"-1.5": {0x03, 0x7F, 0xFF, 0xFF, 0xFE, 0xFD, 0xCC, 0xFF},
	}
	for s, w := range want {
		if got := dec(s).encodeKey(); !bytesEqual(got, w) {
			t.Errorf("EncodeKey(%s) = % x, want % x", s, got, w)
		}
	}
}
