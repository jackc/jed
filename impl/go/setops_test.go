package jed

// Set operations — UNION/INTERSECT/EXCEPT (each [ALL]). The per-feature row/multiset/precedence/
// unification/cost/error assertions live in the shared conformance corpus
// (spec/conformance/suites/setops/*.test). See spec/design/grammar.md §25.

import "testing"

func setopAB(t *testing.T) *Engine {
	return dbWith(
		t,
		"CREATE TABLE a (id i32 PRIMARY KEY, k i32)",
		"CREATE TABLE b (id i32 PRIMARY KEY, k i32)",
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

// The per-feature row/multiset/precedence/unification/error assertions that used to live here
// are covered by the shared conformance corpus (spec/conformance/suites/setops/*.test), which
// runs on all three cores. TestSetOpDispatchReturnsQuery is retained: it asserts the internal
// Outcome.Kind discriminant, which the SQL-in->rows corpus cannot observe.
