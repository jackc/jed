package jed

// Offset-less timestamptz input in the session zone — the parts the corpus cannot express
// (spec/design/timezones.md §9.3). The PG-agreeing surface (literals, casts, assignment, DEFAULT,
// array/range/record text) is oracle-checked in suites/types/timestamptz_session_input.test; this
// file covers the two deliberate divergences — a prepared statement re-reads the session zone on
// every execution (PG freezes the prepare-time zone) and an index expression or predicate holding
// an offset-less literal is 42P17 (PG folds it into the definition) — plus a text value bound to a
// timestamptz parameter (host-API surface). Mirrors impl/rust/tests/timestamptz_session_input.rs.

import "testing"

// sessionInputDB builds a UTC session over ev with 12:00 UTC (id 1) and 10:00 UTC (id 2) — the
// latter is 12:00 at UTC+2.
func sessionInputDB(t *testing.T) *Session {
	t.Helper()
	return dbWith(t,
		"CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)",
		"INSERT INTO ev VALUES (1, '2024-07-15 12:00:00+00'), (2, '2024-07-15 10:00:00+00')",
	)
}

// preparedIDs runs a prepared statement and returns its first column as integers.
func preparedIDs(t *testing.T, db *Session, stmt *PreparedStatement, params ...Value) []int64 {
	t.Helper()
	out, err := prepOutcome(db, stmt, params)
	if err != nil {
		t.Fatal(err)
	}
	got := make([]int64, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Int
	}
	return got
}

func mustPrepare(t *testing.T, db *Session, sql string) *PreparedStatement {
	t.Helper()
	stmt, err := db.Prepare(sql)
	if err != nil {
		t.Fatalf("prepare %q: %v", sql, err)
	}
	return stmt
}

func TestPreparedStatementRereadsTheSessionZone(t *testing.T) {
	t.Parallel()
	db := sessionInputDB(t)
	stmt := mustPrepare(t, db, "SELECT id FROM ev WHERE at = '2024-07-15 12:00:00'")
	if got := preparedIDs(t, db, stmt); !eqInts(got, 1) {
		t.Fatalf("UTC session: %v", got)
	}
	// The POSIX fixed-offset spelling '-02:00' is UTC+2 (timezones.md §6): 12:00 local = 10:00 UTC.
	if err := db.SetTimeZone("-02:00"); err != nil {
		t.Fatal(err)
	}
	if got := preparedIDs(t, db, stmt); !eqInts(got, 2) {
		t.Fatalf("a cached plan must not keep the instant folded under the prepare-time zone: %v", got)
	}
	// An explicit offset does not depend on the zone either way.
	fixed := mustPrepare(t, db, "SELECT id FROM ev WHERE at = '2024-07-15 12:00:00+00'")
	if got := preparedIDs(t, db, fixed); !eqInts(got, 1) {
		t.Fatalf("explicit offset: %v", got)
	}
}

func TestPreparedInsertRereadsTheSessionZone(t *testing.T) {
	t.Parallel()
	db := sessionInputDB(t)
	ins := mustPrepare(t, db, "INSERT INTO ev VALUES ($1, '2024-07-15 12:00:00')")
	preparedIDs(t, db, ins, IntValue(3))
	if err := db.SetTimeZone("-02:00"); err != nil {
		t.Fatal(err)
	}
	preparedIDs(t, db, ins, IntValue(4))
	if err := db.SetTimeZone("UTC"); err != nil {
		t.Fatal(err)
	}
	q := mustPrepare(t, db, "SELECT id FROM ev WHERE at = '2024-07-15 10:00:00' ORDER BY id")
	if got := preparedIDs(t, db, q); !eqInts(got, 2, 4) {
		t.Fatalf("got %v", got)
	}
}

func TestBoundTextParameterReadsTheSessionZone(t *testing.T) {
	t.Parallel()
	db := sessionInputDB(t)
	if err := db.SetTimeZone("-02:00"); err != nil {
		t.Fatal(err)
	}
	stmt := mustPrepare(t, db, "SELECT id FROM ev WHERE at = $1")
	if got := preparedIDs(t, db, stmt, TextValue("2024-07-15 12:00:00")); !eqInts(got, 2) {
		t.Fatalf("offset-less: %v", got)
	}
	if got := preparedIDs(t, db, stmt, TextValue("2024-07-15 12:00:00+00")); !eqInts(got, 1) {
		t.Fatalf("explicit offset: %v", got)
	}
}

func TestOffsetLessLiteralInAnIndexIs42P17(t *testing.T) {
	t.Parallel()
	db := sessionInputDB(t)
	for _, sql := range []string{
		"CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00'",
		"CREATE INDEX ON ev ((at > timestamptz '2024-01-01'))",
		`CREATE INDEX ON ev (id) WHERE at < ('{"2024-01-01 00:00:00"}'::timestamptz[])[1]`,
	} {
		if code := errCode(t, db, sql); code != "42P17" {
			t.Fatalf("%s: got %s, want 42P17", sql, code)
		}
	}
	// An explicit offset never consults the zone, so it stays immutable and indexable.
	mustExec(t, db, "CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00+00'")
	mustExec(t, db, "CREATE INDEX ON ev ((at > timestamptz 'infinity'))")
}
