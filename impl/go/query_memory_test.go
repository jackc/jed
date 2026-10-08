package jed

// Host-API surface of the live query-memory budget (spec/design/memory.md §2/§5): the setting's
// default and normalization, cursor lifetime, and release of buffered result rows as a cursor yields
// them. Abort thresholds themselves are pinned in spec/conformance/suites/resource/query_memory.test.
// (The Go API has no engine-owned materializing collector — every row iterator yields to the host —
// so there is no collector admission to pin here.)

import (
	"path/filepath"
	"testing"
)

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
	// The columnar projection lane gathers the id column (2 values × 32 = 64 bytes of lane state,
	// memory.md §6.5), held until its emission completes; the yielded rows are the host's own memory.
	s.SetMaxQueryMemoryBytes(64)
	if out, err := queryOutcome(s, "SELECT id FROM t", nil); err != nil || len(out.Rows) != 2 {
		t.Fatalf("columnar scan under its lane budget: %v", err)
	}
	s.SetMaxQueryMemoryBytes(63)
	if _, err := queryOutcome(s, "SELECT id FROM t", nil); err == nil || err.(*EngineError).Code() != "54P05" {
		t.Fatalf("columnar scan one byte under its lane budget: want 54P05, got %v", err)
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

// Spill-capable operator state charges only its resident portion (memory.md §6.6): on a file-backed
// database the bounded-spill lane's structures release their charge as they spill, so a budget that
// a fully resident DISTINCT exceeds is enough once work_mem makes it spill — and the account forces
// that spill itself when work_mem would not. Only work_mem = 0 (never spill) fails. Disk-only
// behavior and host-observed peaks, so it lives here rather than in the corpus.
func TestQueryMemorySpillingOperatorStateReleasesItsCharge(t *testing.T) {
	dir := t.TempDir()
	db, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "query_memory_spill.jed"), SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.setSpillDirForTest(dir)
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, s text)")
	mustExec(t, s, "INSERT INTO t SELECT g, repeat('x', g) FROM generate_series(1, 300) g")
	const sql = "SELECT DISTINCT s FROM t"
	peak := func(workMem int) int64 {
		s.SetWorkMem(workMem)
		s.SetMaxQueryMemoryBytes(1 << 40)
		ResetQueryMemoryPeak()
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatal(err)
		}
		return QueryMemoryPeak()
	}
	resident := peak(1 << 30)
	spilling := peak(4096)
	if spilling >= resident/4 {
		t.Fatalf("spilling peak %d vs resident %d", spilling, resident)
	}
	// The spilling run fits a budget the resident run exceeds.
	s.SetMaxQueryMemoryBytes(spilling)
	s.SetWorkMem(4096)
	out, err := queryOutcome(s, sql, nil)
	if err != nil || len(out.Rows) != 300 {
		t.Fatalf("spilling DISTINCT under its peak budget: %d rows, %v", len(out.Rows), err)
	}
	// With a work_mem the resident DISTINCT fits, the rejected reservation spills it instead.
	s.SetWorkMem(1 << 30)
	out, err = queryOutcome(s, sql, nil)
	if err != nil || len(out.Rows) != 300 {
		t.Fatalf("resident-work_mem DISTINCT under the spilling budget: %d rows, %v", len(out.Rows), err)
	}
	s.SetWorkMem(0)
	if _, err := queryOutcome(s, sql, nil); err == nil || err.(*EngineError).Code() != "54P05" {
		t.Fatalf("never-spill DISTINCT under the spilling budget: want 54P05, got %v", err)
	}
}

// A statement's account opens holding its transaction's pending writes (memory.md §7): a release past
// the statement's own reservations is an accounting bug, clamped at that floor and counted.
func TestQueryMemoryReleaseNeverDipsIntoThePendingWriteFloor(t *testing.T) {
	used := int64(30)
	acct := queryAccount{used: &used, limit: 100, floor: 30}
	if err := acct.reserve(20); err != nil {
		t.Fatal(err)
	}
	before := queryMemoryUnderflows.Load()
	acct.release(25)
	if used != 30 || queryMemoryUnderflows.Load() != before+1 {
		t.Fatalf("over-release: used %d, underflows %d (was %d)", used, queryMemoryUnderflows.Load(), before)
	}
	if err := acct.reserve(10); err != nil {
		t.Fatal(err)
	}
	acct.release(10)
	if used != 30 || queryMemoryUnderflows.Load() != before+1 {
		t.Fatalf("matched release: used %d, underflows %d", used, queryMemoryUnderflows.Load())
	}
}
