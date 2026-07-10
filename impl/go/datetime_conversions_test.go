package jed

// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   * EXTRACT(julian …) — jed defers the field (0A000); PG returns a value (timezones.md §9.2).
//   * date_part('field', …) — jed has no such function (42883); PG returns double precision, deferred.
//   * EXTRACT(field FROM ±infinity) — jed's decimal is finite-only, so 22003; PG returns ±Infinity.
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

func TestExtractJulianIsDeferred(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if c := dtErrCode(t, db, "SELECT EXTRACT(julian FROM timestamp '2024-03-15 00:00:00')"); c != "0A000" {
		t.Fatalf("julian/timestamp: got %s, want 0A000", c)
	}
	if c := dtErrCode(t, db, "SELECT EXTRACT(julian FROM date '2024-03-15')"); c != "0A000" {
		t.Fatalf("julian/date: got %s, want 0A000", c)
	}
}

func TestDatePartJulianIsDeferred(t *testing.T) {
	t.Parallel()
	// date_part has LANDED (suites/expr/date_part.test); julian stays EXTRACT's deferred field on
	// it too — 0A000 where PG computes a value (the documented divergence the oracle_overrides
	// ledger records; the timestamp overload cannot live in the PG-clean corpus).
	db := memDB().Session(SessionOptions{})
	if c := dtErrCode(t, db, "SELECT date_part('julian', timestamp '2024-03-15 13:00:00')"); c != "0A000" {
		t.Fatalf("date_part julian: got %s, want 0A000", c)
	}
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
}

func TestNonDatetimeSourceToDatetimeIsDeferred(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if c := dtErrCode(t, db, "SELECT CAST(1 + 1 AS timestamp)"); c != "0A000" {
		t.Fatalf("int->timestamp: got %s, want 0A000", c)
	}
	if c := dtErrCode(t, db, "SELECT CAST(current_setting('x.y', true) AS timestamptz)"); c != "0A000" {
		t.Fatalf("text->timestamptz: got %s, want 0A000", c)
	}
}
