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

func subqueryAB(t *testing.T) *Session {
	return dbWith(
		t,
		"CREATE TABLE a (id i32 PRIMARY KEY, k i32)",
		"CREATE TABLE b (id i32 PRIMARY KEY, k i32)",
		"CREATE TABLE one (id i32 PRIMARY KEY)",
		"INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
		"INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
		"INSERT INTO one VALUES (1)",
	)
}

func errCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := db.Execute(sql, nil)
	if err == nil {
		t.Fatalf("expected error for %q", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected *EngineError for %q, got %T", sql, err)
	}
	return ee.Code()
}

func TestSubqueryCostAddedOnce(t *testing.T) {
	db := subqueryAB(t)
	base, _ := db.Execute("SELECT id FROM a WHERE k = 999", nil)
	withSub, _ := db.Execute("SELECT id FROM a WHERE k = (SELECT max(k) FROM b)", nil)
	// The folded constant is a leaf, so the only delta is the subquery's own cost (1 page_read +
	// 3 scan + 3 accumulate + 1 produced = 8), added exactly once.
	if d := withSub.Cost - base.Cost; d != 8 {
		t.Errorf("subquery cost delta got %d want 8", d)
	}
}

// ---- a correlated subquery's structural error is raised at plan time (kept per review) ------

func TestCorrelatedInnerErrorOverEmptyOuter(t *testing.T) {
	// The subquery is PLANNED once, so a structural error (here >1 column) is raised even when the
	// outer query is empty and the subquery never executes (PostgreSQL parity). The corpus pins the
	// same guarantee via an empty inner filter, not an empty outer — so this trigger shape is kept.
	db := dbWith(
		t,
		"CREATE TABLE e (id i32 PRIMARY KEY, v i32)",
		"CREATE TABLE f (id i32 PRIMARY KEY, v i32)",
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

func TestDeleteCorrelatedSubqueryCostIsPerRow(t *testing.T) {
	// A correlated DELETE subquery re-runs per scanned row; an uncorrelated one folds once. The
	// correlated cost therefore exceeds the uncorrelated baseline on the same data — proving the
	// per-row execution. Both are deterministic + cross-core identical (CLAUDE.md §13).
	corr, _ := subqueryAB(t).Execute("DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)", nil)
	uncorr, _ := subqueryAB(t).Execute("DELETE FROM a WHERE k IN (SELECT k FROM b)", nil)
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
