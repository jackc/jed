package jed

// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   * EXTRACT(field FROM ±infinity) (timestamp, timestamptz, or date) — jed's decimal is finite-only,
//     so 22003; PG returns ±Infinity (or NULL for an oscillating field). date_part follows EXTRACT
//     (22003 where PG returns float ±Infinity).
//   * a non-datetime / non-literal-text source to a datetime target — jed 0A000 (text→datetime is a
//     valid PG cast; int→datetime is PG 42846).

import "testing"

func dtErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

func TestExtractFromInfinityTraps(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if c := dtErrCode(t, db, "SELECT EXTRACT(year FROM timestamp 'infinity')"); c != "22003" {
		t.Fatalf("extract year/infinity: got %s, want 22003", c)
	}
	if c := dtErrCode(t, db, "SELECT EXTRACT(epoch FROM timestamptz '-infinity')"); c != "22003" {
		t.Fatalf("extract epoch/-infinity: got %s, want 22003", c)
	}
	for _, sql := range []string{
		"SELECT EXTRACT(julian FROM timestamp 'infinity')",
		"SELECT EXTRACT(julian FROM date '-infinity')",
		"SELECT EXTRACT(year FROM date 'infinity')",
		"SELECT EXTRACT(day FROM date '-infinity')",
		"SELECT date_part('julian', timestamp '-infinity')",
		"SELECT date_part('julian', timestamptz 'infinity')",
		"SELECT date_part('julian', date 'infinity')",
	} {
		if c := dtErrCode(t, db, sql); c != "22003" {
			t.Fatalf("%s: got %s, want 22003", sql, c)
		}
	}
}

func TestNonDatetimeSourceToDatetimeIsForbidden(t *testing.T) {
	t.Parallel()
	// No integer → timestamp cast exists: jed's strict-matrix 42804 (PG 42846). (Runtime text →
	// datetime has landed — cast_text_string_native_test.go.)
	db := memDB().Session(SessionOptions{})
	if c := dtErrCode(t, db, "SELECT CAST(1 + 1 AS timestamp)"); c != "42804" {
		t.Fatalf("int->timestamp: got %s, want 42804", c)
	}
}
