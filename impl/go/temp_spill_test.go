package jed

// Temp-table spill-to-disk (spec/design/temp-tables.md §6, slice 3): once a session-local temp domain's
// page bytes outgrow tempBuffers on a host with a spill directory, the domain migrates from its in-RAM
// store to a scratch file and its page cache is bounded by tempBuffers. These per-core tests reach what
// the corpus cannot (the corpus runs in-memory databases, which have no spill target): the residency
// flip itself, the bounded pool, the unlinked scratch file, and the results/cost/54P03 neutrality of
// spilling. Mirrors Rust's temp_spill_internal_tests (shared.rs) and TS temp_spill.test.ts.

import (
	"fmt"
	"os"
	"reflect"
	"runtime"
	"strings"
	"testing"
)

var tempSpillWorkload = []string{
	"CREATE TEMP TABLE s (id i32 PRIMARY KEY, grp i32, pad text)",
	"CREATE INDEX s_grp ON s (grp)",
	"INSERT INTO s SELECT g, g % 7, repeat('x', 40 + g % 13) FROM generate_series(1, 400) AS g",
	"UPDATE s SET pad = repeat('y', 3 + id % 9) WHERE id % 5 = 0",
	"DELETE FROM s WHERE id % 11 = 0",
	"INSERT INTO s SELECT g, g % 7, repeat('z', g % 17) FROM generate_series(401, 600) AS g",
}

var tempSpillQueries = []string{
	"SELECT count(*), sum(id) FROM s",
	"SELECT id, pad FROM s WHERE id = 250",
	"SELECT id FROM s WHERE grp = 3 ORDER BY id",
	"SELECT pad FROM s ORDER BY id DESC LIMIT 5",
	"SELECT grp, count(*) FROM s GROUP BY grp ORDER BY grp",
}

// tempSpillResult is one statement's observable result: its rows (nil for a statement) and cost.
type tempSpillResult struct {
	rows [][]Value
	cost int64
}

// spillDB is an in-memory database at page size 256 given dir as its spill target.
func spillDB(dir string) *Database {
	db := newInMemoryWithPageSize(256)
	if dir != "" {
		db.setSpillDirForTest(dir)
	}
	return db
}

// runTempSpill runs the workload + queries under tempBuffers (with no storage ceiling), returning each
// statement's rows and cost, whether the domain spilled, and the session (still open).
func runTempSpill(t *testing.T, db *Database, tempBuffers int) ([]tempSpillResult, bool, *Session) {
	t.Helper()
	unlimited := 0
	sess := db.Session(SessionOptions{TempBuffers: &tempBuffers, MaxTempStorageBytes: &unlimited})
	var out []tempSpillResult
	for _, sql := range tempSpillWorkload {
		o, err := queryOutcome(sess, sql, nil)
		if err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
		out = append(out, tempSpillResult{cost: o.Cost})
	}
	for _, sql := range tempSpillQueries {
		o, err := queryOutcome(sess, sql, nil)
		if err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
		if o.Kind != outcomeQuery {
			t.Fatalf("%s: expected a query result", sql)
		}
		out = append(out, tempSpillResult{rows: o.Rows, cost: o.Cost})
	}
	return out, sess.engine.tempStorage.spilled, sess
}

func dirEntries(t *testing.T, dir string) int {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func TestSpilledTempDomainMatchesResidentRowsAndCost(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	resident, residentSpilled, a := runTempSpill(t, spillDB(dir), 0)
	defer a.Close()
	if residentSpilled {
		t.Fatal("tempBuffers = 0 must never spill")
	}
	spilled, didSpill, sess := runTempSpill(t, spillDB(dir), 4*256)
	defer sess.Close()
	if !didSpill {
		t.Fatal("a temp domain past tempBuffers should spill")
	}
	if !reflect.DeepEqual(resident, spilled) {
		t.Fatalf("spilling changed rows or cost:\nresident %v\nspilled  %v", resident, spilled)
	}
	// The page cache is bounded by tempBuffers (4 leaves at page 256), not pinned.
	paging := sess.engine.tempStorage.paging
	if got := paging.capacity(); got != 4 {
		t.Fatalf("spilled pool capacity = %d, want 4", got)
	}
	if got := paging.residentLeaves(); got > 4 {
		t.Fatalf("spilled pool holds %d leaves, want ≤ 4", got)
	}
}

func TestTempSpillNeedsASpillTarget(t *testing.T) {
	t.Parallel()
	// An in-memory database has no spill directory, so temp stays resident past tempBuffers.
	out, spilled, sess := runTempSpill(t, spillDB(""), 4*256)
	defer sess.Close()
	if spilled {
		t.Fatal("a temp domain with no spill target must not spill")
	}
	if len(out) != len(tempSpillWorkload)+len(tempSpillQueries) {
		t.Fatalf("want %d results, got %d", len(tempSpillWorkload)+len(tempSpillQueries), len(out))
	}
}

func TestTempScratchFileIsUnlinkedAndReleased(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	_, didSpill, sess := runTempSpill(t, spillDB(dir), 4*256)
	if !didSpill {
		t.Fatal("a temp domain past tempBuffers should spill")
	}
	if runtime.GOOS != "windows" {
		if n := dirEntries(t, dir); n != 0 {
			t.Fatalf("the scratch file should be unlinked while open; %d entries remain", n)
		}
	}
	sess.Close()
	if sess.engine.tempStorage != nil {
		t.Fatal("Session.Close should release the temp domain")
	}
	if n := dirEntries(t, dir); n != 0 {
		t.Fatalf("nothing should remain after the session closes; %d entries remain", n)
	}
}

func TestTempSpillDoesNotMoveThe54P03Point(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	abortAt := func(tempBuffers int) (int, bool) {
		ceiling := 40 * 256
		sess := spillDB(dir).Session(SessionOptions{TempBuffers: &tempBuffers, MaxTempStorageBytes: &ceiling})
		defer sess.Close()
		sessExec(t, sess, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)")
		pad := strings.Repeat("z", 40)
		for i := 1; i <= 1000; i++ {
			_, err := queryOutcome(sess, fmt.Sprintf("INSERT INTO lt VALUES (%d, 'r-%s')", i, pad), nil)
			if err != nil {
				if code := err.(*EngineError).Code(); code != "54P03" {
					t.Fatalf("insert %d: want 54P03, got %s (%v)", i, code, err)
				}
				return i, sess.engine.tempStorage.spilled
			}
		}
		t.Fatal("never hit 54P03")
		return 0, false
	}
	residentAt, residentSpilled := abortAt(0)
	spilledAt, didSpill := abortAt(2 * 256)
	if residentSpilled {
		t.Fatal("tempBuffers = 0 must never spill")
	}
	if !didSpill {
		t.Fatal("a temp domain past tempBuffers should spill")
	}
	if residentAt != spilledAt {
		t.Fatalf("54P03 at insert %d resident, %d spilled — spilling moved the abort point", residentAt, spilledAt)
	}
}

func TestTempPoolCapacityFollowsALaterTempBuffersChange(t *testing.T) {
	t.Parallel()
	_, didSpill, sess := runTempSpill(t, spillDB(t.TempDir()), 4*256)
	defer sess.Close()
	if !didSpill {
		t.Fatal("a temp domain past tempBuffers should spill")
	}
	sess.SetTempBuffers(16 * 256)
	sessExec(t, sess, "INSERT INTO s VALUES (100000, 1, 'x')")
	if got := sess.engine.tempStorage.paging.capacity(); got != 16 {
		t.Fatalf("spilled pool capacity = %d after the change, want 16", got)
	}
}
