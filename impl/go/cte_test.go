package jed

// Common table expressions — WITH [RECURSIVE] name [(cols)] AS [NOT] MATERIALIZED (query) [, …]
// <query> (spec/design/cte.md, spec/design/recursive-cte.md). The row/name/error assertions and the
// inline/materialize cost contract live in the shared conformance corpus
// (spec/conformance/suites/cte/*.test). What remains here is what the corpus cannot express: the
// MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), and — for WITH RECURSIVE — the
// cost-ceiling termination of a non-terminating recursion (54P01) and the inert materialization hint.

import "testing"

// cteT3 is a 3-row, single-node table t(id, n) = {(1,10),(2,20),(3,30)}.
func cteT3(t *testing.T) *Session {
	return dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, n i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
	)
}

// cteCost runs sql and returns its accrued cost.
func cteCost(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Cost
}

func eqStrs(a []string, b ...string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func TestCteMaterializedHintForcesBuffering(t *testing.T) {
	db := cteT3(t)
	// MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
	// row_produced = 13 (vs the inlined 10).
	if got := cteCost(t, db,
		"WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"); got != 13 {
		t.Errorf("MATERIALIZED cost got %d want 13", got)
	}
	// NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
	// bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
	if got := cteCost(t, db,
		"WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"); got != 23 {
		t.Errorf("NOT MATERIALIZED cost got %d want 23", got)
	}
}

// A non-terminating recursion (UNION ALL with no stopping predicate) is bounded by the cost ceiling.
// Each iteration is cheap (a 1-row working table), so this trips 54P01 ONLY through the CONTINUOUS
// cross-iteration meter (recursive-cte.md §5) — the untrusted-query safety mechanism doing real
// work. A per-iteration meter would never fire here, so the corpus cannot express it.
func TestCteRecursiveUnboundedAbortsAtCostCeiling(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	db.SetMaxCost(1000)
	_, err := db.Execute("WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT n FROM c", nil)
	if err == nil {
		t.Fatal("an unbounded recursion must abort, not loop forever")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("expected 54P01, got %v", err)
	}
}

// A recursion whose total cost fits under the ceiling runs to completion (the ceiling bounds the
// actual accrued cost, not a per-iteration figure); the 5-row counter accrues 29 (the corpus cost
// contract).
func TestCteRecursiveUnderCeilingSucceeds(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	db.SetMaxCost(1000)
	if got := cteCost(t, db,
		"WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT n FROM c"); got != 29 {
		t.Errorf("recursive cost got %d want 29", got)
	}
}

// A recursive CTE is ALWAYS materialized — NOT MATERIALIZED is inert (recursive-cte.md §1), so a
// single-reference recursive CTE still iterates to a fixpoint (3 rows, cost 17) rather than inlining.
func TestCteRecursiveHintIsInert(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	for _, hint := range []string{"", "MATERIALIZED ", "NOT MATERIALIZED "} {
		sql := "WITH RECURSIVE c(n) AS " + hint +
			"(SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT n FROM c ORDER BY n"
		out, err := db.Execute(sql, nil)
		if err != nil {
			t.Fatalf("hint %q: %v", hint, err)
		}
		if len(out.Rows) != 3 {
			t.Errorf("hint %q: got %d rows want 3", hint, len(out.Rows))
		}
		if out.Cost != 17 {
			t.Errorf("hint %q: cost got %d want 17", hint, out.Cost)
		}
	}
}

// TestNestedWithDoesNotInheritEnclosingCtes pins the nested-WITH narrowing (spec/design/cte.md §7): a
// nested WITH establishes its OWN CTE scope and does NOT inherit the enclosing statement's CTE
// bindings — a documented DIVERGENCE from PostgreSQL (which inherits them), so it cannot live in the
// oracle corpus. Inside the nested WITH, an enclosing CTE name with no base table is 42P01; one that
// shadows a base table reads the BASE TABLE (PG would read the CTE).
func TestNestedWithDoesNotInheritEnclosingCtes(t *testing.T) {
	// (a) No base table named e: the inner reference to the enclosing CTE e is unresolved -> 42P01.
	db := cteT3(t)
	_, err := db.Execute("WITH e AS (SELECT 1 AS v) SELECT * FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s", nil)
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "42P01" {
		t.Fatalf("enclosing CTE inside a nested WITH: want 42P01, got %v", err)
	}

	// (b) A base table e exists: inside the nested WITH the enclosing CTE e is invisible, so the
	// reference resolves to the BASE TABLE (rows are the table's, not the CTE's). PG diverges.
	db = dbWith(
		t,
		"CREATE TABLE e (v i32 PRIMARY KEY)",
		"INSERT INTO e VALUES (7), (8)",
	)
	got := firstInts(queryRows(t, db,
		"WITH e AS (SELECT 1 AS v) SELECT v FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s ORDER BY v"))
	if !eqInts(got, 7, 8) {
		t.Fatalf("nested WITH reads the BASE TABLE e, not the enclosing CTE; got %v", got)
	}
}
