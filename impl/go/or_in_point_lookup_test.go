package jed

import (
	"fmt"
	"strings"
	"testing"
)

// OR / IN-list → merged point lookups (spec/design/cost.md §3 "OR / IN-list"). The conformance corpus
// (spec/conformance/suites/query/or_in_point_lookup.test) pins the cost contract on single-leaf tables
// and the EXPLAIN access-path label; this exercises what it cannot — a MULTI-leaf B-tree where the
// point-set's page_read genuinely drops below the full node count, and the routing invariant that a
// point-set bound takes the eager materialize path (never a fast path that would silently full-scan).

// bigTableAV builds `t (id i32 PRIMARY KEY, a i32)` with n rows (a == id*10), large enough to span
// several B-tree leaves at the default page size.
func bigTableAV(t *testing.T, n int) *Session {
	t.Helper()
	var b strings.Builder
	b.WriteString("INSERT INTO t VALUES ")
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, "(%d,%d)", i, i*10)
	}
	return dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)", b.String())
}

func TestOrInPointSetMultiLeaf(t *testing.T) {
	t.Parallel()
	const n = 1000
	db := bigTableAV(t, n)

	store := db.engine.readSnap().store("t")
	if store.NodeCount() <= 1 {
		t.Fatalf("test needs a multi-leaf tree; got nodeCount=%d (raise n)", store.NodeCount())
	}

	full, err := queryOutcome(db, "SELECT a FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}

	// An IN-list of three scattered keys reads exactly three rows at cost far below the full scan: a
	// union of three point probes, not a walk of 1000 rows (the merged-point-lookup win).
	out, err := queryOutcome(db, "SELECT a FROM t WHERE id IN (10, 500, 990)", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 3 {
		t.Fatalf("IN(10,500,990) got %d rows, want 3", len(out.Rows))
	}
	got := map[int64]bool{}
	for _, r := range out.Rows {
		got[r[0].Int] = true
	}
	if !got[100] || !got[5000] || !got[9900] {
		t.Errorf("IN(10,500,990) rows = %v, want {100,5000,9900}", out.Rows)
	}
	if out.Cost >= full.Cost {
		t.Errorf("IN-list point-set cost %d should be far below full-scan cost %d", out.Cost, full.Cost)
	}
	if out.Cost > 60 {
		t.Errorf("IN-list point-set cost %d should be small (≈ 3 probes), not ~%d", out.Cost, n)
	}

	// The OR spelling is the identical bound.
	or, err := queryOutcome(db, "SELECT a FROM t WHERE id = 10 OR id = 500 OR id = 990", nil)
	if err != nil {
		t.Fatal(err)
	}
	if or.Cost != out.Cost {
		t.Errorf("OR spelling cost %d should equal the IN spelling cost %d", or.Cost, out.Cost)
	}

	// A mix of hits and misses: only the present keys yield rows; every probe still charges its path.
	mix, err := queryOutcome(db, "SELECT a FROM t WHERE id IN (7, 99999, 42)", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(mix.Rows) != 2 {
		t.Errorf("IN(7,99999,42) got %d rows, want 2 (99999 misses)", len(mix.Rows))
	}
}

// Bind parameters and a correlated outer column flow through the interval-set encode path, which
// resolves reParam / reOuterColumn per the same rules as the single point-lookup bound.
func TestOrInPointSetParamsAndCorrelated(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, a i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)")

	// Bind params in the IN-list.
	out, err := queryOutcome(db, "SELECT a FROM t WHERE id IN ($1, $2, $3) ORDER BY a", []Value{
		IntValue(1), IntValue(3), IntValue(99),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 2 || out.Rows[0][0].Int != 10 || out.Rows[1][0].Int != 30 {
		t.Fatalf("IN($1,$2,$3) got %v, want [10, 30]", out.Rows)
	}

	// A duplicate param value de-duplicates to one probe (same as a literal duplicate).
	dup, err := queryOutcome(db, "SELECT a FROM t WHERE id IN ($1, $2)", []Value{IntValue(2), IntValue(2)})
	if err != nil {
		t.Fatal(err)
	}
	if len(dup.Rows) != 1 || dup.Rows[0][0].Int != 20 {
		t.Fatalf("IN($1,$2) with equal params got %v, want [20]", dup.Rows)
	}

	// Runtime endpoints are canonicalized only after binding: adjacent ranges merge, and the
	// co-present clip removes points below its lower endpoint.
	ranges, err := queryOutcome(db, "SELECT id FROM t WHERE id < $1 OR id >= $2 ORDER BY id", []Value{
		IntValue(3), IntValue(4),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(ranges.Rows) != 3 || ranges.Rows[0][0].Int != 1 || ranges.Rows[1][0].Int != 2 || ranges.Rows[2][0].Int != 4 {
		t.Fatalf("parameter interval union got %v, want [1, 2, 4]", ranges.Rows)
	}
	clipped, err := queryOutcome(db, "SELECT id FROM t WHERE id IN ($1, $2, $3) AND id > $4 ORDER BY id", []Value{
		IntValue(1), IntValue(3), IntValue(4), IntValue(2),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(clipped.Rows) != 2 || clipped.Rows[0][0].Int != 3 || clipped.Rows[1][0].Int != 4 {
		t.Fatalf("parameter interval clip got %v, want [3, 4]", clipped.Rows)
	}

	// A correlated OR of the inner PK against outer columns bounds the inner per outer row.
	corr := queryIDs(t, db, `SELECT o.id FROM t o WHERE EXISTS (SELECT 1 FROM t i WHERE i.id = o.id OR i.id = o.a) ORDER BY o.id`)
	// Every outer row satisfies EXISTS (i.id = o.id always matches the same row), so all four survive.
	if len(corr) != 4 {
		t.Errorf("correlated OR-EXISTS got %v, want all 4 ids", corr)
	}
}

// A count(*) over an IN-list is aggregate-eligible; the point-set bound routes it through the eager
// path (the vectorized-aggregate fast path declines a point-set bound), so the answer and the
// sublinear cost are both correct.
func TestOrInPointSetAggregate(t *testing.T) {
	t.Parallel()
	const n = 1000
	db := bigTableAV(t, n)
	full, err := queryOutcome(db, "SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	out, err := queryOutcome(db, "SELECT count(*) FROM t WHERE id IN (1, 2, 3, 4, 5)", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 5 {
		t.Fatalf("count(*) over IN(1..5) = %v, want 5", out.Rows)
	}
	if out.Cost >= full.Cost {
		t.Errorf("count over point-set cost %d should be below full-scan count cost %d", out.Cost, full.Cost)
	}
}

// UPDATE / DELETE bound by the PK point set seek the listed rows at sublinear cost and leave the rest
// intact (the mutation analog of the SELECT bound).
func TestOrInPointSetMutation(t *testing.T) {
	t.Parallel()
	const n = 1000
	db := bigTableAV(t, n)

	d, err := queryOutcome(db, "DELETE FROM t WHERE id IN (100, 300, 500)", nil)
	if err != nil {
		t.Fatal(err)
	}
	if d.Cost > 60 {
		t.Errorf("DELETE point-set cost %d should be small (sublinear in %d)", d.Cost, n)
	}
	if got := queryIDs(t, db, "SELECT id FROM t WHERE id IN (100, 300, 500)"); len(got) != 0 {
		t.Errorf("rows 100,300,500 should be deleted, got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t WHERE id IN (99, 301)"); len(got) != 2 {
		t.Errorf("neighbouring rows must survive the point-set delete, got %v", got)
	}

	u, err := queryOutcome(db, "UPDATE t SET a = -1 WHERE id = 200 OR id = 400", nil)
	if err != nil {
		t.Fatal(err)
	}
	if u.Cost > 60 {
		t.Errorf("UPDATE point-set cost %d should be small", u.Cost)
	}
	if rows := query(t, db, "SELECT a FROM t WHERE id IN (200, 400)"); len(rows) != 2 ||
		rows[0][0].Int != -1 || rows[1][0].Int != -1 {
		t.Errorf("rows 200,400 should be updated to -1, got %v", rows)
	}
}

// EXPLAIN surfaces the point-set access path (the label the corpus pins on single-leaf, re-asserted
// here as the introspection contract; also the secondary-index variant).
func TestOrInPointSetExplain(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, a i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
		"CREATE TABLE s (id i32 PRIMARY KEY, x i32)",
		"INSERT INTO s VALUES (1, 10), (2, 20)",
		"CREATE INDEX sx ON s (x)")

	cases := []struct{ sql, want string }{
		{"EXPLAIN SELECT a FROM t WHERE id IN (3, 1)", "PK interval set: id; intervals=2"},
		{"EXPLAIN SELECT a FROM t WHERE id = 1 OR id = 2", "PK interval set: id; intervals=2"},
		// P6b whole-pipeline costing prefers a full scan for this tiny secondary-index relation.
		{"EXPLAIN SELECT id FROM s WHERE x IN (10, 20)", "Full scan"},
		// DML lowers the PK point set too; secondary-index mutation point sets are covered by corpus.
		{"EXPLAIN DELETE FROM t WHERE id IN (1, 3)", "PK interval set: id; intervals=2"},
		{"EXPLAIN UPDATE t SET a = 0 WHERE id = 2 OR id = 3", "PK interval set: id; intervals=2"},
	}
	for _, c := range cases {
		out, err := queryOutcome(db, c.sql, nil)
		if err != nil {
			t.Fatalf("%q: %v", c.sql, err)
		}
		found := false
		for _, r := range out.Rows {
			for _, v := range r {
				if v.Kind == ValText && strings.Contains(v.str(), c.want) {
					found = true
				}
			}
		}
		if !found {
			t.Errorf("%q: EXPLAIN did not surface %q; got %v", c.sql, c.want, out.Rows)
		}
	}
}
