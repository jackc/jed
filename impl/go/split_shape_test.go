package jed

// Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes the
// position-aware split rule produces, observed through the page_read block of a full
// scan (= structural node count, cost.md §3). Ascending inserts take the right-edge
// append split (byte-identical to the old always-largest-left rule, ~full leaves);
// random-order inserts even out with a sibling or take the balanced split — before the
// position-aware rule they splintered into [N-2 | 1] pairs and converged on a few-percent
// fill (the spec/design/benchmarks.md finding). CREATE INDEX inserts its entries sorted
// (indexes.md §1), so a built index packs like the ascending case. Mirrored in
// impl/rust/tests/split_shape.rs and impl/ts/tests/split_shape.test.ts.

import (
	"bytes"
	"fmt"
	"path/filepath"
	"testing"
)

// splitShapeDB is a 1210-row table at the fixture page size (256): id bigint pk, v
// integer = id % 7. Ascending inserts pks 0..1209 in order; shuffled inserts the
// permutation (i*37) mod 1210 — deterministic, identical in every core. (121 rows until
// format v52, whose leaves hold about ten times as many of these rows.)
func splitShapeDB(t *testing.T, shuffled bool) *engine {
	t.Helper()
	return splitShapeOrderDB(t, func(i int) int {
		if shuffled {
			return (i * 37) % 1210
		}
		return i
	})
}

// splitShapeOrderDB is the same table with its 1210 rows inserted in the order pk(0), pk(1), ….
func splitShapeOrderDB(t *testing.T, pk func(i int) int) *engine {
	t.Helper()
	db, err := create(filepath.Join(t.TempDir(), "t.jed"), databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	siRun(t, db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)")
	for i := 0; i < 1210; i++ {
		siRun(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", pk(i), pk(i)%7))
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
	pin(asc, "SELECT count(*) FROM t", 2436)
	shuf := splitShapeDB(t, true)
	pin(shuf, "SELECT count(*) FROM t", 2436)

	// Evening out with a sibling (format.md): a leaf that overflows away from the tree's right edge
	// shares its records with a sibling that has room before it splits, which is why the shuffled
	// tree costs exactly what the ascending one does (2443 before the rule). Descending inserts
	// overflow at each leaf's left edge and even out with the right sibling; the zigzag order
	// (0, 1209, 1, 1208, …) grows two fronts that meet in the middle, so its overflows find room on
	// either side. Both land one node behind.
	pin(splitShapeOrderDB(t, func(i int) int { return 1209 - i }), "SELECT count(*) FROM t", 2437)
	pin(splitShapeOrderDB(t, func(i int) int {
		if i%2 == 0 {
			return i / 2
		}
		return 1209 - i/2
	}), "SELECT count(*) FROM t", 2437)

	// Sorted index build (indexes.md §1) packs the index tree like the ascending case;
	// the build charges only its table scan, and the bounded lookup's cost pins the
	// index path's shape (pk ≡ 3 mod 7 in [0,1209] ⇒ 173 admitted rows for v = 3). The
	// lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
	// (interior records could terminate a v23 descent early), so each of the 17 table
	// fetches pays the full root→leaf path. Re-pinned for format v52 (the compact leaf), and again
	// when evening out left the shuffled table tree one level shorter (1045 before).
	pin(shuf, "CREATE INDEX t_v_idx ON t (v)", 1225)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 872)

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
	siRun(t, shuf, "UPDATE t SET v = 42 WHERE id = 1209")
	for id := 1210; id <= 2209; id++ {
		siRun(t, shuf, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", id, id%7))
	}
	pin(shuf, "SELECT count(*) FROM t", 4450)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 1907)
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
	pin(shuf, "SELECT count(*) FROM t", 2436)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 872)
}
