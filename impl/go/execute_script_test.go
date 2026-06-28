package jed

// S2 ExecuteScript host-API surface (spec/design/session.md §4.2): the multi-statement
// migration/import convenience — split, run each in order, discard rows, return the O(1)
// ScriptSummary. All-or-nothing when Idle, join-when-Open, in-script transaction control 0A000.
// Host-API behaviors the single-statement corpus cannot call (CLAUDE.md §10). Mirrors
// impl/rust/tests/execute_script.rs.

import "testing"

func scriptCount(t *testing.T, db *Engine) int64 {
	t.Helper()
	out, err := Execute(db, "SELECT count(*) FROM t")
	if err != nil {
		t.Fatalf("count: %v", err)
	}
	return out.Rows[0][0].Int
}

func TestScriptSummaryCountsAndCommitsAtomicallyWhenIdle(t *testing.T) {
	db := NewEngine()
	summary, err := db.ExecuteScript(
		`CREATE TABLE t (id i32 PRIMARY KEY, v i32);
		 INSERT INTO t VALUES (1, 10);
		 INSERT INTO t VALUES (2, 20), (3, 30);
		 UPDATE t SET v = v + 1 WHERE id >= 2;
		 DELETE FROM t WHERE id = 1;`,
	)
	if err != nil {
		t.Fatalf("ExecuteScript: %v", err)
	}
	if summary.StatementsRun != 5 {
		t.Fatalf("StatementsRun: want 5, got %d", summary.StatementsRun)
	}
	if summary.RowsAffectedTotal != 1+2+2+1 { // insert+insert+update+delete; DDL = 0
		t.Fatalf("RowsAffectedTotal: want 6, got %d", summary.RowsAffectedTotal)
	}
	if summary.Cost <= 0 {
		t.Fatalf("Cost: want > 0, got %d", summary.Cost)
	}
	if db.Status() != TxIdle {
		t.Fatalf("status: want Idle, got %v", db.Status())
	}
	if n := scriptCount(t, db); n != 2 {
		t.Fatalf("count: want 2, got %d", n)
	}
}

func TestScriptIsAllOrNothingOnError(t *testing.T) {
	db := NewEngine()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	_, err := db.ExecuteScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (1)")
	if got := sessCode(t, err); got != "23505" {
		t.Fatalf("want 23505, got %s", got)
	}
	if db.Status() != TxIdle {
		t.Fatalf("status: want Idle, got %v", db.Status())
	}
	if n := scriptCount(t, db); n != 0 {
		t.Fatalf("count: want 0 (all-or-nothing), got %d", n)
	}
}

func TestScriptSelectRowsDiscardedButStatementCounted(t *testing.T) {
	db := NewEngine()
	summary, err := db.ExecuteScript(
		`CREATE TABLE t (id i32 PRIMARY KEY);
		 INSERT INTO t VALUES (1), (2);
		 SELECT * FROM t;`,
	)
	if err != nil {
		t.Fatalf("ExecuteScript: %v", err)
	}
	if summary.StatementsRun != 3 {
		t.Fatalf("StatementsRun: want 3, got %d", summary.StatementsRun)
	}
	if summary.RowsAffectedTotal != 2 { // only the INSERT; the SELECT contributes 0
		t.Fatalf("RowsAffectedTotal: want 2, got %d", summary.RowsAffectedTotal)
	}
	if n := scriptCount(t, db); n != 2 {
		t.Fatalf("count: want 2, got %d", n)
	}
}

func TestEmptyScriptIsANoOpSuccess(t *testing.T) {
	db := NewEngine()
	summary, err := db.ExecuteScript("  -- just a comment\n /* and a block */ ;;; ")
	if err != nil {
		t.Fatalf("ExecuteScript: %v", err)
	}
	if summary != (ScriptSummary{}) {
		t.Fatalf("want zero summary, got %+v", summary)
	}
	if db.Status() != TxIdle {
		t.Fatalf("status: want Idle, got %v", db.Status())
	}
}

func TestInScriptTransactionControlIsFeatureNotSupported(t *testing.T) {
	db := NewEngine()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for _, script := range []string{
		"INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2)",
		"INSERT INTO t VALUES (1); BEGIN; INSERT INTO t VALUES (2)",
		"INSERT INTO t VALUES (1); ROLLBACK",
	} {
		_, err := db.ExecuteScript(script)
		if got := sessCode(t, err); got != "0A000" {
			t.Fatalf("%q: want 0A000, got %s", script, got)
		}
		if db.Status() != TxIdle {
			t.Fatalf("%q: status want Idle, got %v", script, db.Status())
		}
		if n := scriptCount(t, db); n != 0 {
			t.Fatalf("%q: count want 0 (wrapper rolled back), got %d", script, n)
		}
	}
}

func TestScriptJoinsAnOpenTransactionWithoutCommitting(t *testing.T) {
	db := NewEngine()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	sessExec(t, db, "BEGIN")
	summary, err := db.ExecuteScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
	if err != nil {
		t.Fatalf("ExecuteScript: %v", err)
	}
	if summary.StatementsRun != 2 {
		t.Fatalf("StatementsRun: want 2, got %d", summary.StatementsRun)
	}
	if db.Status() != TxOpen { // NOT auto-committed — the caller's block stays open
		t.Fatalf("status: want Open, got %v", db.Status())
	}
	if n := scriptCount(t, db); n != 2 {
		t.Fatalf("count inside block: want 2, got %d", n)
	}
	sessExec(t, db, "ROLLBACK")
	if n := scriptCount(t, db); n != 0 {
		t.Fatalf("count after rollback: want 0, got %d", n)
	}
}

func TestScriptErrorInsideOpenTransactionLeavesItFailed(t *testing.T) {
	db := NewEngine()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	sessExec(t, db, "BEGIN")
	_, err := db.ExecuteScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (1)")
	if got := sessCode(t, err); got != "23505" {
		t.Fatalf("want 23505, got %s", got)
	}
	if db.Status() != TxFailed { // execute_script does NOT roll back a tx it does not own
		t.Fatalf("status: want Failed, got %v", db.Status())
	}
	sessExec(t, db, "ROLLBACK")
	if db.Status() != TxIdle {
		t.Fatalf("status: want Idle, got %v", db.Status())
	}
}

func TestAdditionalSessionRunsAScriptOverTheSharedCore(t *testing.T) {
	// ExecuteScript on an ADDITIONAL session (§2.1/§2.4) shares committed storage through the Database
	// core and commits the run all-or-nothing — another session sees it.
	db := NewDatabase()
	a := db.Session(SessionOptions{})
	if _, err := a.Execute("CREATE TABLE t (id i32 PRIMARY KEY)", nil); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	summary, err := s.ExecuteScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
	if err != nil {
		t.Fatalf("ExecuteScript: %v", err)
	}
	if summary.StatementsRun != 2 {
		t.Fatalf("StatementsRun: want 2, got %d", summary.StatementsRun)
	}
	out, err := a.Execute("SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if out.Rows[0][0].Int != 2 {
		t.Fatalf("count: want 2, got %v", out.Rows[0][0])
	}
	if a.Status() != TxIdle {
		t.Fatalf("status: want Idle, got %v", a.Status())
	}
}
