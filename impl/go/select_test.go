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
