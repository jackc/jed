package jed

// Host-API behaviors of the View/Update closure, failed-block poisoning at the parse/bind stage, and
// the never-waiting host operations (spec/design/api.md §2.2/§2.3, transactions.md §6) — per-core
// because the corpus is single-handle SQL-in/rows-out (CLAUDE.md §10). Mirrors
// impl/rust/tests/closure_txn.rs and impl/ts/tests/closure_txn.test.ts.
//
//   - Transaction control inside a closure is 2D000 (invalid_transaction_termination): SQL
//     COMMIT/ROLLBACK through the closure's Transaction aborts the block (PostgreSQL's rule for a
//     procedure called within a block), and the handle's own Commit/Rollback is refused without
//     aborting. The closure's writes are never silently discarded: a closure that swallows the error
//     gets 25P02 from Update, not a nil that committed nothing.
//   - A parse error (42601), the 54000 input-size limit, an arg-conversion error, and a Prepare parse
//     error inside an open block poison it like an execution error; BEGIN in a failed block is 25P02.
//   - Close / Attach / Detach never wait for the writer gate: 55006 while a write block is open on
//     this goroutine (which waiting would deadlock), and fine once it ends.

import (
	"context"
	"testing"
)

func ctxCode(t *testing.T, err error) string {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error, got nil")
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an *EngineError, got %T: %v", err, err)
	}
	return ee.Code()
}

func TestUpdateClosureSQLCommitIs2D000AndNeverDiscardsWrites(t *testing.T) {
	t.Parallel()
	db := memDB()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY)")

	// The closure propagates the 2D000: Update rolls back and reports it.
	err := s.Update(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "INSERT INTO t VALUES (1)", nil); e != nil {
			return e
		}
		_, e := queryOutcome(tx, "COMMIT", nil)
		if ctxCode(t, e) != "2D000" {
			t.Fatalf("COMMIT inside Update: want 2D000, got %v", e)
		}
		// the block is failed: a later statement is 25P02
		if _, e2 := queryOutcome(tx, "INSERT INTO t VALUES (5)", nil); ctxCode(t, e2) != "25P02" {
			t.Fatalf("after 2D000: want 25P02, got %v", e2)
		}
		return e
	})
	if ctxCode(t, err) != "2D000" {
		t.Fatalf("Update: want 2D000, got %v", err)
	}
	if s.InTransaction() {
		t.Fatal("the block must be ended after Update")
	}
	if n := txCount(t, s, "t"); n != 0 {
		t.Fatalf("rows after the rolled-back Update = %d want 0", n)
	}

	// The closure swallows the error and returns nil: Update still reports that nothing committed.
	err = s.Update(func(tx *Transaction) error {
		_, _ = queryOutcome(tx, "INSERT INTO t VALUES (1)", nil)
		_, _ = queryOutcome(tx, "ROLLBACK", nil)
		_, _ = queryOutcome(tx, "INSERT INTO t VALUES (5)", nil)
		return nil
	})
	if ctxCode(t, err) != "25P02" {
		t.Fatalf("Update over a failed block: want 25P02, got %v", err)
	}
	if n := txCount(t, s, "t"); n != 0 {
		t.Fatalf("rows = %d want 0", n)
	}

	// The gate was released: this session and another both write again.
	mustExec(t, s, "INSERT INTO t VALUES (2)")
	other := db.Session(SessionOptions{})
	defer other.Close()
	mustExec(t, other, "INSERT INTO t VALUES (3)")
	if n := txCount(t, other, "t"); n != 2 {
		t.Fatalf("rows = %d want 2", n)
	}
}

func TestClosureTransactionHandleCommitRollbackIs2D000WithoutAborting(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY)")
	err := s.Update(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "INSERT INTO t VALUES (1)", nil); e != nil {
			return e
		}
		if c := ctxCode(t, tx.Commit()); c != "2D000" {
			t.Fatalf("tx.Commit inside Update: want 2D000, got %s", c)
		}
		if c := ctxCode(t, tx.Rollback()); c != "2D000" {
			t.Fatalf("tx.Rollback inside Update: want 2D000, got %s", c)
		}
		// not a statement error: the block is still open and usable
		_, e := queryOutcome(tx, "INSERT INTO t VALUES (2)", nil)
		return e
	})
	if err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, s, "t"); n != 2 {
		t.Fatalf("rows = %d want 2", n)
	}
	// View: the same gate.
	err = s.View(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "COMMIT", nil); ctxCode(t, e) != "2D000" {
			t.Fatalf("COMMIT inside View: want 2D000, got %v", e)
		}
		return nil
	})
	if ctxCode(t, err) != "25P02" {
		t.Fatalf("View over a failed block: want 25P02, got %v", err)
	}
}

func TestSavepointsWorkInsideUpdateClosure(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY)")
	err := s.Update(func(tx *Transaction) error {
		for _, sql := range []string{"INSERT INTO t VALUES (1)", "SAVEPOINT a", "INSERT INTO t VALUES (2)", "ROLLBACK TO a", "INSERT INTO t VALUES (3)", "RELEASE a"} {
			if _, e := queryOutcome(tx, sql, nil); e != nil {
				return e
			}
		}
		// a nested BEGIN stays 25001 and does not abort
		if _, e := queryOutcome(tx, "BEGIN", nil); ctxCode(t, e) != "25001" {
			t.Fatalf("nested BEGIN: want 25001, got %v", e)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	out, err := queryOutcome(s, "SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 2 || out.Rows[0][0].Int != 1 || out.Rows[1][0].Int != 3 {
		t.Fatalf("rows = %v want [1 3]", out.Rows)
	}
}

func TestParseStageErrorsPoisonAnOpenBlock(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{MaxSQLLength: 64})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY)")
	expectPoisoned := func(what string) {
		t.Helper()
		if _, e := queryOutcome(s, "SELECT id FROM t", nil); ctxCode(t, e) != "25P02" {
			t.Fatalf("after %s: want 25P02, got %v", what, e)
		}
		if _, e := queryOutcome(s, "BEGIN", nil); ctxCode(t, e) != "25P02" {
			t.Fatalf("BEGIN in a failed block after %s: want 25P02, got %v", what, e)
		}
		if _, e := queryOutcome(s, "ROLLBACK", nil); e != nil {
			t.Fatal(e)
		}
		mustExec(t, s, "SELECT id FROM t")
	}

	// a syntax error (the parse stage, before any planning)
	mustExec(t, s, "BEGIN")
	if _, e := queryOutcome(s, "SELECT 1 FROM", nil); ctxCode(t, e) != "42601" {
		t.Fatalf("want 42601, got %v", e)
	}
	expectPoisoned("a 42601")

	// the input-size limit
	mustExec(t, s, "BEGIN")
	if _, e := queryOutcome(s, "SELECT '"+string(make([]byte, 80))+"'", nil); ctxCode(t, e) != "54000" {
		t.Fatalf("want 54000, got %v", e)
	}
	expectPoisoned("a 54000")

	// an arg-conversion error on the ergonomic surface
	mustExec(t, s, "BEGIN")
	if _, e := s.Exec(context.Background(), "INSERT INTO t VALUES ($1)", struct{}{}); e == nil {
		t.Fatal("want an arg-conversion error")
	}
	expectPoisoned("an arg-conversion error")

	// a Prepare parse error on the session
	mustExec(t, s, "BEGIN")
	if _, e := s.Prepare("SELEC 1"); ctxCode(t, e) != "42601" {
		t.Fatalf("want 42601, got %v", e)
	}
	expectPoisoned("a Prepare 42601")

	// the closure's Transaction too
	err := s.Update(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "SELECT 1 FROM", nil); ctxCode(t, e) != "42601" {
			t.Fatalf("want 42601, got %v", e)
		}
		if _, e := queryOutcome(tx, "SELECT id FROM t", nil); ctxCode(t, e) != "25P02" {
			t.Fatalf("want 25P02, got %v", e)
		}
		return nil
	})
	if ctxCode(t, err) != "25P02" {
		t.Fatalf("Update over a failed block: want 25P02, got %v", err)
	}
	if s.InTransaction() {
		t.Fatal("the block must be ended")
	}
}

func TestCloseAttachDetachNeverWaitForAnOpenWriteBlock(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("aux", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	mustExec(t, s, "INSERT INTO t VALUES (1)")
	// Each would deadlock on this goroutine if it waited for the gate the open block holds.
	if c := ctxCode(t, db.Close()); c != "55006" {
		t.Fatalf("Close with an open write block: want 55006, got %s", c)
	}
	if c := ctxCode(t, db.Attach("more", AttachMemory(), false)); c != "55006" {
		t.Fatalf("Attach with an open write block: want 55006, got %s", c)
	}
	if c := ctxCode(t, db.Detach("aux")); c != "55006" {
		t.Fatalf("Detach with an open write block: want 55006, got %s", c)
	}
	// The refusals disturbed nothing: the block commits and the operations then succeed.
	if err := s.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, s, "t"); n != 1 {
		t.Fatalf("rows = %d want 1", n)
	}
	if err := db.Attach("more", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	if err := db.Detach("aux"); err != nil {
		t.Fatal(err)
	}
	s.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}
