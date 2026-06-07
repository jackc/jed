package jed

// Set operations — UNION/INTERSECT/EXCEPT (each [ALL]). These complement the conformance corpus
// (spec/conformance/suites/setops) with finer-grained per-feature assertions: PG precedence,
// multiset multiplicities, integer<->decimal unification, the lhs+rhs cost contract, and the
// error codes. See spec/design/grammar.md §25.

import "testing"

func setopAB(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
		"INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
		"INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
	)
}

func TestSetOpDispatchReturnsQuery(t *testing.T) {
	db := setopAB(t)
	out, err := Execute(db, "SELECT k FROM a UNION SELECT k FROM b")
	if err != nil {
		t.Fatal(err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("expected query outcome, got %v", out.Kind)
	}
}

func TestUnionDistinctAndAll(t *testing.T) {
	db := setopAB(t)
	if got := queryIDs(t, db, "SELECT k FROM a UNION SELECT k FROM b ORDER BY k"); !eqInts(got, 10, 20, 30, 40) {
		t.Errorf("UNION got %v", got)
	}
	if got := queryIDs(t, db, "SELECT k FROM a UNION ALL SELECT k FROM b ORDER BY k"); !eqInts(got, 10, 20, 20, 30, 30, 40) {
		t.Errorf("UNION ALL got %v", got)
	}
}

func TestSetOpCostIsSumOfOperands(t *testing.T) {
	db := setopAB(t)
	// (1 page_read + 3 scan + 3 produce) per operand = 7 + 7; dedup unmetered.
	out, _ := Execute(db, "SELECT k FROM a UNION SELECT k FROM b")
	if out.Cost != 14 {
		t.Errorf("UNION cost got %d want 14", out.Cost)
	}
	// LIMIT does not lower the cost: operands fully produce, the window is unmetered.
	out2, _ := Execute(db, "SELECT k FROM a UNION SELECT k FROM b ORDER BY k LIMIT 1")
	if out2.Cost != 14 {
		t.Errorf("UNION+LIMIT cost got %d want 14", out2.Cost)
	}
}

func TestIntersectAndExceptMultiset(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE l (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE r (id int32 PRIMARY KEY, k int32)",
		"INSERT INTO l VALUES (1,1),(2,1),(3,1),(4,2),(5,3)", // k: 1->3, 2->1, 3->1
		"INSERT INTO r VALUES (1,1),(2,2)",                   // k: 1->1, 2->1
	)
	// INTERSECT ALL = min(m,n): 1->min(3,1)=1, 2->min(1,1)=1, 3->0 => {1,2}.
	if got := queryIDs(t, db, "SELECT k FROM l INTERSECT ALL SELECT k FROM r ORDER BY k"); !eqInts(got, 1, 2) {
		t.Errorf("INTERSECT ALL got %v", got)
	}
	// INTERSECT (distinct) = keys in both => {1,2}.
	if got := queryIDs(t, db, "SELECT k FROM l INTERSECT SELECT k FROM r ORDER BY k"); !eqInts(got, 1, 2) {
		t.Errorf("INTERSECT got %v", got)
	}
	// EXCEPT ALL = max(0,m-n): 1->2, 2->0, 3->1 => {1,1,3}.
	if got := queryIDs(t, db, "SELECT k FROM l EXCEPT ALL SELECT k FROM r ORDER BY k"); !eqInts(got, 1, 1, 3) {
		t.Errorf("EXCEPT ALL got %v", got)
	}
	// EXCEPT (distinct) = left keys absent from right => {3}.
	if got := queryIDs(t, db, "SELECT k FROM l EXCEPT SELECT k FROM r ORDER BY k"); !eqInts(got, 3) {
		t.Errorf("EXCEPT got %v", got)
	}
}

func TestSetOpPrecedenceIntersectBindsTighter(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE p (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE q (id int32 PRIMARY KEY, k int32)",
		"CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
		"INSERT INTO p VALUES (1, 1)",
		"INSERT INTO q VALUES (1, 2), (2, 3)",
		"INSERT INTO s VALUES (1, 3), (2, 4)",
	)
	// p UNION q INTERSECT s = p UNION (q INTERSECT s) = {1} UNION {3} = {1,3}.
	// (Left-assoc would give ({1,2,3} INTERSECT {3,4}) = {3}.)
	if got := queryIDs(t, db, "SELECT k FROM p UNION SELECT k FROM q INTERSECT SELECT k FROM s ORDER BY k"); !eqInts(got, 1, 3) {
		t.Errorf("precedence got %v want [1 3]", got)
	}
}

func TestSetOpIntDecimalUnification(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE ai (id int32 PRIMARY KEY, n int32)",
		"CREATE TABLE ad (id int32 PRIMARY KEY, n decimal(10,2))",
		"INSERT INTO ai VALUES (1, 5), (2, 7)",
		"INSERT INTO ad VALUES (1, 5.0), (2, 9.50)",
	)
	// 5 (int, converted) == 5.00 (decimal) -> matched. Distinct set {5, 7, 9.50}: 3 rows,
	// the column is decimal-typed.
	rows := query(t, db, "SELECT n FROM ai UNION SELECT n FROM ad")
	if len(rows) != 3 {
		t.Fatalf("int<->decimal UNION got %d rows want 3: %+v", len(rows), rows)
	}
	for _, r := range rows {
		if r[0].Kind != ValDecimal {
			t.Errorf("expected decimal-typed value, got kind %v", r[0].Kind)
		}
	}
}

func TestSetOpErrors(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE x (id int32 PRIMARY KEY, a int32, b int32)",
		"CREATE TABLE y (id int32 PRIMARY KEY, a int32, t text)",
		"INSERT INTO x VALUES (1, 10, 20)",
		"INSERT INTO y VALUES (1, 30, 'hi')",
	)
	cases := []struct {
		sql  string
		code string
	}{
		{"SELECT a, b FROM x UNION SELECT a FROM y", "42601"},            // arity
		{"SELECT a FROM x UNION SELECT t FROM y", "42804"},               // type mismatch
		{"SELECT a FROM x ORDER BY a UNION SELECT a FROM y", "42601"},    // operand ORDER BY -> leftover
		{"SELECT a FROM x UNION SELECT a FROM y ORDER BY x.a", "42P01"},  // qualified key
		{"SELECT a FROM x UNION SELECT a FROM y ORDER BY nope", "42703"}, // unknown name
	}
	for _, c := range cases {
		_, err := Execute(db, c.sql)
		ee, ok := err.(*EngineError)
		if !ok || ee.Code() != c.code {
			t.Errorf("%q: got %v, want %s", c.sql, err, c.code)
		}
	}
}
