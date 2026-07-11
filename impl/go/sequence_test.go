package jed

// Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean oracle
// corpus cannot express: the transactional-rollback divergence (nextval rolls back — a deliberate
// PG divergence, §5), the read-only 25006 gate, session-local currval, and NULL propagation. The
// PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE) lives in
// suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10). Mirrors
// impl/rust/tests/sequence.rs.

import (
	"path/filepath"
	"testing"
)

// seqOneInt runs sql and returns its single int result (or nil for a NULL result).
func seqOneInt(t *testing.T, db dbHandle, sql string) *int64 {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != outcomeQuery {
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
func seqMustInt(t *testing.T, db dbHandle, sql string, want int64) {
	t.Helper()
	got := seqOneInt(t, db, sql)
	if got == nil {
		t.Fatalf("%q: expected %d, got NULL", sql, want)
	}
	if *got != want {
		t.Fatalf("%q: expected %d, got %d", sql, want, *got)
	}
}

func seqErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

// THE headline divergence (§5): a nextval advance inside a transaction is discarded by ROLLBACK
// (PostgreSQL keeps it — its sequences are non-transactional). jed is deterministic instead.
func TestSequenceNextvalRollsBack(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, "CREATE SEQUENCE s", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1) // committed: last_value 1

	if _, err := queryOutcome(db, "BEGIN", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 2) // working: last_value 2
	seqMustInt(t, db, "SELECT nextval('s')", 3) // working: last_value 3
	if _, err := queryOutcome(db, "ROLLBACK", nil); err != nil {
		t.Fatal(err)
	}

	// jed: the in-transaction advances vanished — the committed counter is still 1, so the next
	// value is 2 (PostgreSQL would return 4 here: its advance to 3 survived the rollback).
	seqMustInt(t, db, "SELECT nextval('s')", 2)

	// A COMMITted advance, by contrast, persists (identical to PG).
	if _, err := queryOutcome(db, "BEGIN", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 3)
	if _, err := queryOutcome(db, "COMMIT", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 4)
}

// A failed autocommit statement does not advance the sequence either (the per-statement rollback).
func TestSequenceFailedStatementDoesNotAdvance(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// A two-value [1, 2] sequence (MINVALUE == MAXVALUE is rejected, matching PG — §15.2).
	if _, err := queryOutcome(db, "CREATE SEQUENCE s MAXVALUE 2", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1)
	seqMustInt(t, db, "SELECT nextval('s')", 2)
	// The next nextval traps 2200H — and because it failed, the counter did not move, so a second
	// attempt traps identically.
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, "CREATE SEQUENCE s", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT nextval('s')", 1) // 1, defines the session value

	if _, err := queryOutcome(db, "BEGIN READ ONLY", nil); err != nil {
		t.Fatal(err)
	}
	if code := seqErrCode(t, db, "SELECT nextval('s')"); code != "25006" {
		t.Fatalf("expected 25006, got %s", code)
	}
	if _, err := queryOutcome(db, "ROLLBACK", nil); err != nil {
		t.Fatal(err)
	}

	// currval is allowed in a read-only transaction (it mutates nothing) — a fresh block, since the
	// 25006 above poisoned the previous one (any in-block error aborts it).
	if _, err := queryOutcome(db, "BEGIN READ ONLY", nil); err != nil {
		t.Fatal(err)
	}
	seqMustInt(t, db, "SELECT currval('s')", 1)
	if _, err := queryOutcome(db, "ROLLBACK", nil); err != nil {
		t.Fatal(err)
	}
}

// currval is session-local and 55000 before the first nextval.
func TestSequenceCurrvalSessionState(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, "CREATE SEQUENCE s", nil); err != nil {
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
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

// The ALTER SEQUENCE actions jed still does not support are 0A000 — each VALID in PostgreSQL, so they
// cannot live in the PG-clean oracle corpus (sequences.md §15). AS type is foreclosed because the
// value type is not persisted (§14.4); OWNED BY / OWNER TO / SET … have no jed concept. (The option
// set INCREMENT/MINVALUE/… and RENAME TO are now supported — see ddl/alter_sequence.test.)
func TestSequenceAlterUnsupportedActionsAre0A000(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE SEQUENCE s")
	for _, sql := range []string{
		"ALTER SEQUENCE s AS bigint",
		"ALTER SEQUENCE s OWNED BY t.c",
		"ALTER SEQUENCE s OWNER TO bob",
		"ALTER SEQUENCE s SET SCHEMA other",
	} {
		if code := seqErrCode(t, db, sql); code != "0A000" {
			t.Fatalf("%q: expected 0A000, got %s", sql, code)
		}
	}
	// ALTER TABLE owns its authoritative planned grammar; ADD COLUMN is a later slice → 0A000.
	if code := seqErrCode(t, db, "ALTER TABLE t ADD COLUMN c i32"); code != "0A000" {
		t.Fatalf("expected 0A000, got %s", code)
	}
}

// An ALTER SEQUENCE … <options> edit is a transactional catalog write — it rolls back with its block
// (the §5 divergence applies to every ALTER action, not just RESTART). A jed-vs-PG divergence, so a
// per-core unit test, not corpus.
func TestSequenceAlterOptionsRollBack(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE SEQUENCE s INCREMENT 1")
	mustExec(t, db, "BEGIN")
	mustExec(t, db, "ALTER SEQUENCE s INCREMENT BY 100")
	mustExec(t, db, "ROLLBACK")
	// The INCREMENT edit rolled back, so the step is still 1: setval to 5, next is 6 (not 105).
	mustExec(t, db, "SELECT setval('s', 5)")
	seqMustInt(t, db, "SELECT nextval('s')", 6)
}

// setval/ALTER … RESTART are writes — a READ ONLY transaction rejects each with 25006 (each in its
// own block, since the error poisons the block). lastval/currval (pure reads) are allowed.
func TestSequenceSetvalAlterInReadOnlyIs25006(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
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

// ---------------------------------------------------------------------------
// S3 — serial / bigserial / smallserial (spec/design/sequences.md §12). These per-core tests cover
// what the PG-clean corpus cannot: the auto-named OWNED sequence, the DROP TABLE auto-drop surviving
// a reopen (file persistence of the owner link, v13), and the DROP SEQUENCE 2BP01. The PG-agreeing
// surface lives in suites/ddl/serial.test. Mirrors impl/rust/tests/sequence.rs.

// seqQueryRows runs sql and returns the int values of its rows (panicking on NULL/non-int cells).
func seqQueryRows(t *testing.T, db dbHandle, sql string) [][]int64 {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != outcomeQuery {
		t.Fatalf("%q: expected a query, got kind %v", sql, out.Kind)
	}
	rows := make([][]int64, len(out.Rows))
	for i, r := range out.Rows {
		row := make([]int64, len(r))
		for j, v := range r {
			if v.Kind != ValInt {
				t.Fatalf("%q: row %d col %d expected int, got %v", sql, i, j, v)
			}
			row[j] = v.Int
		}
		rows[i] = row
	}
	return rows
}

// A serial column desugars to an integer column, NOT NULL, with a DEFAULT nextval backed by an
// auto-created OWNED sequence named <table>_<col>_seq. Inserts auto-number from 1.
func TestSerialDesugarsToOwnedSequence(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id serial PRIMARY KEY, b bigserial, s smallserial, v text)")
	rows := seqQueryRows(t, db, "INSERT INTO t (v) VALUES ('a'), ('b') RETURNING id, b, s")
	want := [][]int64{{1, 1, 1}, {2, 2, 2}}
	if len(rows) != 2 || rows[0][0] != want[0][0] || rows[1][0] != want[1][0] ||
		rows[0][1] != 1 || rows[1][1] != 2 || rows[0][2] != 1 || rows[1][2] != 2 {
		t.Fatalf("auto-numbering wrong: got %v want %v", rows, want)
	}
	seqMustInt(t, db, "SELECT nextval('t_id_seq')", 3)
	seqMustInt(t, db, "SELECT nextval('t_b_seq')", 3)
	seqMustInt(t, db, "SELECT nextval('t_s_seq')", 3)
}

// A NULL into a serial column violates the implied NOT NULL (23502); an explicit value overrides the
// default and does NOT advance the sequence (PG).
func TestSerialNotNullAndExplicitOverride(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id serial PRIMARY KEY, v text)")
	if code := seqErrCode(t, db, "INSERT INTO t (id, v) VALUES (NULL, 'x')"); code != "23502" {
		t.Fatalf("expected 23502, got %s", code)
	}
	mustExec(t, db, "INSERT INTO t (id, v) VALUES (100, 'y')")
	rows := seqQueryRows(t, db, "INSERT INTO t (v) VALUES ('z') RETURNING id")
	if rows[0][0] != 1 {
		t.Fatalf("expected next default 1, got %d", rows[0][0])
	}
}

// An explicit DEFAULT on a serial column conflicts with the synthesized one — 42601 (PG).
func TestSerialWithExplicitDefaultIs42601(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if code := seqErrCode(t, db, "CREATE TABLE t (id serial DEFAULT 5)"); code != "42601" {
		t.Fatalf("expected 42601, got %s", code)
	}
}

// The auto-name collision-resolves with a numeric suffix when <table>_<col>_seq is taken (PG).
func TestSerialSeqNameCollisionResolves(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE SEQUENCE t_id_seq")
	mustExec(t, db, "CREATE TABLE t (id serial)")
	mustExec(t, db, "INSERT INTO t (id) VALUES (DEFAULT)")
	// t_id_seq (the manual one) was never advanced; t_id_seq1 produced the row's 1.
	seqMustInt(t, db, "SELECT nextval('t_id_seq1')", 2)
	seqMustInt(t, db, "SELECT nextval('t_id_seq')", 1)
}

// DROP SEQUENCE of an OWNED (serial) sequence is 2BP01; DROP TABLE auto-drops it.
func TestSerialOwnedSequenceDropRules(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id serial PRIMARY KEY)")
	if code := seqErrCode(t, db, "DROP SEQUENCE t_id_seq"); code != "2BP01" {
		t.Fatalf("expected 2BP01, got %s", code)
	}
	mustExec(t, db, "DROP TABLE t")
	if code := seqErrCode(t, db, "SELECT nextval('t_id_seq')"); code != "42P01" {
		t.Fatalf("expected 42P01 after auto-drop, got %s", code)
	}
	mustExec(t, db, "CREATE SEQUENCE t_id_seq") // the name is free to reuse
}

// The OWNED BY link persists (format_version 13): after create + commit + reopen, DROP TABLE still
// auto-drops the owned sequence, and DROP SEQUENCE of it is still 2BP01.
func TestSerialOwnedLinkSurvivesReopen(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "serial_owned_reopen.jed")
	db, err := create(path, databaseOptions{PageSize: 4096, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id serial PRIMARY KEY, v text)")
	mustExec(t, db, "INSERT INTO t (v) VALUES ('a')")
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if code := seqErrCode(t, db, "DROP SEQUENCE t_id_seq"); code != "2BP01" {
		t.Fatalf("expected 2BP01 after reopen, got %s", code)
	}
	mustExec(t, db, "DROP TABLE t")
	if code := seqErrCode(t, db, "SELECT nextval('t_id_seq')"); code != "42P01" {
		t.Fatalf("expected 42P01 after reopen auto-drop, got %s", code)
	}
}

// serial is recognized only in a column-type position — a CAST to it is an undefined type.
func TestSerialIsNotACastableType(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if code := seqErrCode(t, db, "SELECT 1::serial"); code != "42704" {
		t.Fatalf("expected 42704, got %s", code)
	}
}
