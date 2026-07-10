package jed

// Clock-relative date literals — the parts the corpus cannot express (spec/design/date.md §6).
// The literal surface (all five specials, the STABLE statement clock, INSERT/UPDATE/DEFAULT, the
// 42P17 index rejections, and the strict INSERT…SELECT divergence) is corpus-tested with injected
// clocks (suites/types/date_clock.test, run on every core); this file covers only what that
// cannot reach: the SESSION-ZONE interaction (the corpus has no zone-setting directive — the
// session TimeZone is host-API-only, session.md §6.2) and the never-folded property observed
// across clock changes on ONE handle. Mirrors impl/rust/tests/date_clock.rs.

import "testing"

// clockDB builds a session over `stmts` with a FIXED injected clock (µs since the epoch).
func clockDB(t *testing.T, micros int64, stmts ...string) *Session {
	t.Helper()
	db := dbWith(t, stmts...)
	db.SetClockSource(FixedClock(micros))
	return db
}

// 2024-07-15 23:30:00 UTC — half an hour before a UTC midnight, so a positive-offset session
// zone is already on 2024-07-16 while UTC is still on 2024-07-15.
const nearMidnightUTC int64 = 1721086200000000

func dateAt(t *testing.T, db *Session, expr string) string {
	t.Helper()
	v := castOne(t, db, "SELECT ("+expr+") = '2024-07-16'::date")
	if v.Kind != ValBool {
		t.Fatalf("expected boolean, got %v", v)
	}
	if v.boolVal() {
		return "2024-07-16"
	}
	return "2024-07-15"
}

func TestDateClockUsesSessionZone(t *testing.T) {
	t.Parallel()
	db := clockDB(t, nearMidnightUTC)
	// Default UTC session: still 2024-07-15.
	if d := dateAt(t, db, "'today'::date"); d != "2024-07-15" {
		t.Fatalf("UTC today = %s", d)
	}
	// A UTC+2 session zone (the POSIX fixed-offset spelling '-02:00' — positive is WEST,
	// timezones.md §6): local wall clock is 01:30 on 2024-07-16.
	if err := db.SetTimeZone("-02:00"); err != nil {
		t.Fatal(err)
	}
	if d := dateAt(t, db, "'today'::date"); d != "2024-07-16" {
		t.Fatalf("UTC+2 today = %s", d)
	}
	// The runtime text→date cast consults the same zone.
	if _, err := queryOutcome(db, "CREATE TABLE s (id i32 PRIMARY KEY, w text)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(db, "INSERT INTO s VALUES (1, 'today')", nil); err != nil {
		t.Fatal(err)
	}
	if d := dateAt(t, db, "(SELECT w FROM s WHERE id = 1) :: date"); d != "2024-07-16" {
		t.Fatalf("UTC+2 cast today = %s", d)
	}
}

func TestDateClockDefaultNeverFolds(t *testing.T) {
	t.Parallel()
	// The DEFAULT is created under clock A; an INSERT under clock B (a day later) must see B's
	// day — a CREATE-TABLE-folded constant (PostgreSQL's behavior) could not. Same handle.
	db := clockDB(t, nearMidnightUTC,
		"CREATE TABLE d (id i32 PRIMARY KEY, dt date DEFAULT 'today')")
	if _, err := queryOutcome(db, "INSERT INTO d (id) VALUES (1)", nil); err != nil {
		t.Fatal(err)
	}
	db.SetClockSource(FixedClock(nearMidnightUTC + 86_400_000_000))
	if _, err := queryOutcome(db, "INSERT INTO d (id) VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	v := castOne(t, db, "SELECT (SELECT dt FROM d WHERE id = 2) = (SELECT dt FROM d WHERE id = 1) + 1")
	if v.Kind != ValBool || !v.boolVal() {
		t.Fatalf("DEFAULT 'today' folded: day 2 is not day 1 + 1 (%v)", v)
	}
}
