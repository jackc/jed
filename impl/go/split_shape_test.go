package jed

// Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes the
// position-aware split rule produces, observed through the page_read block of a full
// scan (= structural node count, cost.md §3). Ascending inserts take the right-edge
// append split (byte-identical to the old always-largest-left rule, ~full leaves);
// random-order inserts take the balanced split and settle near the classic ~2/3 fill —
// before this rule they splintered into [N-2 | 1] pairs and converged on a few-percent
// fill (the spec/design/benchmarks.md finding). CREATE INDEX inserts its entries sorted
// (indexes.md §1), so a built index packs like the ascending case. Mirrored in
// impl/rust/tests/split_shape.rs and impl/ts/tests/split_shape.test.ts.

import (
	"bytes"
	"fmt"
	"path/filepath"
	"testing"
)

// splitShapeDB is a 121-row table at the fixture page size (256): id bigint pk, v
// integer = id % 7. Ascending inserts pks 0..120 in order; shuffled inserts the
// permutation (i*37) mod 121 — deterministic, identical in every core.
func splitShapeDB(t *testing.T, shuffled bool) *engine {
	t.Helper()
	db, err := create(filepath.Join(t.TempDir(), "t.jed"), databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	siRun(t, db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)")
	for i := 0; i < 121; i++ {
		pk := i
		if shuffled {
			pk = (i * 37) % 121
		}
		siRun(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", pk, pk%7))
	}
	return db
}

func TestSplitShapeCostsArePinned(t *testing.T) {
	t.Parallel()
	pin := func(db *engine, sql string, want int64) {
		t.Helper()
		if got := siCost(t, db, sql); got != want {
			t.Fatalf("%q cost = %d, want %d", sql, got, want)
		}
	}

	// The same logical table costs nearly the same full scan whichever order built it:
	// ascending packs ~full (append splits), shuffled lands a few nodes behind (balanced
	// splits). Under the old always-largest-left rule the shuffled tree splintered into
	// hundreds of near-empty nodes and this cost exploded. The counts dropped from v23
	// (268/278/156) with the v24 B+tree (format.md): interior nodes are a record-free
	// separator skeleton with far higher fan-out, so a full scan touches fewer pages.
	asc := splitShapeDB(t, false)
	pin(asc, "SELECT count(*) FROM t", 258)
	shuf := splitShapeDB(t, true)
	pin(shuf, "SELECT count(*) FROM t", 265)

	// Sorted index build (indexes.md §1) packs the index tree like the ascending case;
	// the build charges only its table scan, and the bounded lookup's cost pins the
	// index path's shape (pk ≡ 3 mod 7 in [0,120] ⇒ 17 admitted rows for v = 3). The
	// lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
	// (interior records could terminate a v23 descent early), so each of the 17 table
	// fetches pays the full root→leaf path.
	pin(shuf, "CREATE INDEX t_v_idx ON t (v)", 143)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 105)

	// The complete INSERT mutation guard: the committed shape above already contains table-leaf and
	// table-interior splits plus a split secondary index. In one working root, overwrite an existing
	// table row/index entry, append enough rows to split both trees again, and pin the resulting exact
	// node counts through cost. Rollback must restore the byte-exact committed image and original costs.
	before, err := shuf.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	tx, err := shuf.Begin(true)
	if err != nil {
		t.Fatal(err)
	}
	siRun(t, shuf, "UPDATE t SET v = 42 WHERE id = 120")
	for id := 121; id <= 220; id++ {
		siRun(t, shuf, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", id, id%7))
	}
	pin(shuf, "SELECT count(*) FROM t", 477)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 199)
	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
	after, err := shuf.ToImage(256, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(after, before) {
		t.Fatal("rollback changed the committed image")
	}
	pin(shuf, "SELECT count(*) FROM t", 265)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 105)
}
