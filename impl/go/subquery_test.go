package jed

// Subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS (SELECT …)`, both
// uncorrelated and CORRELATED. These complement the conformance corpus
// (spec/conformance/suites/subquery) with finer-grained per-feature assertions: the uncorrelated
// fold (execute once → constant, cost added once), the typed NULL of an empty scalar, three-valued
// IN, EXISTS ignoring the select list; and for correlated subqueries the scope-chain resolution,
// per-outer-row execution + cost, correlation in a JOIN ON and inside an aggregate argument,
// multi-level + skip-level (grandparent) correlation, and the error / narrowing codes
// (21000 / 42601 / 0A000). See spec/design/grammar.md §26.

import "testing"

func subqueryAB(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE one (id int32 PRIMARY KEY)",
		"INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
		"INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
		"INSERT INTO one VALUES (1)",
	)
}

func errCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("expected error for %q", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected *EngineError for %q, got %T", sql, err)
	}
	return ee.Code()
}

func TestSubqueryScalarInWhereAndSelectList(t *testing.T) {
	db := subqueryAB(t)
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM a) ORDER BY id"); !eqInts(got, 3) {
		t.Errorf("scalar in WHERE got %v", got)
	}
	if got := queryIDs(t, db, "SELECT (SELECT count(*) FROM b) FROM a ORDER BY id"); !eqInts(got, 3, 3, 3) {
		t.Errorf("scalar in select list got %v", got)
	}
}

func TestSubqueryScalarNestedAndInExpression(t *testing.T) {
	db := subqueryAB(t)
	if got := queryIDs(t, db, "SELECT (SELECT (SELECT max(k) FROM b) FROM one) FROM one"); !eqInts(got, 40) {
		t.Errorf("nested scalar got %v", got)
	}
	if got := queryIDs(t, db, "SELECT k + (SELECT max(k) FROM b) FROM a ORDER BY id"); !eqInts(got, 50, 60, 70) {
		t.Errorf("scalar in expression got %v", got)
	}
}

func TestSubqueryScalarEmptyIsNull(t *testing.T) {
	db := subqueryAB(t)
	rows := query(t, db, "SELECT (SELECT k FROM b WHERE id = 99) FROM one")
	if len(rows) != 1 || rows[0][0].Kind != ValNull {
		t.Errorf("empty scalar should project NULL, got %+v", rows)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k = (SELECT k FROM b WHERE id = 99) ORDER BY id"); len(got) != 0 {
		t.Errorf("k = NULL should keep no rows, got %v", got)
	}
}

func TestSubqueryInAndNotIn(t *testing.T) {
	db := subqueryAB(t)
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k IN (SELECT k FROM b) ORDER BY id"); !eqInts(got, 2, 3) {
		t.Errorf("IN got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b) ORDER BY id"); !eqInts(got, 1) {
		t.Errorf("NOT IN got %v", got)
	}
}

func TestSubqueryInEmptyResult(t *testing.T) {
	db := subqueryAB(t)
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k IN (SELECT k FROM b WHERE id = 99) ORDER BY id"); len(got) != 0 {
		t.Errorf("IN empty should keep no rows, got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b WHERE id = 99) ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("NOT IN empty should keep all rows, got %v", got)
	}
}

func TestSubqueryInWithNullThreeValued(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE vals (id int32 PRIMARY KEY, v int32)",
		"INSERT INTO s VALUES (1, 5), (2, 10)",
		"INSERT INTO vals VALUES (1, 10), (2, NULL)",
	)
	// 10 matches -> TRUE (id 2). 5 matches nothing but the NULL makes it UNKNOWN -> dropped.
	if got := queryIDs(t, db, "SELECT id FROM s WHERE k IN (SELECT v FROM vals) ORDER BY id"); !eqInts(got, 2) {
		t.Errorf("IN with NULL got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM s WHERE k NOT IN (SELECT v FROM vals) ORDER BY id"); len(got) != 0 {
		t.Errorf("NOT IN with NULL should keep no rows, got %v", got)
	}
}

func TestSubqueryExists(t *testing.T) {
	db := subqueryAB(t)
	if got := queryIDs(t, db, "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b) ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("EXISTS got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE id = 99) ORDER BY id"); len(got) != 0 {
		t.Errorf("EXISTS empty should keep no rows, got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE id = 99) ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("NOT EXISTS empty should keep all rows, got %v", got)
	}
	// EXISTS ignores the select list (multi-column / star are legal).
	if got := queryIDs(t, db, "SELECT id FROM a WHERE EXISTS (SELECT 1, 2, 3 FROM b) ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("EXISTS multi-col got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM a WHERE EXISTS (SELECT * FROM b) ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("EXISTS star got %v", got)
	}
}

func TestSubqueryCostAddedOnce(t *testing.T) {
	db := subqueryAB(t)
	base, _ := Execute(db, "SELECT id FROM a WHERE k = 999")
	withSub, _ := Execute(db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)")
	// The folded constant is a leaf, so the only delta is the subquery's own cost (1 page_read +
	// 3 scan + 3 accumulate + 1 produced = 8), added exactly once.
	if d := withSub.Cost - base.Cost; d != 8 {
		t.Errorf("subquery cost delta got %d want 8", d)
	}
}

func TestSubqueryErrorCodes(t *testing.T) {
	db := subqueryAB(t)
	cases := []struct {
		sql, code string
	}{
		{"SELECT (SELECT k FROM b) FROM one", "21000"},
		{"SELECT (SELECT id, k FROM b WHERE id = 1) FROM one", "42601"},
		{"SELECT id FROM a WHERE k IN (SELECT id, k FROM b)", "42601"},
		// the >1-column check is plan-time, so it fires even over an empty subquery result
		{"SELECT (SELECT id, k FROM b WHERE id = 99) FROM one", "42601"},
		// A $N inside a subquery is now allowed (see the params tests below); a $N with NO type
		// context anywhere (here a bare select-list $1) stays uninferable -> 42P18 (PG instead
		// defaults it to text, then `int = text` errors — a documented divergence, §26).
		{"SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", "42P18"},
		// grouping / ordering a subquery BY an enclosing-query column -> 0A000 (degenerate)
		{"SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b GROUP BY a.k)", "0A000"},
		{"SELECT id FROM a WHERE EXISTS (SELECT k FROM b ORDER BY a.k)", "0A000"},
	}
	for _, c := range cases {
		if got := errCode(t, db, c.sql); got != c.code {
			t.Errorf("%q: got %s want %s", c.sql, got, c.code)
		}
	}
}

// t123DB is the 3-table fixture for the correlated-subquery tests (matches correlated.test).
func t123DB(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE t1 (id int32 PRIMARY KEY, v int32)",
		"CREATE TABLE t2 (id int32 PRIMARY KEY, v int32)",
		"CREATE TABLE t3 (id int32 PRIMARY KEY, v int32)",
		"INSERT INTO t1 VALUES (1, 10), (2, 20)",
		"INSERT INTO t2 VALUES (1, 10), (2, 30)",
		"INSERT INTO t3 VALUES (1, 10), (2, 20)",
	)
}

func TestCorrelatedExists(t *testing.T) {
	db := t123DB(t)
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"); !eqInts(got, 1) {
		t.Errorf("correlated EXISTS got %v", got)
	}
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"); !eqInts(got, 2) {
		t.Errorf("correlated NOT EXISTS got %v", got)
	}
}

func TestCorrelatedScalarAndEmptyIsNull(t *testing.T) {
	db := t123DB(t)
	// count over a correlated WHERE: (1,1),(2,1).
	rows := query(t, db, "SELECT t1.id, (SELECT count(*) FROM t2 WHERE t2.v > t1.v) FROM t1 ORDER BY t1.id")
	if len(rows) != 2 || rows[0][1].Int != 1 || rows[1][1].Int != 1 {
		t.Errorf("correlated scalar count got %v", rows)
	}
	// a 0-row correlated scalar is NULL, evaluated per outer row.
	rows = query(t, db, "SELECT t1.id, (SELECT t2.v FROM t2 WHERE t2.v = t1.v * 100) FROM t1 ORDER BY t1.id")
	if len(rows) != 2 || rows[0][1].Kind != ValNull || rows[1][1].Kind != ValNull {
		t.Errorf("empty correlated scalar should be NULL, got %v", rows)
	}
}

func TestCorrelatedIn(t *testing.T) {
	db := t123DB(t)
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE t1.v IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"); !eqInts(got, 1) {
		t.Errorf("correlated IN got %v", got)
	}
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE t1.v NOT IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"); !eqInts(got, 2) {
		t.Errorf("correlated NOT IN got %v", got)
	}
}

func TestCorrelatedInJoinOn(t *testing.T) {
	db := t123DB(t)
	// the inner self-join's ON predicate references the OUTER t1 (correlation in a JOIN ON).
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 JOIN t2 AS t2b ON t2b.v = t1.v WHERE t2.id = t1.id) ORDER BY t1.id"); !eqInts(got, 1) {
		t.Errorf("correlation in JOIN ON got %v", got)
	}
}

func TestCorrelatedMultiLevelAndSkipLevel(t *testing.T) {
	db := t123DB(t)
	// two-level nesting, each level correlating to its IMMEDIATE parent.
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v AND EXISTS (SELECT 1 FROM t3 WHERE t3.v = t2.v)) ORDER BY t1.id"); !eqInts(got, 1) {
		t.Errorf("two-level correlation got %v", got)
	}
	// skip-level: the innermost references the GRANDPARENT t1, skipping t2.
	if got := queryIDs(t, db, "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE EXISTS (SELECT 1 FROM t3 WHERE t3.v = t1.v)) ORDER BY t1.id"); !eqInts(got, 1, 2) {
		t.Errorf("skip-level correlation got %v", got)
	}
}

func TestCorrelatedOuterRefInAggregateArg(t *testing.T) {
	db := t123DB(t)
	// sum(t2.v + t1.v) over t2 for each t1 row -> (10+10)+(30+10)=60 ; (10+20)+(30+20)=80.
	rows := query(t, db, "SELECT t1.id, (SELECT sum(t2.v + t1.v) FROM t2) FROM t1 ORDER BY t1.id")
	if len(rows) != 2 || rows[0][1].Int != 60 || rows[1][1].Int != 80 {
		t.Errorf("outer ref in aggregate arg got %v", rows)
	}
}

func TestCorrelatedSubqueryCostIsPerOuterRow(t *testing.T) {
	db := t123DB(t)
	// A correlated subquery re-runs once per outer row (unlike the uncorrelated fold-once), and
	// each re-scan of the inner table charges its page_read too. The derivation is in
	// spec/conformance/suites/subquery/correlated.test (cost = 17).
	out, _ := Execute(db, "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v)")
	if out.Cost != 17 {
		t.Errorf("correlated subquery cost got %d want 17", out.Cost)
	}
}

func TestCorrelatedInnerErrorOverEmptyOuter(t *testing.T) {
	// The subquery is PLANNED once, so a structural error (here >1 column) is raised even when the
	// outer query is empty and the subquery never executes (PostgreSQL parity).
	db := dbWith(
		t,
		"CREATE TABLE e (id int32 PRIMARY KEY, v int32)",
		"CREATE TABLE f (id int32 PRIMARY KEY, v int32)",
		"INSERT INTO f VALUES (1, 1)",
	)
	if got := errCode(t, db, "SELECT (SELECT id, v FROM f WHERE v = e.v) FROM e"); got != "42601" {
		t.Errorf("inner error over empty outer got %s want 42601", got)
	}
}

// ---- subqueries in UPDATE / DELETE (spec/design/grammar.md §26) -----------------------------
// A subquery is legal in a DELETE/UPDATE WHERE and an UPDATE assignment RHS. An uncorrelated one
// folds once (cost added once); a correlated one references the TARGET row via the per-row outer
// environment and re-runs per matching row. The mutation stays two-phase / all-or-nothing: the
// subquery reads the pre-statement snapshot (DELETE collects keys first; UPDATE writes in phase 2).

func TestDeleteWhereUncorrelatedInSubquery(t *testing.T) {
	db := subqueryAB(t)
	// delete a's rows whose k is one of b's k values {20,30,40}: ids 2 (20) and 3 (30) go.
	mustExec(t, db, "DELETE FROM a WHERE k IN (SELECT k FROM b)")
	if got := queryIDs(t, db, "SELECT id FROM a ORDER BY id"); !eqInts(got, 1) {
		t.Errorf("after DELETE got %v want [1]", got)
	}
}

func TestDeleteWhereCorrelatedExistsSubquery(t *testing.T) {
	db := subqueryAB(t)
	// EXISTS a b row whose k equals THIS a row's k: a.k ∈ {10,20,30}, b.k ∈ {20,30,40} -> 20,30 match.
	mustExec(t, db, "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)")
	if got := queryIDs(t, db, "SELECT id FROM a ORDER BY id"); !eqInts(got, 1) {
		t.Errorf("correlated EXISTS DELETE got %v want [1]", got)
	}
	// NOT EXISTS is the complement.
	db = subqueryAB(t)
	mustExec(t, db, "DELETE FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)")
	if got := queryIDs(t, db, "SELECT id FROM a ORDER BY id"); !eqInts(got, 2, 3) {
		t.Errorf("correlated NOT EXISTS DELETE got %v want [2 3]", got)
	}
}

func TestUpdateSetCorrelatedScalarSubquery(t *testing.T) {
	db := subqueryAB(t)
	// each a.k becomes max(b.k) over b rows with b.k > the OLD a.k: 10->40, 20->40, 30->40.
	mustExec(t, db, "UPDATE a SET k = (SELECT max(b.k) FROM b WHERE b.k > a.k)")
	if got := queryIDs(t, db, "SELECT k FROM a ORDER BY id"); !eqInts(got, 40, 40, 40) {
		t.Errorf("correlated scalar UPDATE got %v want [40 40 40]", got)
	}
}

func TestUpdateSetCorrelatedScalarEmptyIsNull(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
		"INSERT INTO a VALUES (1, 5), (2, 100)",
		"INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
	)
	// id1 (k=5): max(b.k>5)=40 ; id2 (k=100): no b.k>100 -> empty scalar -> NULL.
	mustExec(t, db, "UPDATE a SET k = (SELECT max(b.k) FROM b WHERE b.k > a.k)")
	rows := query(t, db, "SELECT id, k FROM a ORDER BY id")
	if len(rows) != 2 || rows[0][1].Int != 40 || rows[1][1].Kind != ValNull {
		t.Errorf("empty scalar UPDATE should NULL the unmatched row, got %+v", rows)
	}
}

func TestUpdateWhereCorrelatedWithUncorrelatedSet(t *testing.T) {
	db := subqueryAB(t)
	// WHERE: a.k + 10 is one of b's k {20,30,40} -> all three rows. SET: uncorrelated min(b.k)=20,
	// folded once. So every row -> 20.
	mustExec(t, db, "UPDATE a SET k = (SELECT min(k) FROM b) WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k + 10)")
	if got := queryIDs(t, db, "SELECT k FROM a ORDER BY id"); !eqInts(got, 20, 20, 20) {
		t.Errorf("mixed UPDATE got %v want [20 20 20]", got)
	}
}

func TestDeleteCorrelatedSubqueryCostIsPerRow(t *testing.T) {
	// A correlated DELETE subquery re-runs per scanned row; an uncorrelated one folds once. The
	// correlated cost therefore exceeds the uncorrelated baseline on the same data — proving the
	// per-row execution. Both are deterministic + cross-core identical (CLAUDE.md §13).
	corr, _ := Execute(subqueryAB(t), "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)")
	uncorr, _ := Execute(subqueryAB(t), "DELETE FROM a WHERE k IN (SELECT k FROM b)")
	if corr.Cost <= uncorr.Cost {
		t.Errorf("correlated cost %d should exceed uncorrelated %d", corr.Cost, uncorr.Cost)
	}
}

// ---- bind parameters inside a subquery (spec/design/grammar.md §26) -------------------------
// A $N inside a subquery is allowed once it gets a type from an INNER context; inference is
// statement-wide (one paramTypes threaded through the whole plan tree), so the same $N may be used
// inside and outside, and a correlated subquery may compare a $N against the outer row.

func TestParamInsideSubqueryInnerContext(t *testing.T) {
	db := subqueryAB(t)
	// $1 typed by `b.k = $1` (inner) AND correlated to the outer a.k: survive iff some b.k equals
	// both $1 and a.k. a.k ∈ {10,20,30}, b.k ∈ {20,30,40}; with $1=20 only a.id=2 survives.
	if got := firstInts(queryRows(t, db, "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = $1 AND b.k = a.k) ORDER BY id", IntValue(20))); !eqInts(got, 2) {
		t.Errorf("inner EXISTS param got %v want [2]", got)
	}
	// $1 typed by `b.id = $1` inside an IN subquery (b.id=1 -> b.k=20 -> a.id=2).
	if got := firstInts(queryRows(t, db, "SELECT id FROM a WHERE k IN (SELECT b.k FROM b WHERE b.id = $1) ORDER BY id", IntValue(1))); !eqInts(got, 2) {
		t.Errorf("inner IN param got %v want [2]", got)
	}
	// The same $1 used OUTSIDE and INSIDE — one statement-wide inference.
	if got := firstInts(queryRows(t, db, "SELECT id FROM a WHERE k > $1 AND EXISTS (SELECT 1 FROM b WHERE b.k = $1 + 10) ORDER BY id", IntValue(10))); !eqInts(got, 2, 3) {
		t.Errorf("shared param got %v want [2 3]", got)
	}
}

func TestParamInsideSubqueryUninferableIs42P18(t *testing.T) {
	// A $N whose only position is a context-free select-list slot can't be typed -> 42P18, even
	// with a value bound (the type, not the value, is missing). PG diverges (defaults to text).
	db := subqueryAB(t)
	if c := paramErrCode(t, db, "SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", IntValue(10)); c != "42P18" {
		t.Errorf("uninferable subquery param got %s want 42P18", c)
	}
}
