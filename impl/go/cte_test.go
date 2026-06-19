package jed

// Common table expressions — WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>,
// non-recursive (spec/design/cte.md). The row/name/error assertions and the inline/materialize
// cost contract live in the shared conformance corpus (spec/conformance/suites/cte/*.test). What
// remains here is the MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), which the corpus
// pins by rows but NOT by cost.

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
