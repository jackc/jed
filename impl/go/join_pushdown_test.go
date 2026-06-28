package jed

// Join base-table primary-key pushdown over a MULTI-LEAF table (spec/design/cost.md §3 "bounded scan
// / JOIN"). The conformance corpus (spec/conformance/suites/joins/pushdown.test) pins the cost
// contract on single-leaf tables; this exercises what it cannot — a join base table wide enough that
// a full materialization would be expensive, so bounding it by its own primary key is the difference
// between sublinear and a full double scan. The win is shown by contrast: `WHERE a.id = c` (a's PK,
// bounded) vs `WHERE a.k = c` (not the PK, full scan), which return the SAME row because k == id.

import (
	"fmt"
	"strings"
	"testing"
)

// joinTables builds `a` (n rows id i32 PRIMARY KEY, k i32; k == id) wide enough to span several
// leaves, and `b` (three small rows whose k-values exist as a's k-values, so the join matches).
func joinTables(t *testing.T, n int) *engine {
	t.Helper()
	var b strings.Builder
	b.WriteString("INSERT INTO a VALUES ")
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, "(%d,%d)", i, i)
	}
	return dbWith(
		t,
		"CREATE TABLE a (id i32 PRIMARY KEY, k i32)",
		"CREATE TABLE b (id i32 PRIMARY KEY, k i32)",
		b.String(),
		"INSERT INTO b VALUES (1, 500), (2, 600), (3, 700)",
	)
}

func TestJoinPushdownBoundsOneSideSublinear(t *testing.T) {
	const n = 1000
	db := joinTables(t, n)

	// Both pick the single a row with id/k == 500 and join it to b(k=500); `a.id` is the PK (seeks a),
	// `a.k` is not (full scan of a). k == id, so they return the SAME row.
	const bounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500"
	const unbounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.k = 500"
	if got := mustIds(t, db, bounded); !eqInts(got, 500) {
		t.Errorf("bounded rows = %v, want [500]", got)
	}
	if got := mustIds(t, db, unbounded); !eqInts(got, 500) {
		t.Errorf("unbounded rows = %v, want [500]", got)
	}

	seek := mustCost(t, db, bounded)
	scan := mustCost(t, db, unbounded)
	// The non-PK predicate full-scans all ~1000 a rows; the PK pushdown materializes one a row.
	if seek*10 >= scan {
		t.Errorf("bounded join %d should be far below the full-scan join %d", seek, scan)
	}
	if seek > 60 {
		t.Errorf("bounded join %d should be sublinear (seek a + scan small b), not ~1000", seek)
	}
}

func TestJoinPushdownMissCollapsesToEmpty(t *testing.T) {
	const n = 1000
	db := joinTables(t, n)

	// A point-lookup miss on the bounded side materializes ZERO a rows, so the loop has nothing to
	// drive: empty result at the cost of (a's miss page) + (b's full scan), not a 1000-row scan.
	const q = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 999999"
	if got := mustIds(t, db, q); len(got) != 0 {
		t.Errorf("rows = %v, want empty", got)
	}
	if miss := mustCost(t, db, q); miss > 60 {
		t.Errorf("a miss-bounded join %d should collapse to b's small scan, not ~1000", miss)
	}
}

func TestJoinPushdownBothSidesBounded(t *testing.T) {
	const n = 1000
	db := joinTables(t, n)

	// Bounding BOTH tables by their own PK: a.id = 500 (one a row, k=500) and b.id = 1 (one b row,
	// k=500). They join on k. Sublinear in a's size.
	const q = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500 AND b.id = 1"
	if got := mustIds(t, db, q); !eqInts(got, 500) {
		t.Errorf("rows = %v, want [500]", got)
	}
	if c := mustCost(t, db, q); c > 30 {
		t.Errorf("both-sides-bounded join %d should be tiny, not ~1000", c)
	}
}
