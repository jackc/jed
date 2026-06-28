package jed

// Phase 5 (P5.2): explicit transactions — the host Transaction API (spec/design/api.md §2.2 / §6,
// transactions.md §4.4). The SQL BEGIN/COMMIT/ROLLBACK surface and its visibility / rollback /
// read-only / failed-block semantics are pinned by the shared conformance corpus
// (suites/transactions/); these per-core tests cover the programmatic surface the corpus does not
// exercise: db.Begin(writable), the db.View/db.Update closure wrappers, and db.Commit/db.Rollback
// as the same mechanism.

import "testing"

// txCount returns the number of rows of `SELECT * FROM t` against the committed/visible state.
func txCount(t *testing.T, db *Engine, table string) int {
	t.Helper()
	out, err := Execute(db, "SELECT * FROM "+table)
	if err != nil {
		t.Fatalf("count %s: %v", table, err)
	}
	return len(out.Rows)
}

func TestBeginExecuteCommitIsVisible(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	tx, err := db.Begin(true)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute("INSERT INTO t VALUES (1)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	// read-your-writes within the transaction
	rows, err := tx.Query("SELECT id FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if n := len(rows.rows); n != 2 {
		t.Fatalf("expected 2 rows visible inside the tx, got %d", n)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, db, "t"); n != 2 {
		t.Fatalf("committed rows = %d want 2", n)
	}
}

func TestBeginExecuteRollbackDiscards(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	tx, err := db.Begin(true)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
	if db.InTransaction() {
		t.Fatal("tx must be closed after rollback")
	}
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows after rollback = %d want 1", n)
	}
}

func TestUpdateClosureCommitsOnNil(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	err := db.Update(func(tx *Transaction) error {
		if _, e := tx.Execute("INSERT INTO t VALUES (1)", nil); e != nil {
			return e
		}
		_, e := tx.Execute("INSERT INTO t VALUES (2)", nil)
		return e
	})
	if err != nil {
		t.Fatal(err)
	}
	if db.InTransaction() {
		t.Fatal("tx must be closed after Update")
	}
	if n := txCount(t, db, "t"); n != 2 {
		t.Fatalf("rows after Update = %d want 2", n)
	}
}

func TestUpdateClosureRollsBackOnErr(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	err := db.Update(func(tx *Transaction) error {
		if _, e := tx.Execute("INSERT INTO t VALUES (2)", nil); e != nil {
			return e
		}
		// a duplicate key fails the closure -> the whole Update auto-rolls-back
		_, e := tx.Execute("INSERT INTO t VALUES (1)", nil)
		return e
	})
	if err == nil || err.(*EngineError).Code() != "23505" {
		t.Fatalf("expected 23505, got %v", err)
	}
	if db.InTransaction() {
		t.Fatal("tx must be closed after a failed Update")
	}
	// both the failing insert AND the earlier successful one are discarded
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows after failed Update = %d want 1", n)
	}
}

func TestViewIsReadOnly(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1), (2)")
	// a read inside a View works
	got := 0
	if err := db.View(func(tx *Transaction) error {
		rows, e := tx.Query("SELECT id FROM t", nil)
		if e != nil {
			return e
		}
		got = len(rows.rows)
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	if got != 2 {
		t.Fatalf("rows read in View = %d want 2", got)
	}
	// a write inside a View is 25006, and the View auto-rolls-back
	err := db.View(func(tx *Transaction) error {
		_, e := tx.Execute("INSERT INTO t VALUES (3)", nil)
		return e
	})
	if err == nil || err.(*EngineError).Code() != "25006" {
		t.Fatalf("expected 25006, got %v", err)
	}
	if n := txCount(t, db, "t"); n != 2 {
		t.Fatalf("rows after read-only write attempt = %d want 2", n)
	}
}

func TestNestedBeginIs25001(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	tx, err := db.Begin(true)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute("INSERT INTO t VALUES (1)", nil); err != nil {
		t.Fatal(err)
	}
	// a SQL BEGIN inside an already-open transaction is 25001
	if _, e := tx.Execute("BEGIN", nil); e == nil || e.(*EngineError).Code() != "25001" {
		t.Fatalf("expected 25001, got %v", e)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows = %d want 1", n)
	}
}

func TestCommitRollbackAreNoopsInAutocommit(t *testing.T) {
	db := NewEngine()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	// no open transaction: both are lenient no-op successes (transactions.md §4.2)
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	if err := db.Rollback(); err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	if err := db.Rollback(); err != nil { // does not undo the autocommitted insert
		t.Fatal(err)
	}
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows = %d want 1", n)
	}
}
