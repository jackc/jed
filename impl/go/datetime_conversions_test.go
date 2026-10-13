package jed

// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   * EXTRACT(field FROM ±infinity) of a monotonic field (year, decade, century, millennium, julian,
//     isoyear, epoch) over a timestamp, timestamptz, or date — jed's decimal is finite-only, so
//     22003; PG returns numeric ±Infinity. (The oscillating fields are NULL, as in PG, and
//     date_part's float8 carries the ±Infinity — both oracle-checked in
//     suites/expr/extract_infinity.test.)
//   * EXTRACT(timezone* FROM ±infinity timestamp) — jed validates EXTRACT's field at resolve, so it
//     is 0A000 as for a finite timestamp; PG decides per value and returns NULL.
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

// EXTRACT of a monotonic field over an infinite timestamp/timestamptz/date traps 22003 (jed's
// decimal is finite-only); PG returns numeric ±Infinity.
func TestExtractMonotonicFieldFromInfinityTraps(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, field := range []string{"year", "decade", "century", "millennium", "julian", "isoyear", "epoch"} {
		for _, ty := range []string{"timestamp", "timestamptz", "date"} {
			for _, inf := range []string{"infinity", "-infinity"} {
				sql := "SELECT EXTRACT(" + field + " FROM " + ty + " '" + inf + "')"
				if c := dtErrCode(t, db, sql); c != "22003" {
					t.Fatalf("%s: got %s, want 22003", sql, c)
				}
			}
		}
	}
}

// EXTRACT(timezone* FROM an infinite timestamp) is 0A000 at resolve, like a finite timestamp's; PG
// returns NULL (its NonFiniteTimestampTzPart accepts every unit).
func TestExtractTimezoneFromInfiniteTimestampIsUnsupported(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, field := range []string{"timezone", "timezone_hour", "timezone_minute"} {
		sql := "SELECT EXTRACT(" + field + " FROM timestamp 'infinity')"
		if c := dtErrCode(t, db, sql); c != "0A000" {
			t.Fatalf("%s: got %s, want 0A000", sql, c)
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
