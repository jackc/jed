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
	"fmt"
	"path/filepath"
	"testing"
)

// splitShapeDB is a 121-row table at the fixture page size (256): id bigint pk, v
// integer = id % 7. Ascending inserts pks 0..120 in order; shuffled inserts the
// permutation (i*37) mod 121 — deterministic, identical in every core.
func splitShapeDB(t *testing.T, shuffled bool) *engine {
	t.Helper()
	db, err := create(filepath.Join(t.TempDir(), "t.jed"), DatabaseOptions{PageSize: 256})
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
	pin := func(db *engine, sql string, want int64) {
		t.Helper()
		if got := siCost(t, db, sql); got != want {
			t.Fatalf("%q cost = %d, want %d", sql, got, want)
		}
	}

	// The same logical table costs nearly the same full scan whichever order built it:
	// ascending packs ~full (append splits), shuffled lands ~2 nodes behind (balanced
	// splits). Under the old always-largest-left rule the shuffled tree splintered into
	// hundreds of near-empty nodes and this cost exploded. The counts rose from the pre-v23
	// row-major layout (259/261/139/103) because a PAX leaf's per-column directory overhead
	// (format.md v23) lowers leaf fan-out, so a scan touches a few more pages.
	asc := splitShapeDB(t, false)
	pin(asc, "SELECT count(*) FROM t", 268)
	shuf := splitShapeDB(t, true)
	pin(shuf, "SELECT count(*) FROM t", 278)

	// Sorted index build (indexes.md §1) packs the index tree like the ascending case;
	// the build charges only its table scan, and the bounded lookup's cost pins the
	// index path's shape (pk ≡ 3 mod 7 in [0,120] ⇒ 17 admitted rows for v = 3).
	pin(shuf, "CREATE INDEX t_v_idx ON t (v)", 156)
	pin(shuf, "SELECT id FROM t WHERE v = 3", 100)
}
