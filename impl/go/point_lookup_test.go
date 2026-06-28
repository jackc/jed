package jed

// Primary-key predicate pushdown over a MULTI-LEAF B-tree (spec/design/cost.md §3 "bounded scan").
// The conformance corpus (spec/conformance/suites/query/point_lookup.test) pins the cost contract on
// single-leaf tables; this exercises what it cannot — a tree tall/wide enough that page_read actually
// drops below the full node count, and a range scan that spans leaf boundaries.

import (
	"fmt"
	"strings"
	"testing"
)

// bigTable builds a table of n rows (id i32 PRIMARY KEY, v i32; v == id) large enough to span
// several B-tree leaves at the default page size.
func bigTable(t *testing.T, n int) *engine {
	t.Helper()
	var b strings.Builder
	b.WriteString("INSERT INTO t VALUES ")
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, "(%d,%d)", i, i)
	}
	return dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", b.String())
}

func TestPointLookupMultiLeaf(t *testing.T) {
	const n = 1000
	db := bigTable(t, n)

	store := db.readSnap().store("t")
	nodeCount := store.NodeCount()
	if nodeCount <= 1 {
		t.Fatalf("test needs a multi-leaf tree; got nodeCount=%d (raise n)", nodeCount)
	}

	// Storage primitive: a point bound visits strictly fewer nodes than the whole tree (the page_read
	// win), and returns exactly the one matching row.
	key := encodeInt(scalarInt32, 500)
	pb := keyBound{lo: key, loInc: true, hi: key, hiInc: true}
	if got := store.OverlapNodeCount(pb); got >= nodeCount {
		t.Errorf("point-lookup overlap node count %d should be < full node count %d", got, nodeCount)
	}
	rows, err := store.RangeRows(pb)
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || rows[0][0].Int != 500 {
		t.Errorf("RangeRows point bound got %v, want one row id=500", rows)
	}

	// End-to-end point lookup: exactly one row, and the cost is sublinear in n (it did not full-scan).
	out, err := execute(db, "SELECT v FROM t WHERE id = 500")
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 500 {
		t.Errorf("point lookup got %v, want [500]", out.Rows)
	}
	full, err := execute(db, "SELECT v FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if out.Cost >= full.Cost {
		t.Errorf("point-lookup cost %d should be far below full-scan cost %d", out.Cost, full.Cost)
	}
	if out.Cost > 50 {
		t.Errorf("point-lookup cost %d should be small (≈ tree height + a few), not ~%d", out.Cost, n)
	}

	// A point-lookup MISS still visits the leaf it would live in (page_read > 0) but reads no row.
	miss, err := execute(db, "SELECT v FROM t WHERE id = 99999")
	if err != nil {
		t.Fatal(err)
	}
	if len(miss.Rows) != 0 || miss.Cost == 0 || miss.Cost > 50 {
		t.Errorf("point-lookup miss got rows=%d cost=%d, want 0 rows and a small non-zero cost", len(miss.Rows), miss.Cost)
	}
}

func TestRangeScanCrossesLeafBoundaries(t *testing.T) {
	const n = 1000
	db := bigTable(t, n)

	// A range that spans many leaves returns the contiguous, correct rows in key order — the property
	// the single-leaf corpus cannot exercise (the leaf-crossing in-order traversal).
	got := queryIDs(t, db, "SELECT id FROM t WHERE id >= 300 AND id <= 700 ORDER BY id")
	if len(got) != 401 {
		t.Fatalf("range scan got %d rows, want 401", len(got))
	}
	for i, id := range got {
		if id != int64(300+i) {
			t.Fatalf("range row %d = %d, want %d", i, id, 300+i)
		}
	}

	// Open-ended range to the end of the key space.
	tail := queryIDs(t, db, "SELECT id FROM t WHERE id > 996 ORDER BY id")
	if len(tail) != 4 || tail[0] != 997 || tail[3] != 1000 {
		t.Errorf("open range got %v, want [997..1000]", tail)
	}

	// Empty (contradictory) bound: zero rows, zero cost — proved without scanning.
	empty, err := execute(db, "SELECT id FROM t WHERE id > 700 AND id < 300")
	if err != nil {
		t.Fatal(err)
	}
	if len(empty.Rows) != 0 || empty.Cost != 0 {
		t.Errorf("empty bound got rows=%d cost=%d, want 0 rows and cost 0", len(empty.Rows), empty.Cost)
	}
}

func TestLimitShortCircuitMultiLeaf(t *testing.T) {
	const n = 1000
	db := bigTable(t, n) // id 1..1000, v == id

	// LIMIT without ORDER BY stops the scan early: it returns `limit` rows at sublinear cost (it did
	// NOT read all 1000). The rows are the primary-key-order prefix (our cores' deterministic choice).
	out, err := execute(db, "SELECT v FROM t LIMIT 5")
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 5 {
		t.Fatalf("LIMIT 5 got %d rows, want 5", len(out.Rows))
	}
	full, err := execute(db, "SELECT v FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if out.Cost >= full.Cost {
		t.Errorf("LIMIT cost %d should be far below the full-scan cost %d", out.Cost, full.Cost)
	}
	if out.Cost > 20 {
		t.Errorf("LIMIT 5 cost %d should be sublinear (≈ limit + node count), not ~%d", out.Cost, n)
	}
	got := queryIDs(t, db, "SELECT v FROM t LIMIT 5")
	for i, v := range got {
		if v != int64(i+1) {
			t.Fatalf("LIMIT 5 row %d = %d, want %d", i, v, i+1)
		}
	}

	// OFFSET skips then takes, still sublinear.
	o := queryIDs(t, db, "SELECT v FROM t LIMIT 3 OFFSET 10")
	if len(o) != 3 || o[0] != 11 || o[2] != 13 {
		t.Errorf("LIMIT 3 OFFSET 10 got %v, want [11 12 13]", o)
	}

	// Trap windowing: streaming projects ONLY the windowed rows (like the eager path), so a later
	// row that would trap is never reached under a LIMIT that excludes it.
	dz := dbWith(t,
		"CREATE TABLE z (id i32 PRIMARY KEY, c i32)",
		"INSERT INTO z VALUES (1, 5), (2, 0), (3, 5)")
	if rows := query(t, dz, "SELECT 100 / c FROM z LIMIT 1"); len(rows) != 1 || rows[0][0].Int != 20 {
		t.Errorf("LIMIT 1 should produce only the safe first row (100/5=20), got %v", rows)
	}
	if _, err := execute(dz, "SELECT 100 / c FROM z LIMIT 2"); err == nil {
		t.Errorf("LIMIT 2 reaches the c=0 row and must trap (division by zero)")
	}
}

func TestMutationPushdownMultiLeaf(t *testing.T) {
	const n = 1000
	db := bigTable(t, n)

	// DELETE by primary key seeks one row at sublinear cost and leaves the rest intact.
	d, err := execute(db, "DELETE FROM t WHERE id = 500")
	if err != nil {
		t.Fatal(err)
	}
	if d.Cost > 50 {
		t.Errorf("DELETE point-lookup cost %d should be small (sublinear in %d)", d.Cost, n)
	}
	if got := queryIDs(t, db, "SELECT id FROM t WHERE id = 500"); len(got) != 0 {
		t.Errorf("row 500 should be deleted, got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t WHERE id = 501"); len(got) != 1 {
		t.Errorf("neighbouring rows must survive the point delete")
	}

	// UPDATE by primary key likewise.
	u, err := execute(db, "UPDATE t SET v = -1 WHERE id = 700")
	if err != nil {
		t.Fatal(err)
	}
	if u.Cost > 50 {
		t.Errorf("UPDATE point-lookup cost %d should be small", u.Cost)
	}
	if got := query(t, db, "SELECT v FROM t WHERE id = 700"); got[0][0].Int != -1 {
		t.Errorf("row 700 should be updated to -1, got %v", got[0][0].Int)
	}
}
