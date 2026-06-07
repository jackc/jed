package jed

import (
	"fmt"
	"path/filepath"
	"testing"
)

// TestDemandPagingScansCorrectlyWithBoundedResidency exercises P6.4b end to end (spec/design/pager.md
// §1/§4): a file-backed database with many leaf pages, reopened with a tiny buffer-pool budget, still
// scans and mutates correctly while keeping only a bounded number of leaves resident — the residency
// win.
func TestDemandPagingScansCorrectlyWithBoundedResidency(t *testing.T) {
	path := filepath.Join(t.TempDir(), "paging.jed")
	const n = 600
	const cap = 3

	// Build a multi-level tree at a small page size, so a few hundred rows span many pages.
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "BEGIN"); err != nil { // one commit, not 600
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := Execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k*2)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := Execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen demand-paged with a 3-leaf budget.
	db, err = openWithCapacity(path, cap)
	if err != nil {
		t.Fatal(err)
	}
	// A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached), so the
	// pool starts empty — and the file holds many pages.
	if db.residentLeaves() != 0 {
		t.Fatalf("skeleton load should cache no leaf; resident = %d", db.residentLeaves())
	}
	if int(db.pageCount) <= cap*5 {
		t.Fatalf("file should have many more pages (%d) than the pool budget", db.pageCount)
	}

	// A full scan faults every leaf through the bounded pool: results are exact, residency bounded.
	rows := db.RowsInKeyOrder("t")
	if len(rows) != n {
		t.Fatalf("row count %d != %d", len(rows), n)
	}
	for i, row := range rows {
		if row[0].Int != int64(i) || row[1].Int != int64(i)*2 {
			t.Fatalf("row %d = (%d, %d)", i, row[0].Int, row[1].Int)
		}
	}
	if db.residentLeaves() > cap {
		t.Fatalf("resident leaves %d exceed the pool budget %d", db.residentLeaves(), cap)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
	db, err = openWithCapacity(path, cap)
	if err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{
		"DELETE FROM t WHERE k = 100",
		"UPDATE t SET v = 999 WHERE k = 200",
		"INSERT INTO t VALUES (600, 1200)",
	} {
		if _, err := Execute(db, sql); err != nil {
			t.Fatal(err)
		}
	}
	if db.residentLeaves() > cap {
		t.Fatalf("mutations should keep residency bounded; resident = %d", db.residentLeaves())
	}
	if err := db.Close(); err != nil { // autocommit already persisted each statement
		t.Fatal(err)
	}

	db, err = openWithCapacity(path, cap)
	if err != nil {
		t.Fatal(err)
	}
	rows = db.RowsInKeyOrder("t")
	if len(rows) != n {
		t.Fatalf("after one delete + one insert, count %d != %d", len(rows), n)
	}
	for _, row := range rows {
		switch row[0].Int {
		case 100:
			t.Fatal("k=100 should have been deleted")
		case 200:
			if row[1].Int != 999 {
				t.Fatalf("k=200 v = %d, want 999", row[1].Int)
			}
		case 600:
			if row[1].Int != 1200 {
				t.Fatalf("k=600 v = %d, want 1200", row[1].Int)
			}
		}
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}
