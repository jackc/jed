package jed

// Phase 5 (P5.2): explicit transactions — the host Session transaction API (spec/design/api.md §2.2 /
// §6, transactions.md §4.4). The SQL BEGIN/COMMIT/ROLLBACK surface and its visibility / rollback /
// read-only / failed-block semantics are pinned by the shared conformance corpus
// (suites/transactions/); these per-core tests cover the programmatic surface the corpus does not
// exercise: s.Begin(writable), the s.View/s.Update closure wrappers, the Close rollback safety net,
// and s.Commit/s.Rollback as the same mechanism. Mirrors impl/rust/tests/transactions.rs.

import "testing"

// txCount returns the number of rows of `SELECT * FROM t` against the committed/visible state.
func txCount(t *testing.T, db dbHandle, table string) int {
	t.Helper()
	out, err := queryOutcome(db, "SELECT * FROM "+table, nil)
	if err != nil {
		t.Fatalf("count %s: %v", table, err)
	}
	return len(out.Rows)
}

func TestBeginExecuteCommitIsVisible(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if err := db.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (1)", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	// read-your-writes within the transaction
	rows, err := db.QueryValues("SELECT id FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	n := 0
	for rows.Next() {
		n++
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Fatalf("expected 2 rows visible inside the tx, got %d", n)
	}
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, db, "t"); n != 2 {
		t.Fatalf("committed rows = %d want 2", n)
	}
}

func TestBeginExecuteRollbackDiscards(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	if err := db.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	if err := db.Rollback(); err != nil {
		t.Fatal(err)
	}
	if db.InTransaction() {
		t.Fatal("tx must be closed after rollback")
	}
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows after rollback = %d want 1", n)
	}
}

func TestDroppingASessionWithAnOpenBlockRollsBack(t *testing.T) {
	// The bbolt safety net: an unfinished transaction never silently commits. Closing a session that
	// left a block open rolls the block back, so a fresh session over the same shared core sees only
	// the pre-block committed state.
	db := memDB()
	func() {
		s := db.Session(SessionOptions{})
		defer s.Close()
		if _, err := queryOutcome(s, "CREATE TABLE t (id i32 PRIMARY KEY)", nil); err != nil {
			t.Fatal(err)
		}
		if _, err := queryOutcome(s, "INSERT INTO t VALUES (1)", nil); err != nil {
			t.Fatal(err)
		}
	}()
	func() {
		s := db.Session(SessionOptions{})
		defer s.Close()
		if err := s.Begin(true); err != nil {
			t.Fatal(err)
		}
		if _, err := queryOutcome(s, "INSERT INTO t VALUES (2)", nil); err != nil {
			t.Fatal(err)
		}
		// s closed here without commit/rollback — the safety net rolls the block back
	}()
	s := db.Session(SessionOptions{})
	defer s.Close()
	if s.InTransaction() {
		t.Fatal("no block is open on a fresh session")
	}
	if n := txCount(t, s, "t"); n != 1 {
		t.Fatalf("rows = %d want 1", n)
	}
}

func TestUpdateClosureCommitsOnNil(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	err := db.Update(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "INSERT INTO t VALUES (1)", nil); e != nil {
			return e
		}
		_, e := queryOutcome(tx, "INSERT INTO t VALUES (2)", nil)
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
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	err := db.Update(func(tx *Transaction) error {
		if _, e := queryOutcome(tx, "INSERT INTO t VALUES (2)", nil); e != nil {
			return e
		}
		// a duplicate key fails the closure -> the whole Update auto-rolls-back
		_, e := queryOutcome(tx, "INSERT INTO t VALUES (1)", nil)
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
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1), (2)")
	// a read inside a View works
	got := 0
	if err := db.View(func(tx *Transaction) error {
		rows, e := tx.QueryValues("SELECT id FROM t", nil)
		if e != nil {
			return e
		}
		for rows.Next() {
			got++
		}
		return rows.Err()
	}); err != nil {
		t.Fatal(err)
	}
	if got != 2 {
		t.Fatalf("rows read in View = %d want 2", got)
	}
	// a write inside a View is 25006, and the View auto-rolls-back
	err := db.View(func(tx *Transaction) error {
		_, e := queryOutcome(tx, "INSERT INTO t VALUES (3)", nil)
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
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if err := db.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (1)", nil); err != nil {
		t.Fatal(err)
	}
	// a SQL BEGIN inside an already-open transaction is 25001
	if _, e := queryOutcome(db, "BEGIN", nil); e == nil || e.(*EngineError).Code() != "25001" {
		t.Fatalf("expected 25001, got %v", e)
	}
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := txCount(t, db, "t"); n != 1 {
		t.Fatalf("rows = %d want 1", n)
	}
}

func TestCommitRollbackAreNoopsInAutocommit(t *testing.T) {
	db := memDB().Session(SessionOptions{})
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
