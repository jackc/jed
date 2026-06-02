package abide

// Phase D/E: SELECT — projection, WHERE (=, ordering ops, IS [NOT] NULL),
// three-valued logic, ORDER BY (NULLs first), and CAST. These complement the
// conformance corpus with finer-grained per-feature assertions.

import "testing"

func query(t *testing.T, db *Database, sql string) [][]Value {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("expected query result for %q", sql)
	}
	return out.Rows
}

func queryIDs(t *testing.T, db *Database, sql string) []int64 {
	t.Helper()
	rows := query(t, db, sql)
	out := make([]int64, len(rows))
	for i, r := range rows {
		out[i] = r[0].Int
	}
	return out
}

func setupT(t *testing.T) *Database {
	return dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, NULL)",
	)
}

func TestPointLookupByPrimaryKey(t *testing.T) {
	db := setupT(t)
	rows := query(t, db, "SELECT v FROM t WHERE id = 2")
	if len(rows) != 1 || rows[0][0].Int != 20 {
		t.Errorf("got %+v", rows)
	}
}

func TestSelectStarProjectsAllColumns(t *testing.T) {
	db := setupT(t)
	rows := query(t, db, "SELECT * FROM t WHERE id = 1")
	if len(rows) != 1 || rows[0][0].Int != 1 || rows[0][1].Int != 10 {
		t.Errorf("got %+v", rows)
	}
}

func TestFullScanInPrimaryKeyOrder(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY)",
		"INSERT INTO t VALUES (3)",
		"INSERT INTO t VALUES (1)",
		"INSERT INTO t VALUES (2)",
	)
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("got %v", got)
	}
}

func TestIsNullAndIsNotNull(t *testing.T) {
	db := setupT(t)
	if got := queryIDs(t, db, "SELECT id FROM t WHERE v IS NULL"); !eqInts(got, 3) {
		t.Errorf("IS NULL got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t WHERE v IS NOT NULL ORDER BY id"); !eqInts(got, 1, 2) {
		t.Errorf("IS NOT NULL got %v", got)
	}
}

func TestEqualityWithNullIsUnknown(t *testing.T) {
	db := setupT(t)
	if rows := query(t, db, "SELECT id FROM t WHERE v = NULL"); len(rows) != 0 {
		t.Errorf("expected no rows, got %+v", rows)
	}
}

func TestComparisonExcludesNullRows(t *testing.T) {
	db := setupT(t)
	if got := queryIDs(t, db, "SELECT id FROM t WHERE v > 5 ORDER BY id"); !eqInts(got, 1, 2) {
		t.Errorf("got %v", got)
	}
}

func TestOrderByNullsFirstThenDescLast(t *testing.T) {
	db := setupT(t)
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY v"); !eqInts(got, 3, 1, 2) {
		t.Errorf("asc got %v want [3 1 2]", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY v DESC"); !eqInts(got, 2, 1, 3) {
		t.Errorf("desc got %v want [2 1 3]", got)
	}
}

func TestCrossTypeComparisonPromotes(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE p (id int32 PRIMARY KEY, a int16, c int64)",
		"INSERT INTO p VALUES (1, 100, 100)",
		"INSERT INTO p VALUES (2, 100, 300)",
	)
	if got := queryIDs(t, db, "SELECT id FROM p WHERE a = c ORDER BY id"); !eqInts(got, 1) {
		t.Errorf("got %v", got)
	}
}

func TestCastNarrowingFitsAndTraps(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, b int64)",
		"INSERT INTO t VALUES (1, 1000)",
		"INSERT INTO t VALUES (2, 5000000000)",
	)
	rows := query(t, db, "SELECT CAST(b AS int16) FROM t WHERE id = 1")
	if len(rows) != 1 || rows[0][0].Int != 1000 {
		t.Errorf("got %+v", rows)
	}
	wantErr(t, db, "SELECT CAST(b AS int16) FROM t WHERE id = 2", "22003")
}

func TestUnknownColumnTraps(t *testing.T) {
	db := setupT(t)
	wantErr(t, db, "SELECT nope FROM t", "42703")
	wantErr(t, db, "SELECT id FROM t WHERE nope = 1", "42703")
}

func TestSelectFromMissingTableTraps(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "SELECT x FROM nope", "42P01")
}

func limitDB(t *testing.T) *Database {
	return dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, 30)",
		"INSERT INTO t VALUES (4, 40)",
		"INSERT INTO t VALUES (5, 50)",
	)
}

func TestLimitCapsAndOffsetSkips(t *testing.T) {
	db := limitDB(t)
	// LIMIT takes the first n; OFFSET skips; the two clauses commute.
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id LIMIT 2"); !eqInts(got, 1, 2) {
		t.Errorf("LIMIT 2 got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1"); !eqInts(got, 2, 3) {
		t.Errorf("LIMIT 2 OFFSET 1 got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id OFFSET 1 LIMIT 2"); !eqInts(got, 2, 3) {
		t.Errorf("OFFSET 1 LIMIT 2 got %v", got)
	}
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id OFFSET 3"); !eqInts(got, 4, 5) {
		t.Errorf("OFFSET 3 got %v", got)
	}
	// LIMIT 0 and an OFFSET past the end are empty (not errors); a huge LIMIT clamps.
	if got := query(t, db, "SELECT id FROM t ORDER BY id LIMIT 0"); len(got) != 0 {
		t.Errorf("LIMIT 0 got %v", got)
	}
	if got := query(t, db, "SELECT id FROM t ORDER BY id OFFSET 10"); len(got) != 0 {
		t.Errorf("OFFSET 10 got %v", got)
	}
	if got := query(t, db, "SELECT id FROM t ORDER BY id LIMIT 100"); len(got) != 5 {
		t.Errorf("LIMIT 100 got %v", got)
	}
}

func TestLimitOffsetWindowReducesProducedCost(t *testing.T) {
	// The slice runs before projection, so only windowed rows charge row_produced:
	// 5 scanned + 2 produced = 7 (spec/design/cost.md §3).
	db := limitDB(t)
	out, err := Execute(db, "SELECT id FROM t ORDER BY id LIMIT 2")
	if err != nil {
		t.Fatal(err)
	}
	if out.Cost != 7 {
		t.Errorf("cost got %d want 7", out.Cost)
	}
}

func TestNegativeLimitAndOffsetTrapDistinctly(t *testing.T) {
	db := setupT(t)
	wantErr(t, db, "SELECT id FROM t LIMIT -1", "2201W")
	wantErr(t, db, "SELECT id FROM t OFFSET -1", "2201X")
}

func TestDuplicateLimitOrOffsetIsSyntaxError(t *testing.T) {
	db := setupT(t)
	wantErr(t, db, "SELECT id FROM t LIMIT 1 LIMIT 2", "42601")
	wantErr(t, db, "SELECT id FROM t OFFSET 1 OFFSET 2", "42601")
}

func TestOutOfRangeLiteralInComparisonTraps(t *testing.T) {
	// Context-adaptive literal typing (spec/design/types.md §6): a literal that cannot be
	// represented in the compared column's type is a type error (22003), not a silent
	// non-match — for every operator. An in-range literal compares normally.
	db := dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, small int16)",
		"INSERT INTO t VALUES (1, 30000)",
	)
	if got := queryIDs(t, db, "SELECT id FROM t WHERE small = 30000"); !eqInts(got, 1) {
		t.Errorf("in-range got %v", got)
	}
	for _, sql := range []string{
		"SELECT id FROM t WHERE small = 100000",
		"SELECT id FROM t WHERE small < 100000",
		"SELECT id FROM t WHERE small > 100000",
	} {
		wantErr(t, db, sql, "22003")
	}
	// The context is the compared column: 5e9 fits int64 but not int32 (the id column).
	wantErr(t, db, "SELECT id FROM t WHERE id = 5000000000", "22003")
}
