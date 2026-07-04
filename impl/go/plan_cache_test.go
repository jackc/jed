package jed

// White-box tests for the prepared-statement plan cache (spec/design/api.md §2.4). A prepared
// statement caches its resolved scan plan and reuses it across executes, re-planning only when the
// catalog changes. The behavior is invisible to the conformance corpus (which never reuses a plan
// across statements), and the interesting properties are internal — that a hit
// reuses the exact plan (not re-plans), that reuse is cost-identical, that DDL invalidates, that a
// subquery / precompiled-regex / temp plan is never cached, and that a plan first executed inside a
// transaction is not cached from the uncommitted working set — so they live here, not in the corpus
// (CLAUDE.md §10).

import (
	"fmt"
	"testing"
)

// drainQ runs a prepared query with params, fully drains it, and returns the rows as an int matrix
// (every column read via Value.Int — the tests use integer columns) plus the final accrued cost.
func drainQ(t *testing.T, stmt *PreparedStatement, params ...Value) ([][]int64, int64) {
	t.Helper()
	rows, err := stmt.QueryValues(params)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	var out [][]int64
	for rows.Next() {
		src := rows.Row()
		row := make([]int64, len(src))
		for i, v := range src {
			row[i] = v.Int
		}
		out = append(out, row)
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain: %v", err)
	}
	return out, rows.Cost()
}

func planCacheRowsEq(got [][]int64, want [][]int64) bool {
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		if len(got[i]) != len(want[i]) {
			return false
		}
		for j := range got[i] {
			if got[i][j] != want[i][j] {
				return false
			}
		}
	}
	return true
}

func seedOrders(t *testing.T, db *Session, n int) {
	t.Helper()
	mustExec(t, db, "CREATE TABLE orders (id i32 PRIMARY KEY, amount i32)")
	for i := 1; i <= n; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO orders VALUES (%d, %d)", i, i*100))
	}
}

// A point lookup fills the cache on the first execute and REUSES the exact plan (same pointer) on
// later executes — no re-plan — and reuse is cost-identical (the regex-cost-drift guard: if a plan
// with per-execution mutable cost state were cached, the 2nd execute would report a different cost).
func TestPlanCachePointLookupReuses(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	seedOrders(t, db, 5)
	stmt, err := db.Prepare("SELECT id, amount FROM orders WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	// Execute 1 (MISS): plans and fills the cache.
	r1, cost1 := drainQ(t, stmt, IntValue(3))
	if !stmt.sc.valid {
		t.Fatal("expected the cache to fill on the first cacheable execute")
	}
	sp := stmt.sc.sp
	if !planCacheRowsEq(r1, [][]int64{{3, 300}}) {
		t.Fatalf("execute 1 rows = %v", r1)
	}

	// Execute 2 (HIT, same param): the exact cached plan is reused (pointer unchanged) and cost is
	// identical to the fresh plan.
	r2, cost2 := drainQ(t, stmt, IntValue(3))
	if stmt.sc.sp != sp {
		t.Fatal("plan pointer changed on a cache hit — statement re-planned")
	}
	if cost2 != cost1 {
		t.Fatalf("cache-hit cost = %d, want %d (reuse must be cost-identical)", cost2, cost1)
	}
	if !planCacheRowsEq(r2, [][]int64{{3, 300}}) {
		t.Fatalf("execute 2 rows = %v", r2)
	}

	// Execute 3 (HIT, different param): plan still reused, params still bound per execute.
	r3, _ := drainQ(t, stmt, IntValue(5))
	if stmt.sc.sp != sp {
		t.Fatal("plan pointer changed on a param-only change")
	}
	if !planCacheRowsEq(r3, [][]int64{{5, 500}}) {
		t.Fatalf("execute 3 rows = %v", r3)
	}

	// A no-match param still binds correctly against the cached plan.
	r4, _ := drainQ(t, stmt, IntValue(999))
	if len(r4) != 0 {
		t.Fatalf("execute 4 (no match) rows = %v", r4)
	}
}

// DROP INDEX bumps the catalog generation on a table that survives, so the next execute re-plans and
// falls back from the (now-gone) index lookup to a full scan — a stale cached index plan would try to
// use a dropped index. Exercises the removeIndex catGen bump.
func TestPlanCacheDropIndexInvalidation(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	for i := 1; i <= 50; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i))
	}
	mustExec(t, db, "CREATE INDEX t_a ON t (a)")
	stmt, err := db.Prepare("SELECT id FROM t WHERE a = $1")
	if err != nil {
		t.Fatal(err)
	}

	rIdx, costIdx := drainQ(t, stmt, IntValue(25)) // index lookup
	if !planCacheRowsEq(rIdx, [][]int64{{25}}) {
		t.Fatalf("index rows = %v", rIdx)
	}
	if !stmt.sc.valid {
		t.Fatal("expected fill")
	}
	gen1 := stmt.sc.catGen

	mustExec(t, db, "DROP INDEX t_a")
	rScan, costScan := drainQ(t, stmt, IntValue(25)) // re-plan → full scan
	if !planCacheRowsEq(rScan, [][]int64{{25}}) {
		t.Fatalf("rows after DROP INDEX = %v", rScan)
	}
	if costScan <= costIdx {
		t.Fatalf("expected full scan costlier than index after DROP INDEX: scan=%d idx=%d "+
			"(stale index plan served?)", costScan, costIdx)
	}
	if stmt.sc.catGen == gen1 {
		t.Fatal("catGen did not advance after DROP INDEX — plan cache would serve a stale plan")
	}
}

// DROP + re-CREATE with a different shape must re-plan (the rollback-collision guard's positive case:
// under fill-only-from-committed the new committed generation differs).
func TestPlanCacheDropCreateInvalidation(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10)")
	stmt, err := db.Prepare("SELECT * FROM t WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	r1, _ := drainQ(t, stmt, IntValue(1))
	if len(r1[0]) != 2 {
		t.Fatalf("before = %v", r1)
	}

	mustExec(t, db, "DROP TABLE t")
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, c i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10, 20)")

	r2, _ := drainQ(t, stmt, IntValue(1))
	if len(r2) != 1 || len(r2[0]) != 3 || r2[0][2] != 20 {
		t.Fatalf("after DROP/CREATE = %v, want one 3-column row {1,10,20}", r2)
	}
}

// CREATE INDEX between executes invalidates the cached full-scan plan; the re-plan picks up the new
// secondary index (cheaper cost), proving the invalidation actually forces a fresh plan.
func TestPlanCacheIndexInvalidation(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	for i := 1; i <= 50; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i))
	}
	stmt, err := db.Prepare("SELECT id FROM t WHERE a = $1")
	if err != nil {
		t.Fatal(err)
	}

	rScan, costScan := drainQ(t, stmt, IntValue(25))
	if !planCacheRowsEq(rScan, [][]int64{{25}}) {
		t.Fatalf("full-scan rows = %v", rScan)
	}

	mustExec(t, db, "CREATE INDEX t_a ON t (a)")
	rIdx, costIdx := drainQ(t, stmt, IntValue(25))
	if !planCacheRowsEq(rIdx, [][]int64{{25}}) {
		t.Fatalf("index rows = %v", rIdx)
	}
	if costIdx >= costScan {
		t.Fatalf("expected index lookup cheaper than full scan after CREATE INDEX: idx=%d scan=%d "+
			"(cached full-scan plan served?)", costIdx, costScan)
	}
}

// A plan containing an uncorrelated subquery or a precompiled (constant-pattern) regex is never
// cached — reusing it would bake in one execution's folded params / under-charge the regex compile.
func TestPlanCacheNonCacheable(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, note text)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 'abc'), (2, 'xyz'), (3, 'abd')")

	// Constant-pattern regex → precompiled program → uncacheable. Re-planned each execute, so the
	// regex_compile cost is charged every time and the two executes are cost-identical (this would
	// FAIL if the regex plan were wrongly cached: the 2nd execute would skip the compile charge).
	rx, err := db.Prepare("SELECT id FROM t WHERE note ~ 'ab'")
	if err != nil {
		t.Fatal(err)
	}
	got, c1 := drainQ(t, rx)
	if rx.sc.valid {
		t.Fatal("a precompiled-regex plan must not be cached")
	}
	if !planCacheRowsEq(got, [][]int64{{1}, {3}}) {
		t.Fatalf("regex rows = %v", got)
	}
	_, c2 := drainQ(t, rx)
	if c1 != c2 {
		t.Fatalf("regex cost drift across executes: %d vs %d (regex plan wrongly cached?)", c1, c2)
	}

	// Uncorrelated subquery → uncacheable.
	sq, err := db.Prepare("SELECT id FROM t WHERE id = (SELECT max(id) FROM t)")
	if err != nil {
		t.Fatal(err)
	}
	sr, _ := drainQ(t, sq)
	if sq.sc.valid {
		t.Fatal("a subquery plan must not be cached")
	}
	if !planCacheRowsEq(sr, [][]int64{{3}}) {
		t.Fatalf("subquery rows = %v", sr)
	}
}

// Fill-only-from-committed: a plan first executed INSIDE an open transaction reads the uncommitted
// working set, so it must not be cached (else a rolled-back DDL generation could later alias a
// different committed catalog). After the transaction commits, the autocommit execute reads committed
// and caches normally.
func TestPlanCacheFillOnlyFromCommitted(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10)")
	stmt, err := db.Prepare("SELECT * FROM t WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	if err := db.Begin(true); err != nil {
		t.Fatal(err)
	}
	inTx, _ := drainQ(t, stmt, IntValue(1))
	if !planCacheRowsEq(inTx, [][]int64{{1, 10}}) {
		t.Fatalf("in-tx rows = %v", inTx)
	}
	if stmt.sc.valid {
		t.Fatal("a plan first executed inside a transaction must not be cached (fill-only-from-committed)")
	}
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}

	// Now autocommit (reads committed) fills the cache.
	if _, _ = drainQ(t, stmt, IntValue(1)); !stmt.sc.valid {
		t.Fatal("expected the cache to fill on a committed-state execute after the transaction")
	}
}
