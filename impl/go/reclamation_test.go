package jed

// P6.2 — free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit
// allocator reuses pages a prior root abandoned instead of always extending the file: on open the
// free-list is reconstructed as [2, pageCount) minus the committed root's reachable pages, and a commit
// draws dirty/catalog pages from it (lowest-first) before extending. These per-core tests cover what a
// static golden cannot (the bytes depend on commit history): that reopening reclaims the dead pages a
// churn left so a later churn reuses them (the file stops growing), that reuse round-trips, and that a
// torn latest commit *after reuse* still falls back to the intact prior snapshot (a reused page was
// dead, so overwriting it never damaged the fallback).

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const reclaimPS = int64(256)

// reclaimPageCount is the committed logical page high-water (db.PageCount) — the count the meta
// records, which directly reports whether a commit extended the high-water or reused a free page. We
// track this, not the file length: the file is preallocated in chunks ahead of the high-water
// (spec/design/pager.md §7), so its physical size no longer equals pageCount*pageSize.
func reclaimPageCount(db *Database) int64 {
	return int64(db.PageCount())
}

// padOf returns the pad text of the row with id, and whether it exists.
func padOf(t *testing.T, db *Database, id int64) (string, bool) {
	rows := queryRows(t, db, fmt.Sprintf("SELECT pad FROM t WHERE id = %d", id))
	if len(rows) == 0 {
		return "", false
	}
	return rows[0][0].Str, true
}

func reclaimSetup(t *testing.T, path string, rows int) *Database {
	db, err := Create(path, DatabaseOptions{PageSize: uint32(reclaimPS)})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	base := strings.Repeat("x", 40)
	for i := 1; i <= rows; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, 'r%02d-%s')", i, i, base))
	}
	return db
}

func TestReopenReclaimsDeadPagesSoALaterChurnReuses(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reclaim_reuse.jed")
	db := reclaimSetup(t, path, 30) // a multi-level tree at page 256
	pad := strings.Repeat("y", 40)

	// Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the catalog
	// to fresh pages and leaks the old ones (P6.2 does not reclaim mid-session), so the logical
	// high-water grows. (We track the committed pageCount, not the file length — the file is
	// preallocated in chunks ahead of it, spec/design/pager.md §7.)
	for k := 0; k < 60; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'a%d-%s' WHERE id = 15", k, pad))
	}
	pcAfterChurn1 := reclaimPageCount(db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen: the free-list is reconstructed from the ~60 churn iterations' dead pages.
	db, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if pc := reclaimPageCount(db); pc != pcAfterChurn1 {
		t.Fatalf("reopen changed the high-water: %d vs %d", pc, pcAfterChurn1)
	}

	// The very first post-reopen commit reuses a free page rather than extending the high-water.
	mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'b0-%s' WHERE id = 15", pad))
	if got := reclaimPageCount(db); got != pcAfterChurn1 {
		t.Fatalf("first commit after reopen grew the high-water (no reuse): %d vs %d", got, pcAfterChurn1)
	}

	// A whole second churn — shorter than the first, so the reclaimed pool covers it — does not grow
	// the high-water: the page count after equals the count after the first churn.
	for k := 1; k < 40; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'b%d-%s' WHERE id = 15", k, pad))
	}
	if got := reclaimPageCount(db); got != pcAfterChurn1 {
		t.Fatalf("second churn grew the high-water despite reuse: %d vs %d", got, pcAfterChurn1)
	}

	// And the data is exactly right (reuse never clobbered a live page).
	want := fmt.Sprintf("b39-%s", pad)
	if got, ok := padOf(t, db, 15); !ok || got != want {
		t.Fatalf("row 15 pad = %q (ok=%v), want %q", got, ok, want)
	}
	checkIDs1to(t, selectIDs(t, db), 30)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := padOf(t, db, 15); !ok || got != want {
		t.Fatalf("after final reopen row 15 pad = %q (ok=%v), want %q", got, ok, want)
	}
	checkIDs1to(t, selectIDs(t, db), 30)
}

func TestHeavyInsertDeleteChurnReopensWithReuse(t *testing.T) {
	// Insert/delete churn dirties a different node set than updates (split/merge rebalance) and, across
	// a reopen, exercises reuse over both. The live snapshot must reopen exactly.
	path := filepath.Join(t.TempDir(), "reclaim_churn.jed")
	db := reclaimSetup(t, path, 25)
	pad := strings.Repeat("z", 40)
	for k := 0; k < 40; k++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1000, 'k%d-%s')", k, pad))
		mustExec(t, db, "DELETE FROM t WHERE id = 1000")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	for k := 0; k < 40; k++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (2000, 'm%d-%s')", k, pad))
		mustExec(t, db, "DELETE FROM t WHERE id = 2000")
	}
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (26, 'p-%s')", pad))
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (27, 'q-%s')", pad))
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	checkIDs1to(t, selectIDs(t, db), 27)
}

func TestTornCommitAfterReuseFallsBackToPriorSnapshot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reclaim_torn.jed")
	db := reclaimSetup(t, path, 20)
	pad := strings.Repeat("w", 40)
	for k := 0; k < 30; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'c%d-%s' WHERE id = 10", k, pad))
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen so the free-list holds the churn's dead pages, then do two commits that reuse them.
	db, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'A-%s' WHERE id = 10", pad)) // prior snapshot
	orig11, _ := padOf(t, db, 11)
	mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'B-%s' WHERE id = 11", pad)) // newest commit
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Corrupt the newest meta slot's checksum (a torn write of the commit that reused free pages).
	img := readAll(t, path)
	ps := int(reclaimPS)
	newest := 0
	if slotTxid(img, 1) > slotTxid(img, 0) {
		newest = 1
	}
	priorTxid := slotTxid(img, 1-newest)
	img[newest*ps+32] ^= 0xFF // flip a CRC byte of the newest slot's meta header
	if err := os.WriteFile(path, img, 0o644); err != nil {
		t.Fatal(err)
	}

	// The loader falls back to the prior snapshot — intact even though the torn commit reused
	// (overwrote) free pages, because those pages were dead and the prior snapshot never referenced
	// them. Row 11's update vanishes; row 10's prior-commit value and every row survive.
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != priorTxid {
		t.Fatalf("should fall back to the prior committed snapshot, got txid %d want %d", db.Txid(), priorTxid)
	}
	if got, _ := padOf(t, db, 11); got != orig11 {
		t.Fatalf("the torn commit's row-11 update should have vanished: got %q want %q", got, orig11)
	}
	if got, _ := padOf(t, db, 10); got != fmt.Sprintf("A-%s", pad) {
		t.Fatalf("row 10 should hold its prior-commit value, got %q", got)
	}
	checkIDs1to(t, selectIDs(t, db), 20)
}

// checkIDs1to asserts the selected ids are exactly 1..=n in order.
func checkIDs1to(t *testing.T, got []int64, n int) {
	t.Helper()
	if len(got) != n {
		t.Fatalf("got %d rows, want %d", len(got), n)
	}
	for i := 0; i < n; i++ {
		if got[i] != int64(i+1) {
			t.Fatalf("row %d = %d, want %d", i, got[i], i+1)
		}
	}
}
