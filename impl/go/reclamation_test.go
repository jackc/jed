package jed

// Free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit allocator reuses
// pages a prior root abandoned instead of always extending the file. Since v25 the free-list is
// persisted (meta offset 28 → a page_type 7 chain) and reclamation is continuous within-session: a file
// commit reclaims this commit's fresh orphans in-commit (periodically — once the high-water passes ~2×
// the live count), so the high-water oscillates in [live, 2×live] across a long churn rather than growing
// monotonically, and open reads the persisted free-list directly (no reconstruction walk). These per-core
// tests cover what a static golden cannot (the bytes depend on commit history): that within-session churn
// stays bounded, that reopening reads the persisted free-list and a later churn stays bounded, that reuse
// round-trips, and that a torn latest commit *after reuse* still falls back to the intact prior snapshot.

import (
	"encoding/binary"
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
func reclaimPageCount(db *engine) int64 {
	return int64(db.PageCount())
}

// padOf returns the pad text of the row with id, and whether it exists.
func padOf(t *testing.T, db *engine, id int64) (string, bool) {
	rows := queryRows(t, db, fmt.Sprintf("SELECT pad FROM t WHERE id = %d", id))
	if len(rows) == 0 {
		return "", false
	}
	return rows[0][0].str(), true
}

func reclaimSetup(t *testing.T, path string, rows int) *engine {
	db, err := create(path, databaseOptions{PageSize: uint32(reclaimPS), noSync: true})
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

func TestWithinSessionChurnStaysBoundedAndReopensFromPersistedFreeList(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "reclaim_reuse.jed")
	db := reclaimSetup(t, path, 30) // a multi-level tree at page 256
	pad := strings.Repeat("y", 40)

	// Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the catalog to
	// fresh pages, and v25 reclaims the pages the prior root abandoned in-commit (periodically), so the
	// high-water oscillates in [live, 2×live] rather than growing monotonically with the 60 updates.
	// (We track the committed pageCount, not the file length — preallocated in chunks, pager.md §7.)
	for k := 0; k < 60; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'a%d-%s' WHERE id = 15", k, pad))
	}
	pcAfterChurn1 := reclaimPageCount(db)
	if pcAfterChurn1 >= 60 {
		t.Fatalf("within-session reclamation should bound the high-water, got %d (60 updates × ~2 pages without it)", pcAfterChurn1)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Reopen: the free-list is read directly from the persisted chain (no reconstruction walk); the
	// high-water is whatever the last commit recorded.
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if pc := reclaimPageCount(db); pc != pcAfterChurn1 {
		t.Fatalf("reopen changed the high-water: %d vs %d", pc, pcAfterChurn1)
	}

	// The first post-reopen commit reuses free pages from the persisted list rather than extending.
	mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'b0-%s' WHERE id = 15", pad))
	if got := reclaimPageCount(db); got > pcAfterChurn1+4 {
		t.Fatalf("first commit after reopen did not reuse the persisted free-list: %d vs %d", got, pcAfterChurn1)
	}

	// A whole second churn stays bounded too — reusing reclaimed pages, the high-water does not grow
	// with the churn count.
	for k := 1; k < 40; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'b%d-%s' WHERE id = 15", k, pad))
	}
	if got := reclaimPageCount(db); got > 2*pcAfterChurn1 {
		t.Fatalf("second churn grew the high-water beyond the [live, 2×live] band: %d vs ~2×%d", got, pcAfterChurn1)
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
	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := padOf(t, db, 15); !ok || got != want {
		t.Fatalf("after final reopen row 15 pad = %q (ok=%v), want %q", got, ok, want)
	}
	checkIDs1to(t, selectIDs(t, db), 30)
}

// freeListHead returns the live meta slot's free_list_head (v25, meta offset 28) in a raw image.
func freeListHead(b []byte) uint32 {
	ps := int(binary.BigEndian.Uint32(b[8:12]))
	live := 0
	if slotTxid(b, 1) > slotTxid(b, 0) {
		live = 1
	}
	return binary.BigEndian.Uint32(b[live*ps+28:])
}

// countFreelistPages counts the page_type 7 free-list pages over all pageCount pages of a raw image.
func countFreelistPages(b []byte) int {
	ps := int(binary.BigEndian.Uint32(b[8:12]))
	live := 0
	if slotTxid(b, 1) > slotTxid(b, 0) {
		live = 1
	}
	pageCount := int(binary.BigEndian.Uint32(b[live*ps+24:]))
	n := 0
	for i := 0; i < pageCount; i++ {
		if b[i*ps] == 7 {
			n++
		}
	}
	return n
}

// TestPersistedFreeListHeadsAPageType7Chain: after enough churn to build a free-list, the meta records a
// non-zero free_list_head (offset 28) that heads a page_type 7 chain, and reopening reads it back so the
// file stays bounded — the persisted-free-list byte contract a static golden cannot pin (it depends on
// commit history; format.md *Free-list page*).
func TestPersistedFreeListHeadsAPageType7Chain(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "reclaim_persisted.jed")
	db := reclaimSetup(t, path, 40)
	big := strings.Repeat("z", 40)
	for round := 0; round < 40; round++ {
		for id := 1; id <= 40; id++ {
			mustExec(t, db, fmt.Sprintf("UPDATE t SET pad = 'r%d-%d-%s' WHERE id = %d", round, id, big, id))
		}
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	bytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	head := freeListHead(bytes)
	if head < 2 {
		t.Fatalf("the meta should record a persisted free-list head (offset 28), got %d", head)
	}
	ps := int(binary.BigEndian.Uint32(bytes[8:12]))
	if bytes[int(head)*ps] != 7 {
		t.Fatalf("the free-list head page is not page_type 7, got %d", bytes[int(head)*ps])
	}
	if countFreelistPages(bytes) < 1 {
		t.Fatal("the file should carry at least one persisted free-list page")
	}

	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	checkIDs1to(t, selectIDs(t, db), 40)
	if pc := reclaimPageCount(db); pc >= 200 {
		t.Fatalf("reopened file should be bounded by within-session reclamation, got %d", pc)
	}
}

func TestHeavyInsertDeleteChurnReopensWithReuse(t *testing.T) {
	t.Parallel()
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

	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
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

	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	checkIDs1to(t, selectIDs(t, db), 27)
}

func TestTornCommitAfterReuseFallsBackToPriorSnapshot(t *testing.T) {
	t.Parallel()
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
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
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
	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
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

// countingStore wraps a fileBlockStore and counts readAt calls — the observable for the open-cost
// invariant below (the other methods are promoted from the embedded store).
type countingStore struct {
	*fileBlockStore
	reads *int
}

func (s *countingStore) readAt(off int64, length int) ([]byte, error) {
	*s.reads++
	return s.fileBlockStore.readAt(off, length)
}

// TestOpenReadsInteriorSpineNotEveryLeaf: open reads the interior spine, NOT every leaf
// (spec/design/storage.md §6). Since v25 dropped the free-list reachability walk and v28 persists the
// row count instead of summing leaf headers, open faults only catalog + interior pages + ~one
// leaf per bottom-level interior (to classify the level) + the meta/free-list pages — all O(interior
// spine). The block-read count must stay well below the leaf count, and above all must not scale with
// it. A counting blockStore is the only way to see this (it is not SQL-observable).
func TestOpenReadsInteriorSpineNotEveryLeaf(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "open_spine_only.jed")
	// A many-leaf table (page 256, ~4 rows/leaf) so "every leaf" is a large, distinctive number.
	db := reclaimSetup(t, path, 400)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	img, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	ps := int(binary.BigEndian.Uint32(img[8:12]))
	leaves := 0
	for i := 0; i < len(img)/ps; i++ {
		if img[i*ps] == 2 { // page_type 2 = leaf
			leaves++
		}
	}
	if leaves < 50 {
		t.Fatalf("the seed should span many leaves, got %d", leaves)
	}

	// Open through the counting store and tally the block reads open performs.
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	reads := 0
	cs := &countingStore{fileBlockStore: &fileBlockStore{f: f}, reads: &reads}
	p, err := pagerFromStore(cs)
	if err != nil {
		t.Fatal(err)
	}
	db2, err := loadEnginePaged(p, cacheLeaves(defaultCacheBytes, p.pageSize))
	if err != nil {
		t.Fatal(err)
	}
	// The ceiling is deliberately loose (< leaves) — the point is that open does NOT read every leaf,
	// and in practice reads ≪ leaves (only the spine + a peek per bottom-level interior).
	if reads >= leaves {
		t.Fatalf("open read %d pages for a %d-leaf table — it must read only the interior spine, not every leaf", reads, leaves)
	}
	if err := db2.Close(); err != nil {
		t.Fatal(err)
	}
}
