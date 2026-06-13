package jed

// P6.1 part B — incremental copy-on-write commit (spec/fileformat/format.md, *Allocation &
// incremental commit*). A commit appends only the dirty pages a mutation introduced and publishes the
// new root by alternating the meta slot, leaving the prior snapshot's pages intact. These per-core
// tests cover what a static golden cannot (the bytes depend on commit history): that a commit grows
// the file incrementally rather than rewriting it, that the meta slots alternate, and that a torn
// write of the latest commit falls back to the prior durable snapshot.

import (
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// slotTxid returns the txid of meta slot `slot` in a raw file image (page_size is the u32 at offset
// 8; the meta header's txid is at offset 12 within the slot's page — spec/fileformat/format.md).
func slotTxid(b []byte, slot int) uint64 {
	ps := int(binary.BigEndian.Uint32(b[8:12]))
	return binary.BigEndian.Uint64(b[slot*ps+12:])
}

func selectIDs(t *testing.T, db *Database) []int64 {
	t.Helper()
	rows := queryRows(t, db, "SELECT id FROM t")
	out := make([]int64, len(rows))
	for i, r := range rows {
		out[i] = r[0].Int
	}
	return out
}

func TestSingleRowCommitAppendsOnlyTheDirtyPath(t *testing.T) {
	path := filepath.Join(t.TempDir(), "incremental_small_growth.jed")
	const ps = int64(256)
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)")
	// Enough rows for a multi-level tree at 256-byte pages (≈3 records/leaf). Each insert
	// autocommits, so the file already holds many leaked pages by the end of the loop.
	pad := ""
	for i := 0; i < 48; i++ {
		pad += "x"
	}
	for i := 1; i <= 30; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, 'row-%02d-%s')", i, i, pad))
	}

	// The whole tree spans many pages; a from-scratch image (no leaks) measures it.
	whole, err := db.ToImage(db.PageSize(), db.Txid())
	if err != nil {
		t.Fatal(err)
	}
	wholePages := int64(len(whole)) / ps
	if wholePages < 10 {
		t.Fatalf("the tree should span several pages, got %d", wholePages)
	}

	// One more row: the incremental commit appends only the rebuilt root→leaf path + catalog —
	// far fewer pages than the whole tree, and bounded by tree height, not table size. We track the
	// committed pageCount delta, not the file length — the file is preallocated in chunks ahead of
	// the high-water (spec/design/pager.md §7), so its physical size jumps by a chunk, not the
	// dirty-page count.
	before := int64(db.PageCount())
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (31, 'row-31-%s')", pad))
	appended := int64(db.PageCount()) - before
	if appended < 2 {
		t.Fatalf("the commit must append its dirty path + catalog, got %d", appended)
	}
	if appended >= wholePages {
		t.Fatalf("incremental commit (%d pages) must not rewrite the whole %d-page tree", appended, wholePages)
	}
	if appended > 8 {
		t.Fatalf("the dirty path is bounded by tree height, not table size, got %d", appended)
	}

	// And it reopens to the full, correct contents (leaked pages and all).
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	got := selectIDs(t, db)
	for i := int64(0); i < 31; i++ {
		if got[i] != i+1 {
			t.Fatalf("row %d = %d, want %d", i, got[i], i+1)
		}
	}
	if len(got) != 31 {
		t.Fatalf("got %d rows, want 31", len(got))
	}
}

func TestDeleteHeavyHistoryReopensCorrectly(t *testing.T) {
	// Deletes commit through the same incremental path but rebalance the tree (merge-then-split),
	// dirtying a different node set than inserts. Across many autocommitted inserts and deletes — each
	// leaking pages — the live snapshot must still reopen exactly (spec/fileformat/format.md).
	path := filepath.Join(t.TempDir(), "incremental_deletes.jed")
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)")
	pad := ""
	for i := 0; i < 48; i++ {
		pad += "x"
	}
	for i := 1; i <= 30; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, 'row-%02d-%s')", i, i, pad))
	}
	for i := 1; i <= 20; i++ {
		mustExec(t, db, fmt.Sprintf("DELETE FROM t WHERE id = %d", i))
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	got := selectIDs(t, db)
	if len(got) != 10 {
		t.Fatalf("got %d rows, want 10", len(got))
	}
	for i := int64(0); i < 10; i++ {
		if got[i] != i+21 {
			t.Fatalf("row %d = %d, want %d", i, got[i], i+21)
		}
	}
}

func TestMetaSlotsAlternateAcrossCommits(t *testing.T) {
	path := filepath.Join(t.TempDir(), "incremental_alternation.jed")
	db, err := Create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}

	// Create seeds BOTH slots at txid 1, so two valid metas exist from the first moment.
	img := readAll(t, path)
	if slotTxid(img, 0) != 1 || slotTxid(img, 1) != 1 {
		t.Fatalf("create should seed both slots at txid 1, got %d/%d", slotTxid(img, 0), slotTxid(img, 1))
	}

	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)") // txid 2 → slot 0
	mustExec(t, db, "INSERT INTO t VALUES (1)")              // txid 3 → slot 1
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Each commit writes only the alternate slot, leaving the prior published meta intact.
	img = readAll(t, path)
	if slotTxid(img, 0) != 2 {
		t.Fatalf("even txid lands in slot 0, got %d", slotTxid(img, 0))
	}
	if slotTxid(img, 1) != 3 {
		t.Fatalf("odd txid lands in slot 1, got %d", slotTxid(img, 1))
	}

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != 3 {
		t.Fatalf("open adopts the highest valid txid, got %d", db.Txid())
	}
}

func TestTornLatestCommitFallsBackToPriorSnapshot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "incremental_torn_meta.jed")
	db, err := Create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)") // txid 2 (slot 0)
	mustExec(t, db, "INSERT INTO t VALUES (1)")              // txid 3 (slot 1)
	mustExec(t, db, "INSERT INTO t VALUES (2)")              // txid 4 (slot 0) — the newest commit
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// Simulate a torn write of the newest commit: corrupt slot 0's checksum (txid 4). The loader must
	// fall back to slot 1 (txid 3) — whose body pages copy-on-write never overwrote — so row 2's
	// commit vanishes but the prior snapshot (row 1 only) is intact and uncorrupted.
	img := readAll(t, path)
	if slotTxid(img, 0) != 4 {
		t.Fatalf("slot 0 should hold the newest commit, got %d", slotTxid(img, 0))
	}
	img[32] ^= 0xFF // flip a CRC byte of slot 0's meta header
	if err := os.WriteFile(path, img, 0o644); err != nil {
		t.Fatal(err)
	}

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != 3 {
		t.Fatalf("should fall back to the prior committed snapshot, got txid %d", db.Txid())
	}
	got := selectIDs(t, db)
	if len(got) != 1 || got[0] != 1 {
		t.Fatalf("only the prior snapshot's row should survive the torn write, got %v", got)
	}
}

// TestCommitPreallocatesFileGrowthInChunks mirrors the Rust/TS preallocation tests (spec/design/pager.md
// §7, TODO.md durable-commit win): a commit that grows past the allocation high-water extends the file
// by whole 1 MiB chunks of real zero blocks, so the physical file is a multiple of the chunk and runs
// ahead of the committed pageCount. The slack is unreferenced (the committed image round-trips
// exactly), and a later commit that fits within it does not grow the file at all (the steady-state
// metadata-free path). The logical pageCount is the real high-water — independent of the physical size.
func TestCommitPreallocatesFileGrowthInChunks(t *testing.T) {
	const chunk = int64(1024 * 1024) // preallocChunkBytes (pager.go)
	path := filepath.Join(t.TempDir(), "prealloc_chunks.jed")

	// A from-scratch image is just the empty catalog — far below one chunk — so the file starts
	// un-aligned (Create writes exactly pageCount pages, no preallocation).
	db, err := Create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)")

	// One commit big enough to push the tree past a chunk: ~400 rows of a ~3.5 KiB pad ≈ 1.4 MiB of
	// tree, > the 128-page (1 MiB) chunk at the default 8 KiB page size.
	pad := strings.Repeat("p", 3500)
	mustExec(t, db, "BEGIN")
	for i := 0; i < 400; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, '%s')", i, pad))
	}
	mustExec(t, db, "COMMIT")

	logical := int64(db.PageCount()) * int64(db.PageSize())
	physical := fileSize(t, path)
	if db.PageCount() <= 128 {
		t.Fatalf("the batch should span more than one chunk's worth of pages, got %d", db.PageCount())
	}
	if physical%chunk != 0 {
		t.Fatalf("physical file should grow in whole chunks, got %d (chunk %d)", physical, chunk)
	}
	if physical < logical || physical < chunk {
		t.Fatalf("preallocation should run ahead of the %d-byte committed image, got physical %d", logical, physical)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// The committed image round-trips exactly through the preallocated file (trailing slack is inert
	// zeros past the high-water).
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	physicalBefore := fileSize(t, path)
	if got := len(selectIDs(t, db)); got != 400 {
		t.Fatalf("expected 400 rows after reopen, got %d", got)
	}

	// A small commit fits within the preallocated slack, so the physical file does not grow at all —
	// the steady-state metadata-free commit path.
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1000, '%s')", pad))
	if got := fileSize(t, path); got != physicalBefore {
		t.Fatalf("a commit within the preallocated slack should reuse it without growing the file: %d vs %d", got, physicalBefore)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// And the extra row is durable.
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := len(selectIDs(t, db)); got != 401 {
		t.Fatalf("expected 401 rows after the in-slack commit, got %d", got)
	}
}

func fileSize(t *testing.T, path string) int64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Size()
}

func readAll(t *testing.T, path string) []byte {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return b
}
