package jed

// Session-zone rendering — the parts the corpus cannot express (spec/design/timezones.md §9.5). The
// PG-agreeing surface (result cells in the record's zone, timestamptz nested in array / composite /
// range output and in array → text, its cost and 42P17) is oracle-checked in
// suites/types/timestamptz_session_render.test; this file covers the host render API — the canonical
// Value.Render stays UTC, Session.Render / Transaction.Render follow the session's current zone, and a
// render at jed's out-of-PG-range extremes is 22008 — plus the documented divergence that EXPLAIN keeps
// the canonical UTC spelling of a timestamptz constant. Mirrors
// impl/rust/tests/timestamptz_session_render.rs and impl/ts/tests/timestamptz_session_render.test.ts.

import (
	"strings"
	"testing"
)

// sessionRenderOne runs sql and returns the first column of its first row.
func sessionRenderOne(t *testing.T, db *Session, sql string) Value {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Rows[0][0]
}

// mustSessionRender renders v through the session and fails the test on an error.
func mustSessionRender(t *testing.T, db *Session, v Value) string {
	t.Helper()
	s, err := db.Render(v)
	if err != nil {
		t.Fatalf("render: %v", err)
	}
	return s
}

func TestSessionRenderFollowsTheCurrentZone(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	v := sessionRenderOne(t, db, "SELECT ARRAY[timestamptz '2024-01-15 12:00:00+00', timestamptz 'infinity']")
	// The default session zone is UTC: the session render equals the canonical render.
	if got := mustSessionRender(t, db, v); got != v.Render() {
		t.Fatalf("UTC session render %q, canonical %q", got, v.Render())
	}
	// The value is zone-free; the render reads the zone in effect when it is called. The POSIX
	// fixed-offset spelling '+05:00' is UTC-5.
	if err := db.SetTimeZone("+05:00"); err != nil {
		t.Fatal(err)
	}
	if got, want := mustSessionRender(t, db, v), `{"2024-01-15 07:00:00-05",infinity}`; got != want {
		t.Fatalf("+05:00: got %q, want %q", got, want)
	}
	if err := db.SetTimeZone("-05:30"); err != nil {
		t.Fatal(err)
	}
	if got, want := mustSessionRender(t, db, v), `{"2024-01-15 17:30:00+05:30",infinity}`; got != want {
		t.Fatalf("-05:30: got %q, want %q", got, want)
	}
	// The canonical render stays zone-free whatever the session zone.
	if got, want := v.Render(), `{"2024-01-15 12:00:00+00",infinity}`; got != want {
		t.Fatalf("canonical: got %q, want %q", got, want)
	}
	// A value with no timestamptz renders the same either way.
	if got := mustSessionRender(t, db, IntValue(7)); got != "7" {
		t.Fatalf("int: got %q", got)
	}
	if got := mustSessionRender(t, db, NullValue()); got != "NULL" {
		t.Fatalf("null: got %q", got)
	}
}

func TestTransactionRenderUsesTheOwningSessionZone(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	if err := db.SetTimeZone("+05:00"); err != nil {
		t.Fatal(err)
	}
	var text string
	err := db.Update(func(tx *Transaction) error {
		out, err := queryOutcome(tx, "SELECT timestamptz '2024-01-15 12:00:00+00'", nil)
		if err != nil {
			return err
		}
		text, err = tx.Render(out.Rows[0][0])
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	if want := "2024-01-15 07:00:00-05"; text != want {
		t.Fatalf("got %q, want %q", text, want)
	}
}

func TestSessionRenderAtTheInt64ExtremeIs22008(t *testing.T) {
	t.Parallel()
	// i64::MAX µs is 294247-01-10 04:00:54.775807 UTC — far outside PostgreSQL's range. A UTC-5
	// reading of a finite instant within five hours of it overflows (timezones.md §6).
	db := dbWith(t)
	v := sessionRenderOne(t, db, "SELECT timestamptz '294247-01-10 04:00:00+00'")
	if got, want := v.Render(), "294247-01-10 04:00:00+00"; got != want {
		t.Fatalf("canonical: got %q, want %q", got, want)
	}
	if err := db.SetTimeZone("-05:00"); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Render(v); errCodeOf(err) != "22008" {
		t.Fatalf("scalar: got %v, want 22008", err)
	}
	// Nested in a container, the same instant fails the whole render.
	arr := sessionRenderOne(t, db, "SELECT ARRAY[timestamptz '294247-01-10 04:00:00+00']")
	if _, err := db.Render(arr); errCodeOf(err) != "22008" {
		t.Fatalf("array: got %v, want 22008", err)
	}
}

func TestExplainKeepsTheCanonicalUTCConstant(t *testing.T) {
	t.Parallel()
	// A documented divergence: PostgreSQL deparses the constant in the session zone; jed's plan text
	// is context-free, so the constant prints in its canonical UTC spelling under any session zone.
	db := dbWith(t, "CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)")
	if err := db.SetTimeZone("+05:00"); err != nil {
		t.Fatal(err)
	}
	out, err := queryOutcome(db,
		"EXPLAIN (VERBOSE) SELECT id FROM ev WHERE at > timestamptz '2024-01-15 12:00:00+00'", nil)
	if err != nil {
		t.Fatal(err)
	}
	var lines []string
	for _, r := range out.Rows {
		for _, v := range r {
			if v.Kind == ValText {
				lines = append(lines, v.str())
			}
		}
	}
	text := strings.Join(lines, "\n")
	if !strings.Contains(text, "'2024-01-15 12:00:00+00'") {
		t.Fatalf("EXPLAIN should print the canonical UTC constant:\n%s", text)
	}
}
