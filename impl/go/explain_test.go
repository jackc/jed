package jed

// EXPLAIN behaviours the shared corpus cannot express (spec/design/explain.md): privilege delegation
// to the inner statement, the read/write classification via a READ ONLY transaction, and the
// EXPLAIN-owns-its-render-cost invariant. The plan RENDERING itself is asserted in the corpus
// (query/explain*.test, dml/explain_dml.test), which runs on every core.

import "testing"

// EXPLAIN requires the INNER statement's privileges (EXPLAIN INSERT needs INSERT), matching PG —
// even though plain EXPLAIN never executes.
func TestExplainDelegatesInnerPrivileges(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	db.SetDefaultPrivileges(PrivSetEmpty.With(PrivSelect))
	sessExec(t, db, "EXPLAIN SELECT v FROM t") // SELECT privilege is held
	for _, sql := range []string{
		"EXPLAIN INSERT INTO t VALUES (2, 20)",
		"EXPLAIN UPDATE t SET v = 0",
		"EXPLAIN DELETE FROM t",
	} {
		if got := privCode(t, db, sql); got != "42501" {
			t.Fatalf("%s: want 42501, got %s", sql, got)
		}
	}
}

// Plain EXPLAIN of a write is a READ (it never mutates), so it is allowed in a READ ONLY transaction;
// EXPLAIN ANALYZE of a write IS a write and is rejected 25006.
func TestExplainWriteClassification(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	sessExec(t, db, "BEGIN READ ONLY")
	sessExec(t, db, "EXPLAIN DELETE FROM t") // a read — allowed in a read-only transaction
	if got := privCode(t, db, "EXPLAIN ANALYZE DELETE FROM t"); got != "25006" {
		t.Fatalf("EXPLAIN ANALYZE DELETE in READ ONLY: want 25006, got %s", got)
	}
	sessExec(t, db, "ROLLBACK")
}

// Plain EXPLAIN of a DELETE does not mutate; EXPLAIN ANALYZE of an INSERT does (and persists).
func TestExplainAnalyzeExecutesWrites(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	sessExec(t, db, "INSERT INTO t VALUES (2, 20)")
	sessExec(t, db, "EXPLAIN DELETE FROM t") // plan-only — deletes nothing
	if n := scalarInt(t, db, "SELECT count(*) FROM t"); n != 2 {
		t.Fatalf("plain EXPLAIN DELETE mutated: count=%d, want 2", n)
	}
	sessExec(t, db, "EXPLAIN ANALYZE INSERT INTO t VALUES (3, 30)") // executes
	if n := scalarInt(t, db, "SELECT count(*) FROM t"); n != 3 {
		t.Fatalf("EXPLAIN ANALYZE INSERT did not persist: count=%d, want 3", n)
	}
}

// The EXPLAIN statement's OWN cost is one row_produced per emitted plan row — independent of the
// (larger) inner cost reported inside the Analyze root.
func TestExplainOwnsRenderCost(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	sessExec(t, db, "INSERT INTO t VALUES (2, 20)")
	out, err := db.Execute("EXPLAIN ANALYZE SELECT * FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if out.Cost != int64(len(out.Rows)) {
		t.Fatalf("EXPLAIN render cost %d != plan-row count %d", out.Cost, len(out.Rows))
	}
	// The Analyze root (row 0) reports the inner cost, which exceeds the render cost here.
	if got := out.Rows[0][1].Render(); got != "Analyze" {
		t.Fatalf("root node = %q, want Analyze", got)
	}
}

func scalarInt(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Rows[0][0].Int
}
