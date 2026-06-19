package jed

// Per-page checksum (format_version 7): every body page — catalog, B-tree leaf, B-tree interior,
// and overflow — carries a CRC-32/IEEE over its own bytes (spec/fileformat/format.md *Page header*;
// spec/design/storage.md §6). This pins the durability guarantee that distinguishes reliability item
// #3 from the meta-only checksum: a silently corrupted LIVE page is detected as XX001 the instant it
// is read — at open for a catalog/interior/overflow page (the loader and the free-list reachability
// walk), at fault for a leaf — and is NEVER served as wrong rows. A corrupted DEAD page (free space
// an earlier incremental commit abandoned, P6.2) is harmless: not reachable from the committed
// snapshot, so the file still reads back exactly. The invariant asserted is the strong one:
// corrupting any body page yields either XX001 or the byte-identical correct result — corruption is
// caught or inert, never silent. Mirrors impl/rust/tests/checksum.rs and impl/ts/tests/checksum.test.ts.
// Uses fillerText from fileformat_golden_test.go.

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

// scanChecksum opens path and returns the rendered "SELECT id, body" rows, or the read error.
func scanChecksum(path string) ([][]string, error) {
	db, err := Open(path)
	if err != nil {
		return nil, err
	}
	defer db.Close()
	out, err := Execute(db, "SELECT id, body FROM t ORDER BY id")
	if err != nil {
		return nil, err
	}
	rows := make([][]string, len(out.Rows))
	for i, r := range out.Rows {
		cells := make([]string, len(r))
		for j, v := range r {
			cells[j] = v.Render()
		}
		rows[i] = cells
	}
	return rows, nil
}

func equalRows(a, b [][]string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if len(a[i]) != len(b[i]) {
			return false
		}
		for j := range a[i] {
			if a[i][j] != b[i][j] {
				return false
			}
		}
	}
	return true
}

// seedChecksum builds a file whose tree spans every body-page kind at page_size 256: a multi-leaf
// B-tree (interior root) of ~30 rows, with row 1 a 600-char incompressible body that spills.
func seedChecksum(t *testing.T, path string) {
	t.Helper()
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)"); err != nil {
		t.Fatal(err)
	}
	sql := "INSERT INTO t VALUES (1, '" + fillerText(600) + "')"
	for id := 2; id <= 30; id++ {
		sql += fmt.Sprintf(", (%d, 'row%d')", id, id)
	}
	if _, err := Execute(db, sql); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestCorruptingAnyBodyPageIsCaughtOrInertNeverSilent(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "seed.jed")
	cpath := filepath.Join(dir, "corrupt.jed")
	seedChecksum(t, path)

	want, err := scanChecksum(path)
	if err != nil {
		t.Fatalf("the intact file must scan cleanly: %v", err)
	}
	if len(want) != 30 {
		t.Fatalf("30 rows seeded, got %d", len(want))
	}

	clean, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	const ps = 256
	pages := len(clean) / ps
	if pages < 6 {
		t.Fatalf("the seed should span several pages, got %d", pages)
	}

	// Corrupt one payload byte of each body page in turn (pages 0/1 are the meta slots, checksummed
	// separately — incremental_test.go / reclamation_test.go). The flip is NOT CRC-repaired, so a
	// live page fails its per-page checksum; a dead page is never read and the snapshot is unaffected.
	detected := 0
	for i := 2; i < pages; i++ {
		bytes := make([]byte, len(clean))
		copy(bytes, clean)
		bytes[i*ps+16] ^= 0xFF // first payload byte (offset pageHeader = 16)
		if err := os.WriteFile(cpath, bytes, 0o644); err != nil {
			t.Fatal(err)
		}
		rows, err := scanChecksum(cpath)
		if err != nil {
			if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
				t.Fatalf("corrupting live page %d: want XX001, got %v", i, err)
			}
			detected++
		} else if !equalRows(rows, want) {
			t.Fatalf("corrupting dead page %d must not change results", i)
		}
	}

	// The live pages — catalog, the interior root, several leaves, the overflow chain — are all
	// protected; a floor of 4 guarantees detection fired across page kinds, not just one.
	if detected < 4 {
		t.Fatalf("expected live pages across kinds to be detected, got %d", detected)
	}
}
