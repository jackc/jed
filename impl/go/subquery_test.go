package jed

// Uncorrelated subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS
// (SELECT …)`. These complement the conformance corpus (spec/conformance/suites/subquery) with
// finer-grained per-feature assertions: plan-time folding (execute once → constant), the typed
// NULL of an empty scalar, three-valued IN, EXISTS ignoring the select list, the cost contract
// (subquery cost added once, the fold is a leaf), and the error / narrowing codes (21000 / 42601 /
// 0A000). See spec/design/grammar.md §26.

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
	// The folded constant is a leaf, so the only delta is the subquery's own cost (3 scan + 3
	// accumulate + 1 produced = 7), added exactly once.
	if d := withSub.Cost - base.Cost; d != 7 {
		t.Errorf("subquery cost delta got %d want 7", d)
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
		{"SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE k = a.k)", "0A000"},
		{"SELECT (SELECT max(k) FROM b WHERE b.id = a.id) FROM a", "0A000"},
		{"SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", "0A000"},
		{"DELETE FROM a WHERE k IN (SELECT k FROM b)", "0A000"},
	}
	for _, c := range cases {
		if got := errCode(t, db, c.sql); got != c.code {
			t.Errorf("%q: got %s want %s", c.sql, got, c.code)
		}
	}
}
