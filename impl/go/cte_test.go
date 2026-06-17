package jed

// Common table expressions — WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>,
// non-recursive (spec/design/cte.md). These complement the conformance corpus
// (spec/conformance/suites/cte) with finer-grained per-feature assertions: the inline-vs-
// materialize cost split, forward-only visibility, base-table shadowing, the column-rename list,
// set-op / aggregate / JOIN bodies, CTE references inside a nested subquery, and the error /
// narrowing codes (42712 / 42P01 / 42P10 / 42703 / 0A000 / 42601).

import "testing"

// cteT3 is a 3-row, single-node table t(id, n) = {(1,10),(2,20),(3,30)}.
func cteT3(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
	)
}

// cteCost runs sql and returns its accrued cost.
func cteCost(t *testing.T, db *Database, sql string) int64 {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Cost
}

// cteNames runs sql and returns its output column names.
func cteNames(t *testing.T, db *Database, sql string) []string {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("expected a query result for %q", sql)
	}
	return out.ColumnNames
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

func TestCteSingleReferenceInlines(t *testing.T) {
	db := cteT3(t)
	sql := "WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id"
	if got := queryIDs(t, db, sql); !eqInts(got, 1, 2, 3) {
		t.Errorf("rows got %v", got)
	}
	// A single reference INLINES: body (page_read 1 + 3 storage_row_read + 3 row_produced = 7) +
	// the outer's 3 row_produced = 10. No cte_scan_row (cost.md §3).
	if got := cteCost(t, db, sql); got != 10 {
		t.Errorf("cost got %d want 10", got)
	}
}

func TestCteMultipleReferencesMaterialize(t *testing.T) {
	db := cteT3(t)
	// Two references MATERIALIZE: body once (7) + 6 cte_scan_row (two 3-row buffer scans) + 9
	// row_produced (3x3 product) = 22.
	sql := "WITH c AS (SELECT id FROM t) SELECT a.id AS x, b.id AS y FROM c a CROSS JOIN c b"
	if rows := query(t, db, sql); len(rows) != 9 {
		t.Errorf("rows got %d want 9", len(rows))
	}
	if got := cteCost(t, db, sql); got != 22 {
		t.Errorf("cost got %d want 22", got)
	}
}

func TestCteUnreferencedIsNotExecuted(t *testing.T) {
	db := cteT3(t)
	// An unreferenced CTE is planned/type-checked but not executed: only SELECT 1's row_produced.
	if got := cteCost(t, db, "WITH c AS (SELECT id FROM t) SELECT 1"); got != 1 {
		t.Errorf("cost got %d want 1", got)
	}
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

func TestCteLaterReferencesEarlier(t *testing.T) {
	db := cteT3(t)
	sql := "WITH c AS (SELECT id, n FROM t), d AS (SELECT n * 2 AS m FROM c) SELECT m FROM d ORDER BY m"
	if got := queryIDs(t, db, sql); !eqInts(got, 20, 40, 60) {
		t.Errorf("rows got %v", got)
	}
}

func TestCteColumnRenameList(t *testing.T) {
	db := cteT3(t)
	if got := cteNames(t, db,
		"WITH c (a, b) AS (SELECT id, n FROM t) SELECT a, b FROM c ORDER BY a"); !eqStrs(got, "a", "b") {
		t.Errorf("rename names got %v", got)
	}
	// Fewer aliases than body columns: a partial rename — the first column becomes `a`, the second
	// keeps its body name `n` (PostgreSQL).
	if got := cteNames(t, db,
		"WITH c (a) AS (SELECT id, n FROM t) SELECT * FROM c ORDER BY a"); !eqStrs(got, "a", "n") {
		t.Errorf("partial rename names got %v", got)
	}
}

func TestCteSetOpAndAggregateBodies(t *testing.T) {
	db := cteT3(t)
	if got := queryIDs(t, db,
		"WITH c AS (SELECT n FROM t WHERE id = 1 UNION ALL SELECT n FROM t WHERE id = 2) SELECT n FROM c ORDER BY n"); !eqInts(got, 10, 20) {
		t.Errorf("set-op body got %v", got)
	}
	if got := queryIDs(t, db,
		"WITH c AS (SELECT count(*) AS k FROM t) SELECT k FROM c"); !eqInts(got, 3) {
		t.Errorf("aggregate body got %v", got)
	}
}

func TestCteJoinOfTwoCtes(t *testing.T) {
	db := cteT3(t)
	sql := "WITH c AS (SELECT id, n FROM t), d AS (SELECT id FROM t WHERE n >= 20) " +
		"SELECT c.n FROM c JOIN d ON c.id = d.id ORDER BY c.id"
	if got := queryIDs(t, db, sql); !eqInts(got, 20, 30) {
		t.Errorf("join of two CTEs got %v", got)
	}
}

func TestCteReferencedInNestedSubquery(t *testing.T) {
	db := cteT3(t)
	sql := "WITH c AS (SELECT n FROM t) SELECT id FROM t WHERE n = (SELECT max(n) FROM c) ORDER BY id"
	if got := queryIDs(t, db, sql); !eqInts(got, 3) {
		t.Errorf("nested-subquery CTE ref got %v", got)
	}
}

func TestCteShadowsBaseTableOutsideBodyNotInside(t *testing.T) {
	// The CTE `t` shadows the base table in the outer query, but its OWN body resolves the base
	// table (the binding is not in scope for itself — spec/design/cte.md §2).
	db := cteT3(t)
	sql := "WITH t AS (SELECT n + 100 AS n FROM t) SELECT n FROM t ORDER BY n"
	if got := queryIDs(t, db, sql); !eqInts(got, 110, 120, 130) {
		t.Errorf("shadowing got %v", got)
	}
}

func TestCteErrorCodes(t *testing.T) {
	db := cteT3(t)
	cases := []struct {
		sql  string
		code string
	}{
		// Duplicate CTE name in one list.
		{"WITH c AS (SELECT id FROM t), c AS (SELECT id FROM t) SELECT id FROM c", "42712"},
		// Self-reference (non-recursive) — no base table `c`.
		{"WITH c AS (SELECT id FROM c) SELECT id FROM c", "42P01"},
		// Forward reference to a later CTE.
		{"WITH c AS (SELECT id FROM d), d AS (SELECT id FROM t) SELECT id FROM c", "42P01"},
		// Column-rename arity: too MANY aliases is 42P10 (too few is a legal partial rename).
		{"WITH c (a, b, x) AS (SELECT id, n FROM t) SELECT a FROM c", "42P10"},
		// A body resolves only its own scope — an unknown column is the ordinary 42703.
		{"WITH c AS (SELECT missing FROM t) SELECT id FROM c", "42703"},
		// WITH RECURSIVE is deferred.
		{"WITH RECURSIVE c AS (SELECT id FROM t) SELECT id FROM c", "0A000"},
		// A nested WITH (top-level-only narrowing) is a syntax error.
		{"WITH a AS (WITH b AS (SELECT id FROM t) SELECT id FROM b) SELECT id FROM a", "42601"},
	}
	for _, c := range cases {
		if got := errCode(t, db, c.sql); got != c.code {
			t.Errorf("%q: code got %s want %s", c.sql, got, c.code)
		}
	}
}
