package jed

// Slice A — out-of-line large values / overflow pages (spec/design/large-values.md §12). A value
// that would push a record past RECORD_MAX spills to a chain of overflow pages (page_type 4),
// leaving a fixed pointer in the record. These per-core tests cover what a static golden cannot
// (an incremental file's bytes depend on commit history): a multi-page chain round-trips, small
// values never spill, the free-list keeps live chains and reclaims dead ones, and the default
// demand-paged file path reads a spilled value back exactly.

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// countPageType counts body pages of a given page_type in an image (meta slots start with the magic,
// so they never collide with a small page_type byte).
func countPageType(image []byte, ps int, ty byte) int {
	c := 0
	for i := 0; i+ps <= len(image); i += ps {
		if image[i] == ty {
			c++
		}
	}
	return c
}

// bigValueDB builds an in-memory table with a ~1250-byte text value (forces a multi-page overflow
// chain at page 256: RECORD_MAX = (256-12-12)/2 = 116, cap = 244) plus a small inline value.
func bigValueDB(t *testing.T) (*Database, string) {
	t.Helper()
	db := NewDatabase()
	big := strings.Repeat("abcΩ", 250) // 5 bytes × 250 = 1250 UTF-8 bytes
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s')", big))
	mustExec(t, db, "INSERT INTO t VALUES (2, 'tiny')")
	return db, big
}

func TestExternalValueSpansOverflowChainAndRoundTrips(t *testing.T) {
	db, big := bigValueDB(t)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	if n := countPageType(image, 256, pageOverflow); n < 2 {
		t.Fatalf("a large value should span several overflow pages, got %d", n)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatal(err)
	}
	// Re-serialization is byte-identical (deterministic spill + chain allocation).
	again, err := loaded.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	if string(again) != string(image) {
		t.Error("re-serialization of an external value is not byte-identical")
	}
	rows := loaded.RowsInKeyOrder("t")
	if len(rows) != 2 || rows[0][1].Str != big || rows[1][1].Str != "tiny" {
		t.Fatalf("external value did not survive the round trip: %+v", rows)
	}
}

func TestSmallValuesNeverSpill(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	if n := countPageType(image, 256, pageOverflow); n != 0 {
		t.Fatalf("inline-fitting values must never spill, got %d overflow pages", n)
	}
}

func TestLoadReclaimsOnlyDeadOverflowPages(t *testing.T) {
	db, _ := bigValueDB(t)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatal(err)
	}
	ovf := countPageType(image, 256, pageOverflow)
	if ovf < 2 {
		t.Fatalf("expected a multi-page chain, got %d", ovf)
	}
	// The live value's chain pages are reachable, so they are NOT on the free-list (else a later
	// commit would reuse a still-referenced page).
	if len(loaded.freePages) >= ovf {
		t.Fatalf("live overflow pages (%d) must be reachable, not free (%d free)", ovf, len(loaded.freePages))
	}
}

func TestExternalValueThroughPagedFileAndReclaims(t *testing.T) {
	path := filepath.Join(t.TempDir(), "large_values.jed")
	big := strings.Repeat("Z", 1500) // ≫ RECORD_MAX at ps 256 ⇒ a multi-page overflow chain

	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s')", big))
	mustExec(t, db, "INSERT INTO t VALUES (2, 'small')")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen demand-paged (the default Open): the big value reconstructs exactly through the
	// pager-backed chain read.
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id, body FROM t")
	if len(rows) != 2 || rows[0][1].Str != big || rows[1][1].Str != "small" {
		t.Fatalf("paged read of an external value is wrong: lens %d", len(rows))
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Delete the big row; its chain is orphaned (leaked this session).
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "DELETE FROM t WHERE id = 1")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen: the free-list reconstruction collects only live chains, so the dead chain's pages are
	// now free. Re-inserting a large value reuses them — the high-water grows by a handful of pages,
	// not by a whole fresh chain (~7 pages).
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	before := db.pageCount
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (3, '%s')", big))
	after := db.pageCount
	if after > before+3 {
		t.Fatalf("re-insert did not reuse reclaimed overflow pages (pageCount %d → %d)", before, after)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Final correctness through the paged path.
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	got := queryRows(t, db, "SELECT body FROM t WHERE id = 3")
	if len(got) != 1 || got[0][0].Str != big {
		t.Fatalf("re-inserted big row is wrong")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}
