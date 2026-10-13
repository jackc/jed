package jed

// Runtime text → timestamp / timestamptz / interval / bytea casts — the parts the PG-clean oracle
// corpus cannot express (spec/types/casts.toml, spec/design/types.md §5). The accepted grammars
// AGREE with PostgreSQL and are oracle-checked in suites/cast/text_to_string_native.test (run on
// every core); this file covers only the DIVERGENCES: the jed-stricter input grammars (identical to
// the literal path), the SQLSTATE subcodes that differ from PG, the forbidden-pair convention
// (42804 where PG reports 42846), the int64 guard on the session-zone shift, and $1::interval.
// Every cast is on a NON-LITERAL text column, so it exercises the per-row eval path, not the
// resolve-time literal fold. Mirrors impl/rust/tests/cast_text_string_native.rs.

import "testing"

func TestRuntimeTextToStringNativeGrammarDivergences(t *testing.T) {
	t.Parallel()
	cases := []struct {
		s, expr, want string
	}{
		{"now", "s :: timestamp", "22007"},                      // PG: the statement time; jed: no clock specials
		{"today", "s :: timestamptz", "22007"},                  // PG: midnight today; jed: no clock specials
		{"epoch", "s :: timestamp", "22007"},                    // PG: 1970-01-01 00:00:00; jed: not accepted
		{"01/15/2024 12:00", "s :: timestamp", "22007"},         // PG (DateStyle MDY) accepts; jed: ISO only
		{"2024-01-01 12:00:00+16", "s :: timestamptz", "22008"}, // PG: 22009 time zone displacement
		{"2147483648 days", "s :: interval", "22008"},           // PG: 22015 interval field overflow
		{`\x0`, "s :: bytea", "22P02"},                          // PG: 22023 odd number of hex digits
		{`\xzz`, "s :: bytea", "22P02"},                         // PG: 22023 invalid hexadecimal digit
	}
	for _, c := range cases {
		db := seededText(t, c.s)
		if code := castErrAt(t, db, c.expr, 1); code != c.want {
			t.Fatalf("%q %s: got %s, want %s", c.s, c.expr, code, c.want)
		}
	}
}

func TestForbiddenSourceToStringNativeIs42804(t *testing.T) {
	t.Parallel()
	// No integer → interval / timestamp / bytea cast exists: jed's strict-matrix 42804 (PG 42846).
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for _, sql := range []string{
		"SELECT 1::interval",
		"SELECT CAST(1 + 1 AS timestamp)",
		"SELECT 1::timestamptz",
		"SELECT 1::bytea",
		"SELECT (DATE '2024-01-01')::interval",
	} {
		if code := paramErrCode(t, db, sql); code != "42804" {
			t.Fatalf("%s: got %s, want 42804", sql, code)
		}
	}
}

func TestRuntimeTextToTimestamptzZoneShiftOverflowTraps(t *testing.T) {
	t.Parallel()
	// A wall clock just below jed's int64 ceiling, read in a UTC-5 session (the POSIX fixed-offset
	// spelling '+05:00' — positive is WEST), shifts past int64: 22008, never a wrapped instant.
	db := seededText(t, "294247-01-10 04:00:00")
	if err := db.SetTimeZone("+05:00"); err != nil {
		t.Fatal(err)
	}
	if code := castErrAt(t, db, "s :: timestamptz", 1); code != "22008" {
		t.Fatalf("got %s, want 22008", code)
	}
	// The same wall clock in UTC is representable.
	if err := db.SetTimeZone("UTC"); err != nil {
		t.Fatal(err)
	}
	if v := castAt(t, db, "s :: timestamptz", 1); v.Kind != ValTimestamptz {
		t.Fatalf("UTC: got %v", v)
	}
}

func TestRuntimeTextToTimestamptzReadsFixedOffsetSessionZone(t *testing.T) {
	t.Parallel()
	// A fixed-offset session zone needs no tz data: '-02:00' is UTC+2, so 12:00 local is 10:00 UTC.
	db := seededText(t, "2024-07-15 12:00:00", "2024-07-15 12:00:00+00")
	if err := db.SetTimeZone("-02:00"); err != nil {
		t.Fatal(err)
	}
	if got := castAt(t, db, "(s :: timestamptz) AT TIME ZONE 'UTC'", 1).Render(); got != "2024-07-15 10:00:00" {
		t.Fatalf("offset-less: got %s", got)
	}
	if got := castAt(t, db, "(s :: timestamptz) AT TIME ZONE 'UTC'", 2).Render(); got != "2024-07-15 12:00:00" {
		t.Fatalf("explicit offset: got %s", got)
	}
}

func TestParamCastToIntervalTypesTheParameter(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rows := queryRows(t, db, "SELECT $1::interval", IntervalValue(Interval{Days: 3}))
	if got := rows[0][0].Render(); got != "3 days" {
		t.Fatalf("$1::interval = %s, want 3 days", got)
	}
}
