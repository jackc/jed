package jed

// Stage-2 WHERE pushdown with BOUND PARAMETERS (spec/design/planner.md §3.2). The conformance corpus
// pins pushdown rows/costs/EXPLAIN with literals (joins/where_pushdown.test,
// query/where_rewrite_explain.test) but cannot bind `$N`; this checks the host-API surface it cannot:
// a parameter operand is pushdown-safe, the pushed filter reads the bound value inside the scan, the
// cost equals the same query spelled with the literal, and a prepared statement's cached plan reuses
// the pushdown across executions with different values.

import "testing"

func TestWherePushdownBoundParameters(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32)",
		"CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32)",
		"INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40)",
		"INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1)",
	)
	const param = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= $1 AND b.w > $2 ORDER BY a.id, b.id"
	literal := func(v, w int64) string {
		return "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= " + IntValue(v).Render() +
			" AND b.w > " + IntValue(w).Render() + " ORDER BY a.id, b.id"
	}

	stmt, err := db.Prepare(param)
	if err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		v, w int64
		want [][2]int64
	}{
		{10, 4, [][2]int64{{1, 11}, {2, 13}}},
		{20, 0, [][2]int64{{2, 13}}},
		{50, 0, nil},
	} {
		params := []Value{IntValue(tc.v), IntValue(tc.w)}
		for _, run := range []func() (outcome, error){
			func() (outcome, error) { return queryOutcome(db, param, params) },
			func() (outcome, error) { return prepOutcome(db, stmt, params) },
		} {
			out, err := run()
			if err != nil {
				t.Fatal(err)
			}
			if len(out.Rows) != len(tc.want) {
				t.Fatalf("v=%d w=%d: rows = %v, want %v", tc.v, tc.w, out.Rows, tc.want)
			}
			for i, row := range out.Rows {
				if row[0].Int != tc.want[i][0] || row[1].Int != tc.want[i][1] {
					t.Fatalf("v=%d w=%d: rows = %v, want %v", tc.v, tc.w, out.Rows, tc.want)
				}
			}
			lit := mustCost(t, db, literal(tc.v, tc.w))
			if out.Cost != lit {
				t.Errorf("v=%d w=%d: parameter cost %d, literal cost %d (a pushed $N must charge like a literal)",
					tc.v, tc.w, out.Cost, lit)
			}
		}
	}
}

// A bound parameter in a conjunct moved into a derived body (planner.md §3.2) or pushed from an ON
// (§3.3) reads the bound value where it now runs, charges exactly like the literal spelling, and a
// prepared statement's cached plan keeps the rewrite (the body was planned again once, before bind).
func TestDerivedAndOnPushdownBoundParameters(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32)",
		"CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32)",
		"INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40)",
		"INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1)",
	)
	for _, tc := range []struct {
		param, literal string
		value          int64
		want           []int64
	}{
		{
			param:   "SELECT d.id FROM (SELECT id, v FROM a) d WHERE d.id = $1",
			literal: "SELECT d.id FROM (SELECT id, v FROM a) d WHERE d.id = 2",
			value:   2, want: []int64{2},
		},
		{
			param:   "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k AND b.w > $1 WHERE b.id IS NOT NULL ORDER BY a.id",
			literal: "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k AND b.w > 4 WHERE b.id IS NOT NULL ORDER BY a.id",
			value:   4, want: []int64{1, 2},
		},
	} {
		stmt, err := db.Prepare(tc.param)
		if err != nil {
			t.Fatal(err)
		}
		lit := mustCost(t, db, tc.literal)
		for _, run := range []func() (outcome, error){
			func() (outcome, error) { return queryOutcome(db, tc.param, []Value{IntValue(tc.value)}) },
			func() (outcome, error) { return prepOutcome(db, stmt, []Value{IntValue(tc.value)}) },
			func() (outcome, error) { return prepOutcome(db, stmt, []Value{IntValue(tc.value)}) },
		} {
			out, err := run()
			if err != nil {
				t.Fatal(err)
			}
			if len(out.Rows) != len(tc.want) {
				t.Fatalf("%s: rows = %v, want %v", tc.param, out.Rows, tc.want)
			}
			for i, row := range out.Rows {
				if row[0].Int != tc.want[i] {
					t.Fatalf("%s: rows = %v, want %v", tc.param, out.Rows, tc.want)
				}
			}
			if out.Cost != lit {
				t.Errorf("%s: parameter cost %d, literal cost %d", tc.param, out.Cost, lit)
			}
		}
	}
}
