package jed

// Phase D/E: SELECT — projection, WHERE (=, ordering ops, IS [NOT] NULL),
// three-valued logic, ORDER BY (NULLs last), and CAST. These complement the
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
	return dbWith(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, NULL)",
	)
}

func TestUnknownColumnTraps(t *testing.T) {
	db := setupT(t)
	wantErr(t, db, "SELECT nope FROM t", "42703")
	wantErr(t, db, "SELECT id FROM t WHERE nope = 1", "42703")
}

func limitDB(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, 30)",
		"INSERT INTO t VALUES (4, 40)",
		"INSERT INTO t VALUES (5, 50)",
	)
}

func TestLimitOffsetWindowReducesProducedCost(t *testing.T) {
	// The slice runs before projection, so only windowed rows charge row_produced:
	// 1 page_read (t is one leaf) + 5 scanned + 2 produced = 8 (spec/design/cost.md §3).
	db := limitDB(t)
	out, err := Execute(db, "SELECT id FROM t ORDER BY id LIMIT 2")
	if err != nil {
		t.Fatal(err)
	}
	if out.Cost != 8 {
		t.Errorf("cost got %d want 8", out.Cost)
	}
}
