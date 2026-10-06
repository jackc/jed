package jed

// Host-API surface of the live query-memory budget (spec/design/memory.md §2/§5): the setting's
// default and normalization, cursor lifetime, and release of buffered result rows as a cursor yields
// them. Abort thresholds themselves are pinned in spec/conformance/suites/resource/query_memory.test.
// (The Go API has no engine-owned materializing collector — every row iterator yields to the host —
// so there is no collector admission to pin here.)

import "testing"

func TestQueryMemorySettingDefaultsToUnlimited(t *testing.T) {
	s := dbWith(t)
	if got := s.MaxQueryMemoryBytes(); got != 0 {
		t.Fatalf("default budget %d, want 0 (unlimited)", got)
	}
	s.SetMaxQueryMemoryBytes(4096)
	if got := s.MaxQueryMemoryBytes(); got != 4096 {
		t.Fatalf("budget %d, want 4096", got)
	}
	s.SetMaxQueryMemoryBytes(-1)
	if got := s.MaxQueryMemoryBytes(); got != 0 {
		t.Fatalf("negative budget reads %d, want 0 (unlimited)", got)
	}
	// Unlimited never aborts, even for a large materialized result.
	out, err := queryOutcome(s, "SELECT repeat('x', 1000) FROM generate_series(1, 2000)", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 2000 {
		t.Fatalf("got %d rows, want 2000", len(out.Rows))
	}
	opts := memDB().Session(SessionOptions{MaxQueryMemoryBytes: 1 << 20})
	if got := opts.MaxQueryMemoryBytes(); got != 1<<20 {
		t.Fatalf("SessionOptions budget %d, want %d", got, 1<<20)
	}
}

// A buffered result's rows leave the engine's account as the cursor yields them (memory.md §5.3).
func TestQueryMemoryBufferedRowsReleasedOnYield(t *testing.T) {
	s := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2)")
	// A streaming scan holds no row buffer: a budget of one byte suffices.
	s.SetMaxQueryMemoryBytes(1)
	if out, err := queryOutcome(s, "SELECT id FROM t", nil); err != nil || len(out.Rows) != 2 {
		t.Fatalf("streaming scan under a 1-byte budget: %v", err)
	}
	// EXPLAIN's rows are a materialized result admitted against the statement's account.
	s.SetMaxQueryMemoryBytes(1 << 20)
	rows, err := s.queryValues("EXPLAIN SELECT id FROM t ORDER BY -id", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	used := s.engine.session.queryBytes
	if *used == 0 {
		t.Fatal("EXPLAIN result rows were not admitted")
	}
	for rows.Next() {
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if *used != 0 {
		t.Fatalf("drained buffered result still holds %d bytes", *used)
	}
}

func TestQueryMemoryBudgetStaysWithCursor(t *testing.T) {
	db := memDB()
	s := db.Session(SessionOptions{MaxQueryMemoryBytes: 1 << 20})
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2)"} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatal(err)
		}
	}
	// A buffered (expression-sorted) result opened under 1 MiB runs its blocking part on the first
	// pull, and keeps that budget after the setting changes.
	old, err := s.queryValues("SELECT id FROM t ORDER BY -id", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer old.Close()
	s.SetMaxQueryMemoryBytes(1)
	_, err = queryOutcome(s, "SELECT id FROM t ORDER BY -id", nil)
	if e, ok := err.(*EngineError); !ok || e.Code() != "54P05" {
		t.Fatalf("want 54P05 under a 1-byte budget, got %v", err)
	}
	var got []int64
	for old.Next() {
		got = append(got, old.Row()[0].Int)
	}
	if err := old.Err(); err != nil {
		t.Fatalf("old cursor lost its budget: %v", err)
	}
	if !eqInts(got, 2, 1) {
		t.Fatalf("old cursor rows %v, want [2 1]", got)
	}
}
