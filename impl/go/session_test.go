package jed

// Session surface (spec/design/session.md §2): the Engine-owned STATEFUL default session (the bare
// single-handle path), and — after the §2.4 convergence — ADDITIONAL sessions minted by db.Session
// over a shared *Database core (each owns its private *Engine, shares committed storage through the
// core, carries an independent envelope, autocommit with the lazy gate — no swap), plus the explicit
// Idle/Open/Failed transaction state machine. Per-core API behaviors the shared corpus cannot
// express (it is single-handle SQL-in/rows-out — CLAUDE.md §10). Mirrors impl/rust/tests/session.rs.

import "testing"

func sessExec(t *testing.T, db dbHandle, sql string) {
	t.Helper()
	if _, err := queryOutcome(db, sql, nil); err != nil {
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
	t.Parallel()
	// The Engine-owned default session holds an open BEGIN block across *separate* calls (the
	// PG/SQLite connection model, §2.1); db.Status() exposes the explicit state machine.
	db := memDB().Session(SessionOptions{})
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
	t.Parallel()
	// A statement error inside a block poisons it: status is Failed, every later statement but
	// ROLLBACK/COMMIT is 25P02 (§2.2 / transactions.md §6), and ROLLBACK returns to Idle.
	db := memDB().Session(SessionOptions{})
	sessExec(t, db, "BEGIN")
	_, err := queryOutcome(db, "SELECT * FROM missing", nil)
	if got := sessCode(t, err); got != "42P01" {
		t.Fatalf("want 42P01, got %s", got)
	}
	if db.Status() != TxFailed {
		t.Fatalf("after error in block: want Failed, got %v", db.Status())
	}
	_, err = queryOutcome(db, "SELECT 1", nil)
	if got := sessCode(t, err); got != "25P02" {
		t.Fatalf("want 25P02, got %s", got)
	}
	sessExec(t, db, "ROLLBACK")
	if db.Status() != TxIdle {
		t.Fatalf("after ROLLBACK: want Idle, got %v", db.Status())
	}
}

func TestAdditionalSessionSharesStorageWithIndependentSettings(t *testing.T) {
	t.Parallel()
	// Two sessions over one shared Database core: each owns its private Engine, but committed storage
	// is shared through the core (§2.4) — no swap. Settings (the cost ceiling) are independent.
	db := memDB()
	a := db.Session(SessionOptions{})
	if _, err := queryOutcome(a, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(a, "INSERT INTO t VALUES (1, 10)", nil); err != nil {
		t.Fatal(err)
	}

	// A second session with its own cost ceiling — a's is untouched.
	s := db.Session(SessionOptions{MaxCost: 5})
	if s.MaxCost() != 5 {
		t.Fatalf("session MaxCost: want 5, got %d", s.MaxCost())
	}
	if a.MaxCost() != 0 {
		t.Fatalf("a MaxCost: want 0, got %d", a.MaxCost())
	}

	// It sees a's committed data (committed storage is shared via the core).
	out, err := queryOutcome(s, "SELECT id, v FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 1 || out.Rows[0][1].Int != 10 {
		t.Fatalf("unexpected rows from second session: %v", out.Rows)
	}

	// A write through the second session (autocommit, lazy gate) is visible to a's next read.
	if _, err := queryOutcome(s, "INSERT INTO t VALUES (2, 20)", nil); err != nil {
		t.Fatal(err)
	}
	out, err = queryOutcome(a, "SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 2 || out.Rows[0][0].Int != 1 || out.Rows[1][0].Int != 2 {
		t.Fatalf("unexpected rows after second-session write: %v", out.Rows)
	}

	// Each session keeps its own state/settings: a is still Idle and unlimited.
	if a.Status() != TxIdle || a.MaxCost() != 0 {
		t.Fatalf("session a not as expected: status=%v maxCost=%d", a.Status(), a.MaxCost())
	}
}

func TestAdditionalSessionCostCeilingEnforced(t *testing.T) {
	t.Parallel()
	// The session's settings drive the execution path: a tiny ceiling aborts the scan with 54P01,
	// while an unlimited session runs it fine — both over the same shared core.
	db := memDB()
	a := db.Session(SessionOptions{})
	if _, err := queryOutcome(a, "CREATE TABLE t (id i32 PRIMARY KEY)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(a, "INSERT INTO t VALUES (1), (2), (3)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(a, "SELECT * FROM t", nil); err != nil { // unlimited
		t.Fatal(err)
	}

	s := db.Session(SessionOptions{MaxCost: 1})
	_, err := queryOutcome(s, "SELECT * FROM t", nil)
	if got := sessCode(t, err); got != "54P01" {
		t.Fatalf("want 54P01, got %s", got)
	}

	if _, err := queryOutcome(a, "SELECT * FROM t", nil); err != nil { // a unaffected
		t.Fatal(err)
	}
	if a.MaxCost() != 0 {
		t.Fatalf("a MaxCost changed: %d", a.MaxCost())
	}
}

func TestAdditionalSessionUpdateClosureCommitsToSharedStorage(t *testing.T) {
	t.Parallel()
	db := memDB()
	a := db.Session(SessionOptions{})
	if _, err := queryOutcome(a, "CREATE TABLE t (id i32 PRIMARY KEY)", nil); err != nil {
		t.Fatal(err)
	}

	s := db.Session(SessionOptions{})
	err := s.Update(func(tx *Transaction) error {
		if _, err := queryOutcome(tx, "INSERT INTO t VALUES (1)", nil); err != nil {
			return err
		}
		_, err := queryOutcome(tx, "INSERT INTO t VALUES (2)", nil)
		return err
	})
	if err != nil {
		t.Fatal(err)
	}

	// The update closure committed through the shared core; another session sees both rows.
	out, err := queryOutcome(a, "SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if out.Rows[0][0].Int != 2 {
		t.Fatalf("want count 2, got %v", out.Rows[0][0])
	}
	if a.Status() != TxIdle {
		t.Fatalf("want Idle, got %v", a.Status())
	}
}
