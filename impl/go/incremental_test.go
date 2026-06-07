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
	// far fewer pages than the whole tree, and bounded by tree height, not table size.
	before := fileSize(t, path)
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (31, 'row-31-%s')", pad))
	appended := (fileSize(t, path) - before) / ps
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
