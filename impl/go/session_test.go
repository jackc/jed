package jed

// S1 session surface (spec/design/session.md §2): the Database-owned STATEFUL default session,
// ADDITIONAL sessions minted by db.NewSession (shared committed storage, independent settings +
// transaction state, run sequentially via the swap), the relocated settings, and the explicit
// Idle/Open/Failed transaction state machine. Per-core API behaviors the shared corpus cannot
// express (it is single-handle SQL-in/rows-out — CLAUDE.md §10). Mirrors impl/rust/tests/session.rs.

import "testing"

func sessExec(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func sessCode(t *testing.T, err error) string {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error, got nil")
	}
	return err.(*EngineError).Code()
}

func TestDefaultSessionIsStatefulAcrossCalls(t *testing.T) {
	// The Database-owned default session holds an open BEGIN block across *separate* calls (the
	// PG/SQLite connection model, §2.1); db.Status() exposes the explicit state machine.
	db := NewDatabase()
	if db.Status() != TxIdle {
		t.Fatalf("fresh db: want Idle, got %v", db.Status())
	}
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	sessExec(t, db, "BEGIN")
	if db.Status() != TxOpen {
		t.Fatalf("after BEGIN: want Open, got %v", db.Status())
	}
	sessExec(t, db, "INSERT INTO t VALUES (1)")
	if db.Status() != TxOpen {
		t.Fatalf("mid-block separate call: want Open, got %v", db.Status())
	}
	sessExec(t, db, "COMMIT")
	if db.Status() != TxIdle {
		t.Fatalf("after COMMIT: want Idle, got %v", db.Status())
	}
}

func TestFailedBlockIsTheFailedState(t *testing.T) {
	// A statement error inside a block poisons it: status is Failed, every later statement but
	// ROLLBACK/COMMIT is 25P02 (§2.2 / transactions.md §6), and ROLLBACK returns to Idle.
	db := NewDatabase()
	sessExec(t, db, "BEGIN")
	_, err := Execute(db, "SELECT * FROM missing")
	if got := sessCode(t, err); got != "42P01" {
		t.Fatalf("want 42P01, got %s", got)
	}
	if db.Status() != TxFailed {
		t.Fatalf("after error in block: want Failed, got %v", db.Status())
	}
	_, err = Execute(db, "SELECT 1")
	if got := sessCode(t, err); got != "25P02" {
		t.Fatalf("want 25P02, got %s", got)
	}
	sessExec(t, db, "ROLLBACK")
	if db.Status() != TxIdle {
		t.Fatalf("after ROLLBACK: want Idle, got %v", db.Status())
	}
}

func TestAdditionalSessionSharesStorageWithIndependentSettings(t *testing.T) {
	db := NewDatabase()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")

	// Mint a second session with its own cost ceiling — the default is untouched.
	s := db.NewSession(SessionOptions{MaxCost: 5})
	if s.MaxCost() != 5 {
		t.Fatalf("session MaxCost: want 5, got %d", s.MaxCost())
	}
	if db.MaxCost() != 0 {
		t.Fatalf("default MaxCost: want 0, got %d", db.MaxCost())
	}

	// It sees the default session's committed data (committed storage is shared).
	out, err := s.ExecuteSQL(db, "SELECT id, v FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 1 || out.Rows[0][1].Int != 10 {
		t.Fatalf("unexpected rows from second session: %v", out.Rows)
	}

	// A write through the second session is visible to the default session.
	if _, err := s.ExecuteSQL(db, "INSERT INTO t VALUES (2, 20)", nil); err != nil {
		t.Fatal(err)
	}
	out, err = Execute(db, "SELECT id FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 2 || out.Rows[0][0].Int != 1 || out.Rows[1][0].Int != 2 {
		t.Fatalf("unexpected rows after second-session write: %v", out.Rows)
	}

	// The swap restored the default session: still Idle, still unlimited.
	if db.Status() != TxIdle || db.MaxCost() != 0 {
		t.Fatalf("default session not restored: status=%v maxCost=%d", db.Status(), db.MaxCost())
	}
}

func TestAdditionalSessionCostCeilingEnforcedViaSwap(t *testing.T) {
	// Proves the swap installs the additional session's settings into the execution path: a tiny
	// ceiling aborts the scan with 54P01, while the unlimited default runs it fine.
	db := NewDatabase()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	sessExec(t, db, "INSERT INTO t VALUES (1), (2), (3)")
	sessExec(t, db, "SELECT * FROM t") // default: unlimited

	s := db.NewSession(SessionOptions{MaxCost: 1})
	_, err := s.ExecuteSQL(db, "SELECT * FROM t", nil)
	if got := sessCode(t, err); got != "54P01" {
		t.Fatalf("want 54P01, got %s", got)
	}

	sessExec(t, db, "SELECT * FROM t") // default unaffected
	if db.MaxCost() != 0 {
		t.Fatalf("default MaxCost changed: %d", db.MaxCost())
	}
}

func TestAdditionalSessionUpdateClosureCommitsToSharedStorage(t *testing.T) {
	db := NewDatabase()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")

	s := db.NewSession(SessionOptions{})
	err := s.Update(db, func(tx *Transaction) error {
		if _, err := tx.Execute("INSERT INTO t VALUES (1)", nil); err != nil {
			return err
		}
		_, err := tx.Execute("INSERT INTO t VALUES (2)", nil)
		return err
	})
	if err != nil {
		t.Fatal(err)
	}

	out, err := Execute(db, "SELECT count(*) FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if out.Rows[0][0].Int != 2 {
		t.Fatalf("want count 2, got %v", out.Rows[0][0])
	}
	if db.Status() != TxIdle {
		t.Fatalf("want Idle, got %v", db.Status())
	}
}
