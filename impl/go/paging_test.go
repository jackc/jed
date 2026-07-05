package jed

import (
	"context"
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
	db, err := create(path, databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "BEGIN"); err != nil { // one commit, not 600
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k*2)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen demand-paged with a 3-leaf budget.
	db, err = openWithOptions(path, OpenOptions{CacheBytes: cap * 256, SkipFsync: true})
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
	db, err = openWithOptions(path, OpenOptions{CacheBytes: cap * 256, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{
		"DELETE FROM t WHERE k = 100",
		"UPDATE t SET v = 999 WHERE k = 200",
		"INSERT INTO t VALUES (600, 1200)",
	} {
		if _, err := execute(db, sql); err != nil {
			t.Fatal(err)
		}
	}
	if db.ResidentLeaves() > cap {
		t.Fatalf("mutations should keep residency bounded; resident = %d", db.ResidentLeaves())
	}
	if err := db.Close(); err != nil { // autocommit already persisted each statement
		t.Fatal(err)
	}

	db, err = openWithOptions(path, OpenOptions{CacheBytes: cap * 256, SkipFsync: true})
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

	db, err := create(path, databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k+1)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = openWithOptions(path, OpenOptions{CacheBytes: cap * 256, SkipFsync: true})
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
		out, err := execute(db, fmt.Sprintf("SELECT v FROM t WHERE k = %d", k))
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

	db, err := create(path, databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)"); err != nil {
		t.Fatal(err)
	}
	if _, err := execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	for k := 0; k < n; k++ {
		if _, err := execute(db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k+1)); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// A 1-byte budget is far below the 256-byte page size: it must clamp to one resident leaf, not zero.
	db, err = openWithOptions(path, OpenOptions{CacheBytes: 1, SkipFsync: true})
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
	_, err := create(path, databaseOptions{PageSize: 1 << 20, noSync: true})
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
	_, err := loadEngine(image)
	ee, ok := err.(*EngineError)
	if !ok || ee.Code() != "XX001" {
		t.Fatalf("want XX001 data_corrupted, got %v", err)
	}
}

// TestRejectsNonPowerOfTwoPageSize exercises page-size hardening (format.md *Page model*): a page
// size in range but not a power of two is rejected — 0A000 on Create, XX001 on the read path.
// Power-of-two keeps page boundaries sector-aligned (CLAUDE.md §9) and collapses the legal set.
func TestRejectsNonPowerOfTwoPageSize(t *testing.T) {
	// Create: 1000 is within [256, 65536] but not a power of two.
	path := filepath.Join(t.TempDir(), "pow2.jed")
	_, err := create(path, databaseOptions{PageSize: 1000, noSync: true})
	ee, ok := err.(*EngineError)
	if !ok || ee.Code() != "0A000" {
		t.Fatalf("want 0A000 feature_not_supported, got %v", err)
	}
	if got := ee.Message; !strings.Contains(got, "power of two") {
		t.Fatalf("message should name the cause, got %q", got)
	}

	// Read: a crafted meta recording page_size = 1000 reads as corrupt.
	image := make([]byte, 4096)
	copy(image[0:4], "JEDB")
	binary.BigEndian.PutUint32(image[8:12], 1000)
	_, err = loadEngine(image)
	ee, ok = err.(*EngineError)
	if !ok || ee.Code() != "XX001" {
		t.Fatalf("want XX001 data_corrupted, got %v", err)
	}
}

// TestRejectsPageSizeBelowFloor exercises the new 256 floor (format.md *Page model*): 128 — a power
// of two but below minPageSize — is rejected on Create.
func TestRejectsPageSizeBelowFloor(t *testing.T) {
	path := filepath.Join(t.TempDir(), "tiny.jed")
	_, err := create(path, databaseOptions{PageSize: 128, noSync: true})
	ee, ok := err.(*EngineError)
	if !ok || ee.Code() != "0A000" {
		t.Fatalf("want 0A000 feature_not_supported, got %v", err)
	}
	if got := ee.Message; !strings.Contains(got, "too small") {
		t.Fatalf("message should name the cause, got %q", got)
	}
}

// countLeafForms walks a committed table's B+tree and tallies leaf residency forms: Decoded
// (vals resident), Packed (block-backed), and OnDisk (demoted references).
func countLeafForms(st *tableStore) (decoded, packed, ondisk int) {
	var walk func(n *pnode)
	walk = func(n *pnode) {
		if n.isLeaf() {
			if n.packed != nil {
				packed++
			} else {
				decoded++
			}
			return
		}
		for _, c := range n.children {
			if c.node == nil {
				ondisk++
			} else {
				walk(c.node)
			}
		}
	}
	if root := st.treeRoot(); root != nil {
		walk(root)
	}
	return
}

// TestInSessionTableJoinsResidencyFlip pins the storePaging-at-creation contract: a table CREATEd
// in this session (never loaded from a file) binds the domain pager at creation, so the post-commit
// residency flip demotes its committed leaves — an in-memory database (which never reopens) must not
// keep every table fully-resident decoded for the handle's lifetime, and a file-backed database must
// take the same shape in its creating session as after a reopen.
func TestInSessionTableJoinsResidencyFlip(t *testing.T) {
	run := func(t *testing.T, db *Database) {
		if _, err := db.ExecuteScript("CREATE TABLE t (k i32 PRIMARY KEY, v i32)"); err != nil {
			t.Fatal(err)
		}
		if _, err := db.ExecuteScript("CREATE INDEX t_v ON t (v)"); err != nil {
			t.Fatal(err)
		}
		// 200 rows at page size 256 → a multi-leaf tree; autocommit runs the flip on every commit.
		for k := 0; k < 200; k++ {
			if _, err := db.ExecuteScript(fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", k, k*2)); err != nil {
				t.Fatal(err)
			}
		}
		snap := db.core.roots.Load().committed
		st := snap.stores["t"]
		if st.paging == nil {
			t.Fatal("an in-session-created table store should bind the domain pager at creation")
		}
		if ix := snap.indexStores["t_v"]; ix == nil || ix.paging == nil {
			t.Fatal("an in-session-created index store should bind the domain pager at creation")
		}
		decoded, packed, ondisk := countLeafForms(st)
		// The root leaf stays resident by the pMap convention; every other committed leaf must have
		// demoted. A multi-leaf tree therefore has OnDisk children and no Decoded leaf at all (the
		// root is interior); nothing should be resident-Packed right after a commit (packed forms
		// arise on fault, and the flip demoted the just-written Decoded forms).
		if ondisk == 0 {
			t.Fatalf("expected a multi-leaf demoted tree, got decoded=%d packed=%d ondisk=%d", decoded, packed, ondisk)
		}
		if decoded != 0 {
			t.Fatalf("committed leaves should demote after the flip, got decoded=%d packed=%d ondisk=%d", decoded, packed, ondisk)
		}
		// Reads fault the demoted leaves back through the pool and still see every row.
		rows, err := db.Query(context.Background(), "SELECT count(*), sum(v) FROM t")
		if err != nil {
			t.Fatal(err)
		}
		defer rows.Close()
		var n, sum int64
		if !rows.Next() {
			t.Fatal("no row")
		}
		if err := rows.Scan(&n, &sum); err != nil {
			t.Fatal(err)
		}
		if n != 200 || sum != 39800 {
			t.Fatalf("count=%d sum=%d", n, sum)
		}
	}
	t.Run("in-memory", func(t *testing.T) {
		db, err := CreateDatabase(CreateOptions{PageSize: 256})
		if err != nil {
			t.Fatal(err)
		}
		defer db.Close()
		run(t, db)
	})
	t.Run("file-create-session", func(t *testing.T) {
		db, err := CreateDatabase(CreateOptions{Path: filepath.Join(t.TempDir(), "flip.jed"), PageSize: 256, SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
		defer db.Close()
		run(t, db)
	})
}
