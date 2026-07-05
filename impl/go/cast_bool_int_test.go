package jed

// boolean ⇄ i32 casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
// spec/design/types.md §9). The agreeing behavior (bool→i32, i32→bool, NULL, chains, the
// literal-adapts-to-i32 rule) is oracle-checked in suites/cast/bool_int.test and runs on every
// core; these per-core tests cover only what the oracle corpus CANNOT express (CLAUDE.md §10):
//
//   - the FORBIDDEN width pairs — PG ties the boolean↔integer cast to int4 ONLY, so bool⇄i16 and
//     bool⇄i64 are not casts. jed reports 42804 (datatype_mismatch — its standing convention for a
//     forbidden cast pair) where PG reports 42846 (cannot_coerce).
//   - the literal-beyond-i32 corner — CAST(5000000000 AS boolean) traps 22003 in jed (the literal
//     adapts to the i32 the bool cast needs and overflows it) where PG says 42846.
//
// Mirrors impl/rust/tests/cast_bool_int.rs.

import "testing"

func castErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

func castOne(t *testing.T, db dbHandle, sql string) Value {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 {
		t.Fatalf("%q: want 1 row, got %d", sql, len(out.Rows))
	}
	return out.Rows[0][0]
}

// bool → i16 and bool → i64 are forbidden (PG has only bool → int4): jed 42804, PG 42846.
func TestBoolToNonI32Forbidden(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		"SELECT CAST(TRUE AS i16)",
		"SELECT CAST(TRUE AS i64)",
		"SELECT CAST(FALSE AS smallint)",
		"SELECT TRUE::bigint",
	} {
		if code := castErrCode(t, db, sql); code != "42804" {
			t.Fatalf("%q: got %s, want 42804", sql, code)
		}
	}
}

// i16 → boolean and i64 → boolean are forbidden (PG has only int4 → bool): jed 42804, PG 42846.
// A column carries the width unambiguously (a bare literal would adapt to i32).
func TestNonI32ToBoolForbidden(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s i16, b i64)",
		"INSERT INTO t VALUES (1, 5, 9)",
	)
	for _, sql := range []string{
		"SELECT CAST(s AS boolean) FROM t WHERE id = 1",
		"SELECT b::boolean FROM t WHERE id = 1",
	} {
		if code := castErrCode(t, db, sql); code != "42804" {
			t.Fatalf("%q: got %s, want 42804", sql, code)
		}
	}
}

// An integer literal operand of a boolean target adapts to i32, so a magnitude beyond i32 range
// traps 22003 (PG reports 42846 — it types the literal as int8 first). A documented divergence.
func TestLiteralBeyondI32ToBoolOverflows(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		"SELECT CAST(5000000000 AS boolean)",
		"SELECT 5000000000::boolean",
	} {
		if code := castErrCode(t, db, sql); code != "22003" {
			t.Fatalf("%q: got %s, want 22003", sql, code)
		}
	}
}

// The headline directions still work here (a quick per-core smoke check alongside the divergences;
// the exhaustive behavior is in the corpus). true→1, false→0, 0→false, nonzero→true, NULL→NULL.
func TestBoolI32RoundTripSmoke(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if v := castOne(t, db, "SELECT CAST(TRUE AS i32)"); v.Kind != ValInt || v.Int != 1 {
		t.Fatalf("CAST(TRUE AS i32) = %v, want 1", v)
	}
	if v := castOne(t, db, "SELECT FALSE::int"); v.Kind != ValInt || v.Int != 0 {
		t.Fatalf("FALSE::int = %v, want 0", v)
	}
	if v := castOne(t, db, "SELECT CAST(0 AS boolean)"); v.Kind != ValBool || v.boolVal() {
		t.Fatalf("CAST(0 AS boolean) = %v, want false", v)
	}
	if v := castOne(t, db, "SELECT (-7)::boolean"); v.Kind != ValBool || !v.boolVal() {
		t.Fatalf("(-7)::boolean = %v, want true", v)
	}
	if v := castOne(t, db, "SELECT CAST(NULL AS boolean)"); v.Kind != ValNull {
		t.Fatalf("CAST(NULL AS boolean) = %v, want NULL", v)
	}
	if v := castOne(t, db, "SELECT 7::boolean::int"); v.Kind != ValInt || v.Int != 1 {
		t.Fatalf("7::boolean::int = %v, want 1", v)
	}
}
