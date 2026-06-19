package jed

// Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean oracle
// corpus cannot express: the transactional-rollback divergence (nextval rolls back — a deliberate
// PG divergence, §5), the read-only 25006 gate, session-local currval, and NULL propagation. The
// PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE) lives in
// suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10). Mirrors
// impl/rust/tests/sequence.rs.

import "testing"

// seqOneInt runs sql and returns its single int result (or nil for a NULL result).
func seqOneInt(t *testing.T, db *Database, sql string) *int64 {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("%q: expected a query, got kind %v", sql, out.Kind)
	}
	v := out.Rows[0][0]
	switch v.Kind {
	case ValInt:
		n := v.Int
		return &n
	case ValNull:
		return nil
	default:
		t.Fatalf("%q: expected int/null, got %v", sql, v)
		return nil
	}
}

// seqMustInt is seqOneInt asserting a non-NULL value equal to want.
func seqMustInt(t *testing.T, db *Database, sql string, want int64) {
	t.Helper()
	got := seqOneInt(t, db, sql)
	if got == nil {
		t.Fatalf("%q: expected %d, got NULL", sql, want)
	}
	if *got != want {
		t.Fatalf("%q: expected %d, got %d", sql, want, *got)
	}
}

func seqErrCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

// THE headline divergence (§5): a nextval advance inside a transaction is discarded by ROLLBACK
// (PostgreSQL keeps it — its sequences are non-transactional). jed is deterministic instead.
func TestSequenceNextvalRollsBack(t *testing.T) {
	db := NewDatabase()
	if _, err := Execute(db, "CREATE SEQUENCE s"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1) // committed: last_value 1

	if _, err := Execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 2) // working: last_value 2
	seqMustInt(t, db, "SELECT nextval('s')", 3) // working: last_value 3
	if _, err := Execute(db, "ROLLBACK"); err != nil {
		t.Fatal(err)
	}

	// jed: the in-transaction advances vanished — the committed counter is still 1, so the next
	// value is 2 (PostgreSQL would return 4 here: its advance to 3 survived the rollback).
	seqMustInt(t, db, "SELECT nextval('s')", 2)

	// A COMMITted advance, by contrast, persists (identical to PG).
	if _, err := Execute(db, "BEGIN"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 3)
	if _, err := Execute(db, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 4)
}

// A failed autocommit statement does not advance the sequence either (the per-statement rollback).
func TestSequenceFailedStatementDoesNotAdvance(t *testing.T) {
	db := NewDatabase()
	if _, err := Execute(db, "CREATE SEQUENCE s MAXVALUE 1"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1)
	// The next nextval traps 2200H — and because it failed, the counter did not move.
	if code := seqErrCode(t, db, "SELECT nextval('s')"); code != "2200H" {
		t.Fatalf("expected 2200H, got %s", code)
	}
	if code := seqErrCode(t, db, "SELECT nextval('s')"); code != "2200H" {
		t.Fatalf("expected 2200H, got %s", code)
	}
}

// nextval is a write, so a READ ONLY transaction rejects it with 25006; currval (a pure read) is
// allowed there (spec/design/sequences.md §4/§6).
func TestSequenceNextvalInReadOnlyIs25006(t *testing.T) {
	db := NewDatabase()
	if _, err := Execute(db, "CREATE SEQUENCE s"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1) // 1, defines the session value

	if _, err := Execute(db, "BEGIN READ ONLY"); err != nil {
		t.Fatal(err)
	}
	if code := seqErrCode(t, db, "SELECT nextval('s')"); code != "25006" {
		t.Fatalf("expected 25006, got %s", code)
	}
	if _, err := Execute(db, "ROLLBACK"); err != nil {
		t.Fatal(err)
	}

	// currval is allowed in a read-only transaction (it mutates nothing) — a fresh block, since the
	// 25006 above poisoned the previous one (any in-block error aborts it).
	if _, err := Execute(db, "BEGIN READ ONLY"); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT currval('s')", 1)
	if _, err := Execute(db, "ROLLBACK"); err != nil {
		t.Fatal(err)
	}
}

// currval is session-local and 55000 before the first nextval.
func TestSequenceCurrvalSessionState(t *testing.T) {
	db := NewDatabase()
	if _, err := Execute(db, "CREATE SEQUENCE s"); err != nil {
		t.Fatal(err)
	}
	if code := seqErrCode(t, db, "SELECT currval('s')"); code != "55000" {
		t.Fatalf("expected 55000, got %s", code)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1)
	seqMustInt(t, db, "SELECT currval('s')", 1)
	// currval does not advance: repeated reads return the same value.
	seqMustInt(t, db, "SELECT currval('s')", 1)
}

// --- S2 (setval / lastval / ALTER SEQUENCE RESTART, spec/design/sequences.md §4/§6) -----------
// (mustExec helper is shared from api_test.go.)

// A setval is transactional too (the §5 divergence): an advance inside a rolled-back transaction is
// discarded — PostgreSQL would keep it.
func TestSequenceSetvalRollsBack(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE SEQUENCE s START 1")
	seqMustInt(t, db, "SELECT nextval('s')", 1) // committed last_value 1

	mustExec(t, db, "BEGIN")
	seqMustInt(t, db, "SELECT setval('s', 99)", 99) // working last_value 99
	mustExec(t, db, "ROLLBACK")

	// jed: the setval vanished — the committed counter is still 1, so the next value is 2.
	seqMustInt(t, db, "SELECT nextval('s')", 2)
}

// An ALTER SEQUENCE … RESTART is transactional as well (the same §5 divergence).
func TestSequenceAlterRestartRollsBack(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE SEQUENCE s START 10")
	seqMustInt(t, db, "SELECT nextval('s')", 10)

	mustExec(t, db, "BEGIN")
	mustExec(t, db, "ALTER SEQUENCE s RESTART WITH 100")
	seqMustInt(t, db, "SELECT nextval('s')", 100) // working
	mustExec(t, db, "ROLLBACK")

	// The RESTART (and its advance) rolled back — the committed counter is still 10, next is 11.
	seqMustInt(t, db, "SELECT nextval('s')", 11)
}

// A nextval's lastval/currval session updates roll back with the transaction too (§5/§6): after a
// rolled-back nextval, lastval reverts to its pre-transaction state. (The PG-agreeing lastval values
// — tracking the most recent nextval, reflecting a setval on that same sequence — live in the oracle
// corpus; this asserts only the rollback, which the corpus cannot.)
func TestSequenceLastvalRollsBack(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE SEQUENCE a START 100")
	mustExec(t, db, "CREATE SEQUENCE b START 200")
	seqMustInt(t, db, "SELECT nextval('a')", 100) // committed: lastval → a's 100
	seqMustInt(t, db, "SELECT lastval()", 100)

	mustExec(t, db, "BEGIN")
	seqMustInt(t, db, "SELECT nextval('b')", 200) // working: lastval → b's 200
	seqMustInt(t, db, "SELECT lastval()", 200)
	mustExec(t, db, "ROLLBACK")

	// The in-transaction nextval('b') vanished, so lastval reverts to a's committed 100.
	seqMustInt(t, db, "SELECT lastval()", 100)
}

// A non-RESTART ALTER SEQUENCE action is 0A000 in jed (only RESTART is supported this slice) — a
// divergence from PostgreSQL, where ALTER SEQUENCE … INCREMENT BY is valid, so it cannot live in the
// PG-clean oracle corpus.
func TestSequenceAlterNonRestartIs0A000(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE SEQUENCE s")
	if code := seqErrCode(t, db, "ALTER SEQUENCE s INCREMENT BY 2"); code != "0A000" {
		t.Fatalf("expected 0A000, got %s", code)
	}
	if code := seqErrCode(t, db, "ALTER SEQUENCE s OWNED BY t.c"); code != "0A000" {
		t.Fatalf("expected 0A000, got %s", code)
	}
	// ALTER of a non-sequence object is not a known statement at all → 42601 (no escape hatch).
	if code := seqErrCode(t, db, "ALTER TABLE t ADD COLUMN c i32"); code != "42601" {
		t.Fatalf("expected 42601, got %s", code)
	}
}

// setval/ALTER … RESTART are writes — a READ ONLY transaction rejects each with 25006 (each in its
// own block, since the error poisons the block). lastval/currval (pure reads) are allowed.
func TestSequenceSetvalAlterInReadOnlyIs25006(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE SEQUENCE s")
	seqMustInt(t, db, "SELECT nextval('s')", 1) // 1, defines session state

	mustExec(t, db, "BEGIN READ ONLY")
	if code := seqErrCode(t, db, "SELECT setval('s', 5)"); code != "25006" {
		t.Fatalf("expected 25006, got %s", code)
	}
	mustExec(t, db, "ROLLBACK")

	mustExec(t, db, "BEGIN READ ONLY")
	if code := seqErrCode(t, db, "ALTER SEQUENCE s RESTART"); code != "25006" {
		t.Fatalf("expected 25006, got %s", code)
	}
	mustExec(t, db, "ROLLBACK")

	// lastval is allowed in a read-only block (it mutates nothing).
	mustExec(t, db, "BEGIN READ ONLY")
	seqMustInt(t, db, "SELECT lastval()", 1)
	mustExec(t, db, "ROLLBACK")
}
