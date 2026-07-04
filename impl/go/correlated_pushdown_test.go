package jed

// Correlated primary-key pushdown over a MULTI-LEAF inner table (spec/design/cost.md §3 "bounded
// scan / correlated"). The conformance corpus
// (spec/conformance/suites/subquery/correlated_pushdown.test) pins the cost contract on single-leaf
// tables; this exercises what it cannot — an inner table wide enough that re-scanning it per outer row
// would be visibly expensive, so the per-outer-row seek is the difference between sublinear and
// quadratic. The win is shown by contrast: `inner.pk = o.col` (bounded) vs `inner.v = o.col` (a full
// re-scan), which return the SAME rows because v == id.

import (
	"fmt"
	"strings"
	"testing"
)

// correlatedTables builds `o` (five outer rows whose k-values all exist as inner ids) and `inr` (n
// rows id i32 PRIMARY KEY, v i32; v == id) large enough to span several leaves.
func correlatedTables(t *testing.T, n int) *Session {
	t.Helper()
	var b strings.Builder
	b.WriteString("INSERT INTO inr VALUES ")
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, "(%d,%d)", i, i)
	}
	return dbWith(
		t,
		"CREATE TABLE o (id i32 PRIMARY KEY, k i32)",
		"CREATE TABLE inr (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO o VALUES (1, 100), (2, 300), (3, 500), (4, 700), (5, 900)",
		b.String(),
	)
}

func mustIds(t *testing.T, db dbHandle, sql string) []int64 {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%s: unexpected error %v", sql, err)
	}
	got := make([]int64, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Int
	}
	return got
}

func TestCorrelatedExistsSeekIsSublinear(t *testing.T) {
	const n = 1000
	db := correlatedTables(t, n)

	// Both correlate the inner to each outer row; `inr.id` is the PK (seeks), `inr.v` is not (full
	// re-scan). v == id, so they select the SAME inner rows and the SAME outer rows survive.
	const bounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)"
	const unbounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.v = o.k)"
	if got := mustIds(t, db, bounded); !eqInts(got, 1, 2, 3, 4, 5) {
		t.Errorf("bounded rows = %v, want [1 2 3 4 5]", got)
	}
	if got := mustIds(t, db, unbounded); !eqInts(got, 1, 2, 3, 4, 5) {
		t.Errorf("unbounded rows = %v, want [1 2 3 4 5]", got)
	}

	seek := mustCost(t, db, bounded)
	scan := mustCost(t, db, unbounded)
	// The non-PK correlation re-scans all ~1000 inner rows for each of the 5 outer rows; the PK
	// pushdown seeks instead, so it is an order of magnitude cheaper.
	if seek*10 >= scan {
		t.Errorf("correlated seek %d should be far below the per-outer-row full re-scan %d", seek, scan)
	}
	// Sublinear in the inner size: 5 outer rows, each ≈ a point lookup (height + a row), not ~1000.
	if seek > 400 {
		t.Errorf("correlated seek %d should be sublinear (≈ outer × tree height), not ~5000", seek)
	}
}

func TestCorrelatedScalarSeekMatchesUnboundedRows(t *testing.T) {
	const n = 1000
	db := correlatedTables(t, n)

	// A correlated SCALAR subquery seeking the inner PK returns each outer row's inner value. Rows are
	// identical to what a full re-scan would produce; only the cost differs.
	const bounded = "SELECT (SELECT inr.v FROM inr WHERE inr.id = o.k) FROM o ORDER BY o.id"
	const unbounded = "SELECT (SELECT inr.v FROM inr WHERE inr.v = o.k) FROM o ORDER BY o.id"
	if got := mustIds(t, db, bounded); !eqInts(got, 100, 300, 500, 700, 900) {
		t.Errorf("scalar bounded rows = %v, want [100 300 500 700 900]", got)
	}

	seek := mustCost(t, db, bounded)
	scan := mustCost(t, db, unbounded)
	if seek*10 >= scan {
		t.Errorf("correlated scalar seek %d should be far below the full re-scan %d", seek, scan)
	}
}

func TestCorrelatedMissAndNullOuterSeekNothing(t *testing.T) {
	const n = 1000
	db := correlatedTables(t, n)

	// An outer k with no matching inner id is a point-lookup miss (visits the leaf, reads no row); a
	// NULL outer k is a 3VL-empty bound (reads no page, no row). Neither re-scans the inner.
	if _, err := queryOutcome(db, "INSERT INTO o VALUES (6, 999999), (7, NULL)", nil); err != nil {
		t.Fatal(err)
	}
	const q = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)"
	if got := mustIds(t, db, q); !eqInts(got, 1, 2, 3, 4, 5) {
		t.Errorf("rows = %v, want [1 2 3 4 5] (miss + NULL outer rows drop)", got)
	}
	if seek := mustCost(t, db, q); seek > 500 {
		t.Errorf("seek cost %d should stay sublinear even with a miss and a NULL outer row", seek)
	}
}
