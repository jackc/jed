package jed

// External merge sort with spill-to-disk for ORDER BY (spec/design/spill.md). Spill is NOT a §8
// byte contract (it changes WHEN rows are resident, never WHAT a query observes — like the buffer
// pool), so it is verified per-core, not in the conformance corpus: a file-backed database sorting
// under a tiny workMem (which forces many sorted runs to spill + a k-way merge) must return
// byte-identical rows and cost to the same query run fully in memory. These tests pin that
// invariance across several ORDER BY shapes, the stable-sort tie-break the merge must reproduce, and
// that no spill temp file leaks.

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// runQuery runs sql and returns (rows, cost).
func runQuery(t *testing.T, db dbHandle, sql string) ([][]Value, int64) {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("query %q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("query %q: not a query result", sql)
	}
	return out.Rows, out.Cost
}

// seedSpill populates t(id i32 PK, k i32, s text) with n rows whose k is deliberately unsorted
// and has many duplicates + a repeating NULL (to exercise the stable-sort tie-break and NULL
// ordering), and a variable-length s (so a spilled run carries variable-width values).
func seedSpill(t *testing.T, db dbHandle, n int64) {
	t.Helper()
	if _, err := db.Execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32, s text)", nil); err != nil {
		t.Fatal(err)
	}
	for id := int64(0); id < n; id++ {
		k := "NULL"
		if id%7 != 0 {
			k = fmt.Sprintf("%d", (id*48271)%100)
		}
		s := strings.Repeat("x", int(id%17))
		if _, err := db.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d, %s, '%s')", id, k, s), nil); err != nil {
			t.Fatal(err)
		}
	}
}

// spillShapes is the set of ORDER BY shapes spill must reproduce exactly. Each is a single-table
// query that takes the streaming external-sort path (spill.md §5).
var spillShapes = []string{
	"SELECT id, k FROM t ORDER BY k, id",
	"SELECT id, k FROM t ORDER BY k DESC, id DESC",
	"SELECT k, id FROM t ORDER BY k NULLS FIRST, id",
	"SELECT id FROM t ORDER BY k, id LIMIT 13",
	"SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
	"SELECT id, s FROM t WHERE k > 20 ORDER BY s, id",
	"SELECT id FROM t ORDER BY k, id OFFSET 195",
}

// valueEqual is a NULL-safe, value-canonical equality (NULL == NULL; decimals by value), so the
// spilling and in-memory results compare exactly even across NULLs (which 3VL Eq3 would not).
func valueEqual(a, b Value) bool {
	if a.Kind != b.Kind {
		return false
	}
	switch a.Kind {
	case ValNull:
		return true
	case ValInt, ValTimestamp, ValTimestamptz:
		return a.Int == b.Int
	case ValBool:
		return a.boolVal() == b.boolVal()
	case ValText, ValBytea, ValUuid:
		return a.str() == b.str()
	case ValDecimal:
		return a.decimal().CmpValue(*b.decimal()) == 0
	default:
		return false
	}
}

func rowsEqual(a, b [][]Value) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if len(a[i]) != len(b[i]) {
			return false
		}
		for j := range a[i] {
			if !valueEqual(a[i][j], b[i][j]) {
				return false
			}
		}
	}
	return true
}

func TestSpillingSortMatchesInMemory(t *testing.T) {
	path := filepath.Join(t.TempDir(), "spill_match.jed")

	// The source of truth: the same data + queries against a pure in-memory database, which never
	// spills (spill.md §2).
	mem := memDB().Session(SessionOptions{})
	seedSpill(t, mem, 200)

	// A file-backed database with a tiny workMem so every shape spills many runs and k-way-merges.
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	seedSpill(t, db, 200)
	db.SetWorkMem(128) // ~2-3 rows per run → dozens of runs, deep merge

	for _, sql := range spillShapes {
		wantRows, wantCost := runQuery(t, mem, sql)
		gotRows, gotCost := runQuery(t, db, sql)
		if !rowsEqual(gotRows, wantRows) {
			t.Fatalf("rows diverged under spill for %q:\n got %v\nwant %v", sql, gotRows, wantRows)
		}
		if gotCost != wantCost {
			t.Fatalf("cost diverged under spill for %q: got %d, want %d", sql, gotCost, wantCost)
		}
	}

	// The same file-backed database with spill DISABLED (workMem 0 = unlimited) must also match.
	db.SetWorkMem(0)
	for _, sql := range spillShapes {
		wantRows, wantCost := runQuery(t, mem, sql)
		gotRows, gotCost := runQuery(t, db, sql)
		if !rowsEqual(gotRows, wantRows) {
			t.Fatalf("rows diverged with spill off for %q", sql)
		}
		if gotCost != wantCost {
			t.Fatalf("cost diverged with spill off for %q: got %d, want %d", sql, gotCost, wantCost)
		}
	}
}

func TestSpillLeavesNoTempFiles(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "spill_cleanup.jed")

	countSpillFiles := func() int {
		entries, err := os.ReadDir(dir)
		if err != nil {
			t.Fatal(err)
		}
		n := 0
		for _, e := range entries {
			if strings.HasPrefix(e.Name(), "jed-spill-") {
				n++
			}
		}
		return n
	}

	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	seedSpill(t, db, 150)
	db.SetWorkMem(64) // force heavy spilling

	// A full-consume sort and an early-stopped (LIMIT) sort both clean up their runs.
	runQuery(t, db, "SELECT id FROM t ORDER BY k, id")
	runQuery(t, db, "SELECT id FROM t ORDER BY k, id LIMIT 3")
	if n := countSpillFiles(); n != 0 {
		t.Fatalf("spill run files leaked: %d remain", n)
	}
}

func TestSpillingSortIsStableOnTies(t *testing.T) {
	// Every row shares the same key, so the whole result is one big tie: a stable sort keeps the
	// scan order (primary key = id ascending). The external sort reproduces it only if the merge
	// tie-breaks by (run, position) = input order (spill.md §6).
	path := filepath.Join(t.TempDir(), "spill_stable.jed")
	db, err := create(path, DatabaseOptions{PageSize: DefaultPageSize})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32)", nil); err != nil {
		t.Fatal(err)
	}
	for id := int64(0); id < 100; id++ {
		if _, err := db.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d, 5)", id), nil); err != nil {
			t.Fatal(err)
		}
	}
	db.SetWorkMem(96) // force spilling so the merge tie-break is exercised

	rows, _ := runQuery(t, db, "SELECT id FROM t ORDER BY k")
	for i := int64(0); i < 100; i++ {
		if rows[i][0].Kind != ValInt || rows[i][0].Int != i {
			t.Fatalf("row %d: expected id %d, got %v", i, i, rows[i][0])
		}
	}
}
