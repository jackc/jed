package jed

// White-box tests for the prepared-statement plan cache (spec/design/api.md §2.4). A prepared
// statement caches its resolved scan plan and reuses it across executes while its exact estimator
// inputs remain unchanged. The behavior is invisible to the conformance corpus (which never reuses a plan
// across statements), and the interesting properties are internal — that a hit
// reuses the exact plan (not re-plans), that reuse is cost-identical, that DDL invalidates, that a
// subquery / precompiled-regex / temp plan is never cached, and that a plan first executed inside a
// transaction is not cached from the uncommitted working set — so they live here, not in the corpus
// (CLAUDE.md §10).

import (
	"context"
	"fmt"
	"reflect"
	"sync"
	"testing"
)

// drainQ runs a prepared query with params on session s (a statement is standalone — the handle
// supplies the session, api.md §2.4), fully drains it, and returns the rows as an int matrix
// (every column read via Value.Int — the tests use integer columns) plus the final accrued cost.
func drainQ(t *testing.T, s *Session, stmt *PreparedStatement, params ...Value) ([][]int64, int64) {
	t.Helper()
	rows, err := s.queryStmt(stmt.ast, params, &stmt.sc)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	defer rows.Close()
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

// cachedExplain renders the exact physical plan held by stmt's cache. Public EXPLAIN performs a
// fresh planning pass, so this white-box helper is what lets P2 compare a refilled cached plan with
// an independently fresh prepared plan rather than merely comparing their executions.
func cachedExplain(t *testing.T, s *Session, stmt *PreparedStatement) [][]Value {
	t.Helper()
	entry := stmt.sc.p.Load()
	if entry == nil {
		t.Fatal("expected cached plan")
	}
	var r explainRender
	if err := s.engine.renderSelectPlan(&r, entry.sp, 0); err != nil {
		t.Fatalf("render cached plan: %v", err)
	}
	return r.rows
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	seedOrders(t, db, 5)
	stmt, err := db.Prepare("SELECT id, amount FROM orders WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	// Execute 1 (MISS): plans and fills the cache.
	r1, cost1 := drainQ(t, db, stmt, IntValue(3))
	entry := stmt.sc.p.Load()
	if entry == nil {
		t.Fatal("expected the cache to fill on the first cacheable execute")
	}
	sp := entry.sp
	if !planCacheRowsEq(r1, [][]int64{{3, 300}}) {
		t.Fatalf("execute 1 rows = %v", r1)
	}

	// Execute 2 (HIT, same param): the exact cached plan is reused (pointer unchanged) and cost is
	// identical to the fresh plan.
	r2, cost2 := drainQ(t, db, stmt, IntValue(3))
	if c := stmt.sc.p.Load(); c == nil || c.sp != sp {
		t.Fatal("plan pointer changed on a cache hit — statement re-planned")
	}
	if cost2 != cost1 {
		t.Fatalf("cache-hit cost = %d, want %d (reuse must be cost-identical)", cost2, cost1)
	}
	if !planCacheRowsEq(r2, [][]int64{{3, 300}}) {
		t.Fatalf("execute 2 rows = %v", r2)
	}

	// Execute 3 (HIT, different param): plan still reused, params still bound per execute.
	r3, _ := drainQ(t, db, stmt, IntValue(5))
	if c := stmt.sc.p.Load(); c == nil || c.sp != sp {
		t.Fatal("plan pointer changed on a param-only change")
	}
	if !planCacheRowsEq(r3, [][]int64{{5, 500}}) {
		t.Fatalf("execute 3 rows = %v", r3)
	}

	// A no-match param still binds correctly against the cached plan.
	r4, _ := drainQ(t, db, stmt, IntValue(999))
	if len(r4) != 0 {
		t.Fatalf("execute 4 (no match) rows = %v", r4)
	}
}

// P2 estimator-input validity is relation-scoped for row statistics: mutating an unrelated table
// keeps the exact cached plan, while mutating a referenced table forces a fresh plan. The refilled
// plan's rows and actual cost must equal an independently fresh prepared statement.
func TestPlanCacheEstimatorRevisionRelevantAndUnrelated(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE a (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "CREATE TABLE b (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO a VALUES (1, 10)")
	mustExec(t, db, "INSERT INTO b VALUES (1, 10)")
	stmt, err := db.Prepare("SELECT id FROM a WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}

	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load() == nil {
		t.Fatal("expected fill")
	}
	first := stmt.sc.p.Load()
	firstPlan := first.sp
	firstRevision := first.inputs[0].revision

	mustExec(t, db, "INSERT INTO b VALUES (2, 20)")
	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load().sp != firstPlan {
		t.Fatal("an unrelated table row-count change invalidated the plan")
	}

	// Return a's count to its prior value before executing again. Revision identity, not count
	// equality, must still force a re-plan because structure/future statistics may differ.
	mustExec(t, db, "INSERT INTO a VALUES (2, 20)")
	mustExec(t, db, "DELETE FROM a WHERE id = 2")
	gotRows, gotCost := drainQ(t, db, stmt, IntValue(10))
	refilled := stmt.sc.p.Load()
	if refilled.sp == firstPlan || refilled.inputs[0].revision == firstRevision {
		t.Fatal("a referenced relation mutation did not invalidate the estimator signature")
	}
	fresh, err := db.Prepare("SELECT id FROM a WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	freshRows, freshCost := drainQ(t, db, fresh, IntValue(10))
	if !planCacheRowsEq(gotRows, freshRows) || gotCost != freshCost {
		t.Fatalf("refilled vs fresh = rows %v/%v cost %d/%d", gotRows, freshRows, gotCost, freshCost)
	}
	if got, want := cachedExplain(t, db, stmt), cachedExplain(t, db, fresh); !reflect.DeepEqual(got, want) {
		t.Fatalf("refilled vs fresh EXPLAIN = %v / %v", got, want)
	}

	// P9 conservatively advances the target revision even for a successful zero-row disposition,
	// retaining its facts as stale.
	beforeNoop := stmt.sc.p.Load().sp
	mustExec(t, db, "INSERT INTO a VALUES (1, 99) ON CONFLICT DO NOTHING")
	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load().sp == beforeNoop {
		t.Fatal("ON CONFLICT DO NOTHING did not conservatively invalidate the target")
	}
	for _, step := range []struct {
		sql   string
		param int64
	}{
		{"UPDATE a SET v = 11 WHERE id = 1", 11},
		{"INSERT INTO a SELECT 2, 20", 11},
		{"INSERT INTO a VALUES (1, 12) ON CONFLICT (id) DO UPDATE SET v = excluded.v", 12},
		{"DELETE FROM a WHERE id = 2", 12},
	} {
		before := stmt.sc.p.Load().sp
		mustExec(t, db, step.sql)
		if _, _ = drainQ(t, db, stmt, IntValue(step.param)); stmt.sc.p.Load().sp == before {
			t.Fatalf("row mutation did not invalidate: %s", step.sql)
		}
	}
}

func TestPlanCacheAnalyzeInvalidatesOnlyRelevantRelation(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE a (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "CREATE INDEX a_v_idx ON a (v)")
	mustExec(t, db, "INSERT INTO a VALUES (1,0),(2,0),(3,0),(4,0),(5,0),(6,0),(7,0),(8,0),(9,1),(10,NULL)")
	mustExec(t, db, "CREATE TABLE b (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO b VALUES (1, 1)")
	stmt, err := db.Prepare("SELECT id FROM a WHERE v = 0")
	if err != nil {
		t.Fatal(err)
	}
	drainQ(t, db, stmt)
	initial := stmt.sc.p.Load().sp

	mustExec(t, db, "ANALYZE b")
	drainQ(t, db, stmt)
	if stmt.sc.p.Load().sp != initial {
		t.Fatal("ANALYZE of an unrelated relation invalidated the plan")
	}

	mustExec(t, db, "ANALYZE a (v)")
	rows, cost := drainQ(t, db, stmt)
	if stmt.sc.p.Load().sp == initial {
		t.Fatal("ANALYZE of the referenced relation did not invalidate the plan")
	}
	fresh, err := db.Prepare("SELECT id FROM a WHERE v = 0")
	if err != nil {
		t.Fatal(err)
	}
	freshRows, freshCost := drainQ(t, db, fresh)
	if !planCacheRowsEq(rows, freshRows) || cost != freshCost {
		t.Fatalf("refilled vs fresh = rows %v/%v cost %d/%d", rows, freshRows, cost, freshCost)
	}
	if got, want := cachedExplain(t, db, stmt), cachedExplain(t, db, fresh); !reflect.DeepEqual(got, want) {
		t.Fatalf("refilled vs fresh EXPLAIN = %v / %v", got, want)
	}
}

// A working transaction receives its own revision token, but cannot overwrite a committed cache
// entry. Rollback restores the committed token, so the original plan becomes a hit again.
func TestPlanCacheEstimatorRevisionRollback(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10)")
	stmt, err := db.Prepare("SELECT id FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load() == nil {
		t.Fatal("expected fill")
	}
	committed := stmt.sc.p.Load()
	if err := db.Begin(true); err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "INSERT INTO t VALUES (2, 10)")
	inTx, _ := drainQ(t, db, stmt, IntValue(10))
	if !planCacheRowsEq(inTx, [][]int64{{1}, {2}}) {
		t.Fatalf("in-tx rows = %v", inTx)
	}
	if stmt.sc.p.Load() != committed {
		t.Fatal("working-transaction statistics populated the committed cache slot")
	}
	if err := db.Rollback(); err != nil {
		t.Fatal(err)
	}
	after, _ := drainQ(t, db, stmt, IntValue(10))
	if !planCacheRowsEq(after, [][]int64{{1}}) || stmt.sc.p.Load() != committed {
		t.Fatalf("rollback did not restore the committed cache hit: rows=%v", after)
	}
}

// An attachment contributes its own identity/generation/revision. Main-database changes do not
// invalidate an attachment-only plan; a row mutation in the referenced attachment does.
func TestPlanCacheAttachmentEstimatorSignature(t *testing.T) {
	t.Parallel()
	base := memDB()
	if err := base.Attach("aux", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	db := base.Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO aux.t VALUES (1, 10)")
	stmt, err := base.Prepare("SELECT id FROM aux.t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load() == nil {
		t.Fatal("expected attachment plan fill")
	}
	firstPlan := stmt.sc.p.Load().sp
	mustExec(t, db, "CREATE TABLE local_only (id i32 PRIMARY KEY)")
	if _, _ = drainQ(t, db, stmt, IntValue(10)); stmt.sc.p.Load().sp != firstPlan {
		t.Fatal("main catalog change invalidated an attachment-only plan")
	}
	mustExec(t, db, "INSERT INTO aux.t VALUES (2, 10)")
	rows, cost := drainQ(t, db, stmt, IntValue(10))
	if stmt.sc.p.Load().sp == firstPlan {
		t.Fatal("attachment row-count change did not invalidate its plan")
	}
	fresh, err := base.Prepare("SELECT id FROM aux.t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	freshRows, freshCost := drainQ(t, db, fresh, IntValue(10))
	if !planCacheRowsEq(rows, freshRows) || cost != freshCost {
		t.Fatalf("attachment refilled vs fresh = rows %v/%v cost %d/%d", rows, freshRows, cost, freshCost)
	}

	// Replacing an attachment with a new database at the same name must not alias even when its
	// catalog generation and table shape happen to match the old database.
	old := stmt.sc.p.Load()
	if err := base.Detach("aux"); err != nil {
		t.Fatal(err)
	}
	if err := base.Attach("aux", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO aux.t VALUES (9, 10), (10, 10)")
	replacedRows, _ := drainQ(t, db, stmt, IntValue(10))
	replaced := stmt.sc.p.Load()
	if replaced.inputs[0].database == old.inputs[0].database || replaced.sp == old.sp {
		t.Fatal("detach/reattach falsely reused the prior attachment identity")
	}
	if !planCacheRowsEq(replacedRows, [][]int64{{9}, {10}}) {
		t.Fatalf("reattached rows = %v", replacedRows)
	}
}

// DROP INDEX bumps the catalog generation on a table that survives, so the next execute re-plans and
// falls back from the (now-gone) index lookup to a full scan — a stale cached index plan would try to
// use a dropped index. Exercises the removeIndex catGen bump.
func TestPlanCacheDropIndexInvalidation(t *testing.T) {
	t.Parallel()
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

	rIdx, costIdx := drainQ(t, db, stmt, IntValue(25)) // index lookup
	if !planCacheRowsEq(rIdx, [][]int64{{25}}) {
		t.Fatalf("index rows = %v", rIdx)
	}
	entry := stmt.sc.p.Load()
	if entry == nil {
		t.Fatal("expected fill")
	}
	gen1 := entry.inputs[0].catGen

	mustExec(t, db, "DROP INDEX t_a")
	rScan, costScan := drainQ(t, db, stmt, IntValue(25)) // re-plan → full scan
	if !planCacheRowsEq(rScan, [][]int64{{25}}) {
		t.Fatalf("rows after DROP INDEX = %v", rScan)
	}
	if costScan <= costIdx {
		t.Fatalf("expected full scan costlier than index after DROP INDEX: scan=%d idx=%d "+
			"(stale index plan served?)", costScan, costIdx)
	}
	if c := stmt.sc.p.Load(); c != nil && c.inputs[0].catGen == gen1 {
		t.Fatal("catGen did not advance after DROP INDEX — plan cache would serve a stale plan")
	}
}

// DROP + re-CREATE with a different shape must re-plan (the rollback-collision guard's positive case:
// under fill-only-from-committed the new committed generation differs).
func TestPlanCacheDropCreateInvalidation(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10)")
	stmt, err := db.Prepare("SELECT * FROM t WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	r1, _ := drainQ(t, db, stmt, IntValue(1))
	if len(r1[0]) != 2 {
		t.Fatalf("before = %v", r1)
	}

	mustExec(t, db, "DROP TABLE t")
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, c i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10, 20)")

	r2, _ := drainQ(t, db, stmt, IntValue(1))
	if len(r2) != 1 || len(r2[0]) != 3 || r2[0][2] != 20 {
		t.Fatalf("after DROP/CREATE = %v, want one 3-column row {1,10,20}", r2)
	}
}

// CREATE INDEX between executes invalidates the cached full-scan plan; the re-plan picks up the new
// secondary index (cheaper cost), proving the invalidation actually forces a fresh plan.
func TestPlanCacheIndexInvalidation(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	for i := 1; i <= 50; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i))
	}
	stmt, err := db.Prepare("SELECT id FROM t WHERE a = $1")
	if err != nil {
		t.Fatal(err)
	}

	rScan, costScan := drainQ(t, db, stmt, IntValue(25))
	if !planCacheRowsEq(rScan, [][]int64{{25}}) {
		t.Fatalf("full-scan rows = %v", rScan)
	}

	mustExec(t, db, "CREATE INDEX t_a ON t (a)")
	rIdx, costIdx := drainQ(t, db, stmt, IntValue(25))
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
	t.Parallel()
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
	got, c1 := drainQ(t, db, rx)
	if rx.sc.p.Load() != nil {
		t.Fatal("a precompiled-regex plan must not be cached")
	}
	if !planCacheRowsEq(got, [][]int64{{1}, {3}}) {
		t.Fatalf("regex rows = %v", got)
	}
	_, c2 := drainQ(t, db, rx)
	if c1 != c2 {
		t.Fatalf("regex cost drift across executes: %d vs %d (regex plan wrongly cached?)", c1, c2)
	}

	// Uncorrelated subquery → uncacheable.
	sq, err := db.Prepare("SELECT id FROM t WHERE id = (SELECT max(id) FROM t)")
	if err != nil {
		t.Fatal(err)
	}
	sr, _ := drainQ(t, db, sq)
	if sq.sc.p.Load() != nil {
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
	t.Parallel()
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
	inTx, _ := drainQ(t, db, stmt, IntValue(1))
	if !planCacheRowsEq(inTx, [][]int64{{1, 10}}) {
		t.Fatalf("in-tx rows = %v", inTx)
	}
	if stmt.sc.p.Load() != nil {
		t.Fatal("a plan first executed inside a transaction must not be cached (fill-only-from-committed)")
	}
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}

	// Now autocommit (reads committed) fills the cache.
	if _, _ = drainQ(t, db, stmt, IntValue(1)); stmt.sc.p.Load() == nil {
		t.Fatal("expected the cache to fill on a committed-state execute after the transaction")
	}
}

// A statement is a standalone value: a plan filled on one session is REUSED (same plan pointer, same
// cost) by a different session over the same Database — the cache is keyed on the shared core's
// committed catalog generation, not on the session that happened to fill it.
func TestPlanCacheSharedAcrossSessions(t *testing.T) {
	base := memDB()
	sA := base.Session(SessionOptions{})
	defer sA.Close()
	seedOrders(t, sA, 5)
	stmt, err := base.Prepare("SELECT id, amount FROM orders WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	rA, costA := drainQ(t, sA, stmt, IntValue(3))
	entry := stmt.sc.p.Load()
	if entry == nil {
		t.Fatal("expected fill on session A")
	}
	if !planCacheRowsEq(rA, [][]int64{{3, 300}}) {
		t.Fatalf("session A rows = %v", rA)
	}

	sB := base.Session(SessionOptions{})
	defer sB.Close()
	rB, costB := drainQ(t, sB, stmt, IntValue(3))
	if c := stmt.sc.p.Load(); c == nil || c.sp != entry.sp {
		t.Fatal("session B re-planned — the cached plan must be shared across sessions of one Database")
	}
	if costB != costA {
		t.Fatalf("cross-session cache-hit cost = %d, want %d (reuse must be cost-identical)", costB, costA)
	}
	if !planCacheRowsEq(rB, [][]int64{{3, 300}}) {
		t.Fatalf("session B rows = %v", rB)
	}
}

// A statement executed against a DIFFERENT Database must not falsely hit: catGen is only monotonic
// within one core, so two databases can share a generation number with different schemas. The entry's
// core identity forces a re-plan against the other database (and the refill re-keys to it).
func TestPlanCacheDistinctDatabasesNoFalseHit(t *testing.T) {
	db1 := memDB().Session(SessionOptions{})
	db2 := memDB().Session(SessionOptions{})
	// One CREATE each → both cores sit at the SAME catalog generation with different table shapes.
	mustExec(t, db1, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	mustExec(t, db2, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
	mustExec(t, db1, "INSERT INTO t VALUES (1, 10)")
	mustExec(t, db2, "INSERT INTO t VALUES (1, 10, 20)")

	stmt, err := db1.Prepare("SELECT * FROM t WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}
	r1, _ := drainQ(t, db1, stmt, IntValue(1))
	if len(r1) != 1 || len(r1[0]) != 2 {
		t.Fatalf("db1 rows = %v, want one 2-column row", r1)
	}
	if stmt.sc.p.Load() == nil {
		t.Fatal("expected fill on db1")
	}

	// Same catGen, different core: a false hit would serve db1's 2-column plan against db2.
	r2, _ := drainQ(t, db2, stmt, IntValue(1))
	if len(r2) != 1 || len(r2[0]) != 3 || r2[0][2] != 20 {
		t.Fatalf("db2 rows = %v, want one 3-column row {1,10,20} (stale cross-database plan served?)", r2)
	}
}

// A plan cached where a relation name is persistent must not be served on a session whose
// session-local temp table shadows that name — the temp domain is invisible to the committed catGen
// the cache is keyed on, so the hit path re-checks the plan's relations against the executing
// session's temp catalog (planTouchesTemp) and re-plans.
func TestPlanCacheTempShadowReplans(t *testing.T) {
	base := memDB()
	// Session B creates its temp table FIRST (a temp name may not shadow an existing persistent
	// table, but a later persistent CREATE in another session cannot see B's temp domain).
	sB := base.Session(SessionOptions{})
	defer sB.Close()
	mustExec(t, sB, "CREATE TEMP TABLE t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, sB, "INSERT INTO t VALUES (1, 111)")

	sA := base.Session(SessionOptions{})
	defer sA.Close()
	mustExec(t, sA, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)")
	mustExec(t, sA, "INSERT INTO t VALUES (1, 10, 20)")

	stmt, err := base.Prepare("SELECT * FROM t WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}
	rA, _ := drainQ(t, sA, stmt, IntValue(1))
	if len(rA) != 1 || len(rA[0]) != 3 {
		t.Fatalf("persistent rows = %v, want one 3-column row", rA)
	}
	if stmt.sc.p.Load() == nil {
		t.Fatal("expected fill on the persistent session")
	}

	// Session B: same core, same catGen — but t resolves temp-first there. The cached persistent
	// plan must not be served; the re-plan reads B's temp table (and is not cached: temp plans never
	// fill).
	rB, _ := drainQ(t, sB, stmt, IntValue(1))
	if len(rB) != 1 || len(rB[0]) != 2 || rB[0][1] != 111 {
		t.Fatalf("temp-shadowed rows = %v, want one 2-column row {1,111} (stale persistent plan served?)", rB)
	}

	// And back on A the persistent plan still serves (B's run did not poison the cache).
	rA2, _ := drainQ(t, sA, stmt, IntValue(1))
	if len(rA2) != 1 || len(rA2[0]) != 3 {
		t.Fatalf("persistent rows after temp run = %v", rA2)
	}
}

// One statement, many goroutines, each on its own session: the atomic cache slot makes concurrent
// fill/hit safe (run under -race in CI), and every execute sees correct rows.
func TestPlanCacheConcurrentSessions(t *testing.T) {
	base := memDB()
	seed := base.Session(SessionOptions{})
	seedOrders(t, seed, 10)
	seed.Close()
	stmt, err := base.Prepare("SELECT id, amount FROM orders WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}

	const goroutines, iters = 8, 100
	errs := make(chan error, goroutines)
	var wg sync.WaitGroup
	for g := 0; g < goroutines; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			s := base.Session(SessionOptions{})
			defer s.Close()
			for i := 0; i < iters; i++ {
				id := int64(i%10) + 1
				var gotID, gotAmount int64
				row := s.QueryRowPrepared(context.Background(), stmt, id)
				if err := row.Scan(&gotID, &gotAmount); err != nil {
					errs <- fmt.Errorf("scan: %w", err)
					return
				}
				if gotID != id || gotAmount != id*100 {
					errs <- fmt.Errorf("row = (%d,%d), want (%d,%d)", gotID, gotAmount, id, id*100)
					return
				}
			}
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
}
