package jed

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// A base-plus-offsets region with exceptions (leaf_pfor.go) must read back the rows it was written
// from, and a leaf's size from its page must equal the size measured from its rows, through inserts,
// in-place updates that create and remove outliers, and deletes. The workload runs with the lazy-leaf
// size and patch cross-checks on, without exceptions (kind 1 alone) and under several caps; the query
// results must agree.
func TestLeafPFORRoundTrip(t *testing.T) {
	saved := leafPFOR
	defer func() { leafPFOR = saved }()
	leafPFOR = 0
	plain := forxWorkload(t)
	for _, k := range []int{1, 4, pforMaxK} {
		leafPFOR = k
		before := pforWritten.Load()
		if got := forxWorkload(t); got != plain {
			t.Fatalf("cap %d: rows with exceptions:\n%s\nwithout:\n%s", k, got, plain)
		}
		if pforWritten.Load() == before {
			t.Fatalf("cap %d: the workload wrote no region with exceptions", k)
		}
	}
}

func forxWorkload(t *testing.T) string {
	t.Helper()
	defer leafSizesCheck.Store(leafSizesCheck.Load())
	defer leafPatchCheck.Store(leafPatchCheck.Load())
	leafSizesCheck.Store(true)
	leafPatchCheck.Store(true)

	path := filepath.Join(t.TempDir(), "forx.jed")
	db, err := Create(CreateOptions{Path: path, PageSize: 4096, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	sess := mustSession(db, SessionOptions{})
	reopen := func() {
		t.Helper()
		sess.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		if db, err = Open(path, OpenOptions{SkipFsync: true}); err != nil {
			t.Fatal(err)
		}
		sess = mustSession(db, SessionOptions{})
	}
	var out strings.Builder
	query := func(sql string) {
		t.Helper()
		for _, row := range renderRows(queryRows(t, sess, sql)) {
			fmt.Fprintf(&out, "%s\n", strings.Join(row, "|"))
		}
	}
	mustExec(t, sess, "CREATE TABLE f (id bigint PRIMARY KEY, a integer, b bigint, c smallint, d date, e bigint)")
	mustExec(t, sess, `INSERT INTO f SELECT x, x % 7, CASE WHEN x % 97 = 0 THEN 9000000000 + x ELSE x % 50 END,
		CASE WHEN x % 13 = 0 THEN NULL ELSE 3 END, DATE '2024-01-01' + (x % 3),
		CASE WHEN x % 29 = 0 THEN NULL ELSE x * 1000 + x % 3 END FROM generate_series(1, 3000) AS g(x)`)
	reopen()
	mustExec(t, sess, "UPDATE f SET a = 2000000000 WHERE id = 10")
	mustExec(t, sess, "UPDATE f SET c = -32000 WHERE id = 500")
	mustExec(t, sess, "UPDATE f SET a = -2000000000 WHERE id IN (1200, 1201)")
	reopen()
	query("SELECT count(*), sum(a), sum(b), sum(c), min(d), max(d), sum(e) FROM f")
	mustExec(t, sess, "UPDATE f SET a = 3 WHERE id = 10")
	mustExec(t, sess, "DELETE FROM f WHERE id % 97 = 0 AND id < 1500")
	mustExec(t, sess, "INSERT INTO f VALUES (5000, 1, 7000000000000, NULL, DATE '1999-12-31', 123456789012)")
	reopen()
	query("SELECT count(*), sum(a), sum(b), sum(c), min(d), max(d), sum(e) FROM f")
	mustExec(t, sess, "UPDATE f SET e = -5 WHERE id % 300 = 7")
	mustExec(t, sess, "DELETE FROM f WHERE id BETWEEN 2000 AND 2100")
	reopen()
	query("SELECT count(*), sum(e), min(e), max(e) FROM f")
	query("SELECT id, a, b, c, d, e FROM f WHERE id IN (10, 97, 500, 1200, 1201, 1552, 1807, 5000) ORDER BY id")
	sess.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	file, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	checkPackedSizes(t, file, "f")
	return out.String()
}

// The same for base-plus-offsets regions in runs (leaf_runs.go), with each lookup index.
func TestLeafRunsRoundTrip(t *testing.T) {
	saved := leafRuns
	defer func() { leafRuns = saved }()
	leafRuns = leafRunsOff
	plain := forxWorkload(t)
	for _, mode := range []int{leafRunsSearch, leafRunsRank} {
		leafRuns = mode
		before := runsWritten.Load()
		if got := forxWorkload(t); got != plain {
			t.Fatalf("mode %d: rows in runs:\n%s\nwithout:\n%s", mode, got, plain)
		}
		if runsWritten.Load() == before {
			t.Fatalf("mode %d: the workload wrote no region in runs", mode)
		}
	}
}

// The same for base-plus-offsets regions in fixed blocks (leaf_blocks.go), with and without the
// exception on top.
func TestLeafBlocksRoundTrip(t *testing.T) {
	saved, savedExc := leafBlocks, leafBlocksExc
	defer func() { leafBlocks, leafBlocksExc = saved, savedExc }()
	leafBlocks, leafBlocksExc = 0, false
	plain := forxWorkload(t)
	for _, b := range []int{16, 64} {
		for _, exc := range []bool{false, true} {
			leafBlocks, leafBlocksExc = b, exc
			before := blocksWritten.Load()
			if got := forxWorkload(t); got != plain {
				t.Fatalf("blocks of %d (exception %v): rows:\n%s\nwithout:\n%s", b, exc, got, plain)
			}
			if blocksWritten.Load() == before {
				t.Fatalf("blocks of %d (exception %v): the workload wrote no region in blocks", b, exc)
			}
		}
	}
}
