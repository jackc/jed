package jed

import (
	"encoding/binary"
	"fmt"
	"path/filepath"
	"strings"
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
	db, err = OpenWithOptions(path, OpenOptions{CacheBytes: cap * 256})
	if err != nil {
		t.Fatal(err)
	}
	// A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached), so the
	// pool starts empty — and the file holds many pages.
	if db.ResidentLeaves() != 0 {
		t.Fatalf("skeleton load should cache no leaf; resident = %d", db.ResidentLeaves())
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
	if db.ResidentLeaves() > cap {
		t.Fatalf("resident leaves %d exceed the pool budget %d", db.ResidentLeaves(), cap)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
	db, err = OpenWithOptions(path, OpenOptions{CacheBytes: cap * 256})
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
	if db.ResidentLeaves() > cap {
		t.Fatalf("mutations should keep residency bounded; resident = %d", db.ResidentLeaves())
	}
	if err := db.Close(); err != nil { // autocommit already persisted each statement
		t.Fatal(err)
	}

	db, err = OpenWithOptions(path, OpenOptions{CacheBytes: cap * 256})
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

// TestMemoryBudgetBoundsResidencyUnderLookups exercises P6.4c (memory-budget API + large-file
// hardening, spec/design/pager.md §6): a database whose leaf pages far exceed a tiny CacheBytes budget
// opens via the public API, and a repeated point-query workload keeps ResidentLeaves() within the
// budget throughout (each scan faults leaves through the pool, which evicts under CLOCK).
func TestMemoryBudgetBoundsResidencyUnderLookups(t *testing.T) {
	path := filepath.Join(t.TempDir(), "budget.jed")
	const n = 2000
	const cap = 4

	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := Execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k+1)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := Execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = OpenWithOptions(path, OpenOptions{CacheBytes: cap * 256})
	if err != nil {
		t.Fatal(err)
	}
	// The data dwarfs the budget: far more pages than cap, yet nothing resident until a read.
	if int(db.pageCount) <= cap*20 {
		t.Fatalf("file (%d pages) should dwarf the %d-page budget", db.pageCount, cap)
	}
	if db.ResidentLeaves() != 0 {
		t.Fatalf("skeleton load should cache no leaf; resident = %d", db.ResidentLeaves())
	}

	// A spread of point queries (each a full scan, no index) repeatedly faults leaves through the
	// bounded pool; residency never exceeds the budget, and every answer is correct.
	for k := 0; k < n; k += 97 {
		out, err := Execute(db, fmt.Sprintf("SELECT v FROM t WHERE k = %d", k))
		if err != nil {
			t.Fatal(err)
		}
		if len(out.Rows) != 1 || out.Rows[0][0].Int != int64(k+1) {
			t.Fatalf("query at k=%d: %v", k, out.Rows)
		}
		if db.ResidentLeaves() > cap {
			t.Fatalf("resident leaves %d exceed the budget %d at k=%d", db.ResidentLeaves(), cap, k)
		}
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

// TestTinyBudgetKeepsOneLeafResident exercises P6.4c (spec/design/pager.md §3, api.md §2.1): a byte
// budget smaller than a single page still keeps one leaf resident — the max(1, CacheBytes/pageSize)
// floor — and still scans correctly. This is the pageSize > CacheBytes case.
func TestTinyBudgetKeepsOneLeafResident(t *testing.T) {
	path := filepath.Join(t.TempDir(), "tiny.jed")
	const n = 400

	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := Execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k+1)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := Execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// A 1-byte budget is far below the 256-byte page size: it must clamp to one resident leaf, not zero.
	db, err = OpenWithOptions(path, OpenOptions{CacheBytes: 1})
	if err != nil {
		t.Fatal(err)
	}
	rows := db.RowsInKeyOrder("t")
	if len(rows) != n {
		t.Fatalf("row count %d != %d", len(rows), n)
	}
	for i, row := range rows {
		if row[0].Int != int64(i) || row[1].Int != int64(i)+1 {
			t.Fatalf("row %d = (%d, %d)", i, row[0].Int, row[1].Int)
		}
	}
	if got := db.ResidentLeaves(); got != 1 {
		t.Fatalf("a sub-page budget should keep exactly one leaf resident; got %d", got)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

// TestCreateRejectsOversizedPageSize exercises P6.4c page-size hardening (format.md *Page model*):
// Create rejects a page size above maxPageSize (64 KiB) — without the cap a huge page size forces a
// multi-gigabyte allocation.
func TestCreateRejectsOversizedPageSize(t *testing.T) {
	path := filepath.Join(t.TempDir(), "huge.jed")
	_, err := Create(path, DatabaseOptions{PageSize: 1 << 20})
	ee, ok := err.(*EngineError)
	if !ok || ee.Code() != "0A000" {
		t.Fatalf("want 0A000 feature_not_supported, got %v", err)
	}
	if got := ee.Message; !strings.Contains(got, "too large") {
		t.Fatalf("message should name the cause, got %q", got)
	}
}

// TestReadRejectsOversizedPageSize exercises P6.4c page-size hardening (format.md *Page model*): the
// read path rejects a file whose meta records an out-of-range page_size as corrupt — the range check
// runs before any allocation against that size, so a hostile file cannot force a giant allocation
// (CLAUDE.md §13).
func TestReadRejectsOversizedPageSize(t *testing.T) {
	// A crafted meta header recording page_size = 70000 (> maxPageSize) in big-endian at offset 8.
	image := make([]byte, 200)
	copy(image[0:4], "JEDB")
	binary.BigEndian.PutUint32(image[8:12], 70000)
	_, err := LoadDatabase(image)
	ee, ok := err.(*EngineError)
	if !ok || ee.Code() != "XX001" {
		t.Fatalf("want XX001 data_corrupted, got %v", err)
	}
}
