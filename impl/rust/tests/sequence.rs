//! Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean
//! oracle corpus cannot express: the **transactional-rollback divergence** (nextval rolls back —
//! a deliberate PG divergence, §5), the read-only `25006` gate, session-local `currval`, and NULL
//! propagation. The PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE)
//! lives in suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10).

use std::path::PathBuf;

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, Session, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn one_int(db: &mut Session, sql: &str) -> Option<i64> {
    match db.execute(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => match &rows[0][0] {
            Value::Int(n) => Some(*n),
            Value::Null => None,
            v => panic!("expected int/null, got {v:?}"),
        },
        o => panic!("expected a query, got {o:?}"),
    }
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[]).unwrap_err().state.code().to_string()
}

/// THE headline divergence (§5): a `nextval` advance inside a transaction is discarded by ROLLBACK
/// (PostgreSQL keeps it — its sequences are non-transactional). jed is deterministic instead.
#[test]
fn nextval_rolls_back() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1)); // committed: last_value 1

    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2)); // working: last_value 2
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(3)); // working: last_value 3
    db.execute("ROLLBACK", &[]).unwrap();

    // jed: the in-transaction advances vanished — the committed counter is still 1, so the next
    // value is 2 (PostgreSQL would return 4 here: its advance to 3 survived the rollback).
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2));

    // A COMMITted advance, by contrast, persists (identical to PG).
    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(3));
    db.execute("COMMIT", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(4));
}

/// A failed autocommit statement does not advance the sequence either (the per-statement rollback).
#[test]
fn failed_statement_does_not_advance() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    // A two-value [1, 2] sequence (MINVALUE == MAXVALUE is rejected, matching PG — §15.2).
    db.execute("CREATE SEQUENCE s MAXVALUE 2", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1));
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2));
    // The next nextval traps 2200H — and because it failed, the counter did not move, so a second
    // attempt traps identically.
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "2200H");
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "2200H");
}

/// nextval is a write, so a READ ONLY transaction rejects it with 25006; currval (a pure read) is
/// allowed there (spec/design/sequences.md §4/§6).
#[test]
fn nextval_in_read_only_is_25006() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s", &[]).unwrap();
    one_int(&mut db, "SELECT nextval('s')"); // 1, defines the session value

    db.execute("BEGIN READ ONLY", &[]).unwrap();
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "25006");
    db.execute("ROLLBACK", &[]).unwrap();

    // currval is allowed in a read-only transaction (it mutates nothing) — a fresh block, since the
    // 25006 above poisoned the previous one (any in-block error aborts it).
    db.execute("BEGIN READ ONLY", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT currval('s')"), Some(1));
    db.execute("ROLLBACK", &[]).unwrap();
}

/// currval is session-local and 55000 before the first nextval.
#[test]
fn currval_session_state() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s", &[]).unwrap();
    assert_eq!(err_code(&mut db, "SELECT currval('s')"), "55000");
    one_int(&mut db, "SELECT nextval('s')");
    assert_eq!(one_int(&mut db, "SELECT currval('s')"), Some(1));
    // currval does not advance: repeated reads return the same value.
    assert_eq!(one_int(&mut db, "SELECT currval('s')"), Some(1));
}

// --- S2 (setval / lastval / ALTER SEQUENCE RESTART, spec/design/sequences.md §4/§6) -----------

/// A `setval` is transactional too (the §5 divergence): an advance inside a rolled-back transaction
/// is discarded — PostgreSQL would keep it.
#[test]
fn setval_rolls_back() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s START 1", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1)); // committed last_value 1

    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT setval('s', 99)"), Some(99)); // working last_value 99
    db.execute("ROLLBACK", &[]).unwrap();

    // jed: the setval vanished — the committed counter is still 1, so the next value is 2.
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2));
}

/// An `ALTER SEQUENCE … RESTART` is transactional as well (the same §5 divergence).
#[test]
fn alter_restart_rolls_back() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s START 10", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(10));

    db.execute("BEGIN", &[]).unwrap();
    db.execute("ALTER SEQUENCE s RESTART WITH 100", &[])
        .unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(100)); // working
    db.execute("ROLLBACK", &[]).unwrap();

    // The RESTART (and its advance) rolled back — the committed counter is still 10, next is 11.
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(11));
}

/// A `nextval`'s `lastval`/`currval` session updates roll back with the transaction too (§5/§6):
/// after a rolled-back `nextval`, `lastval` reverts to its pre-transaction state. (The PG-agreeing
/// `lastval` *values* — tracking the most recent `nextval`, reflecting a `setval` on that same
/// sequence — live in the oracle corpus; this asserts only the rollback, which the corpus cannot.)
#[test]
fn lastval_rolls_back() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE a START 100", &[]).unwrap();
    db.execute("CREATE SEQUENCE b START 200", &[]).unwrap();
    one_int(&mut db, "SELECT nextval('a')"); // committed: lastval → a's 100
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(100));

    db.execute("BEGIN", &[]).unwrap();
    one_int(&mut db, "SELECT nextval('b')"); // working: lastval → b's 200
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(200));
    db.execute("ROLLBACK", &[]).unwrap();

    // The in-transaction nextval('b') vanished, so lastval reverts to a's committed 100.
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(100));
}

/// The `ALTER SEQUENCE` actions jed still does not support are `0A000` — each VALID in PostgreSQL, so
/// they cannot live in the PG-clean oracle corpus (sequences.md §15). `AS type` is foreclosed because
/// the value type is not persisted (§14.4); `OWNED BY` / `OWNER TO` / `SET …` have no jed concept.
/// (The option set INCREMENT/MINVALUE/… and RENAME TO are now supported — see ddl/alter_sequence.test.)
#[test]
fn alter_unsupported_actions_are_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s", &[]).unwrap();
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s AS bigint"), "0A000");
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s OWNED BY t.c"), "0A000");
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s OWNER TO bob"), "0A000");
    assert_eq!(
        err_code(&mut db, "ALTER SEQUENCE s SET SCHEMA other"),
        "0A000"
    );
    // ALTER of a non-sequence object is not a known statement at all → 42601 (no escape hatch).
    assert_eq!(err_code(&mut db, "ALTER TABLE t ADD COLUMN c i32"), "42601");
}

/// An `ALTER SEQUENCE … <options>` edit is a transactional catalog write — it rolls back with its
/// block (the §5 divergence applies to every ALTER action, not just RESTART). A jed-vs-PG divergence
/// (PG's sequence definition change is non-transactional), so a per-core unit test, not corpus.
#[test]
fn alter_options_roll_back() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s INCREMENT 1", &[]).unwrap();
    db.execute("BEGIN", &[]).unwrap();
    db.execute("ALTER SEQUENCE s INCREMENT BY 100", &[])
        .unwrap();
    db.execute("ROLLBACK", &[]).unwrap();
    // The INCREMENT edit rolled back, so the step is still 1: setval to 5, next is 6 (not 105).
    db.execute("SELECT setval('s', 5)", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(6));
}

/// `setval`/`ALTER … RESTART` are writes — a READ ONLY transaction rejects each with 25006 (each in
/// its own block, since the error poisons the block). `lastval`/`currval` (pure reads) are allowed.
#[test]
fn setval_alter_in_read_only_is_25006() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE s", &[]).unwrap();
    one_int(&mut db, "SELECT nextval('s')"); // 1, defines session state

    db.execute("BEGIN READ ONLY", &[]).unwrap();
    assert_eq!(err_code(&mut db, "SELECT setval('s', 5)"), "25006");
    db.execute("ROLLBACK", &[]).unwrap();

    db.execute("BEGIN READ ONLY", &[]).unwrap();
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s RESTART"), "25006");
    db.execute("ROLLBACK", &[]).unwrap();

    // lastval is allowed in a read-only block (it mutates nothing).
    db.execute("BEGIN READ ONLY", &[]).unwrap();
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(1));
    db.execute("ROLLBACK", &[]).unwrap();
}

// ---------------------------------------------------------------------------
// S3 — serial / bigserial / smallserial (spec/design/sequences.md §12). These per-core tests cover
// what the PG-clean corpus cannot: the auto-named OWNED sequence (introspected by the name PG also
// derives), the DROP TABLE auto-drop surviving a reopen (file persistence of the owner link, v13),
// and the DROP SEQUENCE 2BP01. The PG-agreeing surface (the inserted values, an explicit override)
// lives in suites/ddl/serial.test.

/// A `serial` column desugars to an integer column, NOT NULL, with a DEFAULT nextval backed by an
/// auto-created OWNED sequence named `<table>_<col>_seq`. Inserts auto-number from 1.
#[test]
fn serial_desugars_to_owned_sequence() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute(
        "CREATE TABLE t (id serial PRIMARY KEY, b bigserial, s smallserial, v text)",
        &[],
    )
    .unwrap();
    // Two inserts auto-number every serial column from 1 (each column's own sequence).
    match db
        .execute(
            "INSERT INTO t (v) VALUES ('a'), ('b') RETURNING id, b, s",
            &[],
        )
        .unwrap()
    {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0], vec![Value::Int(1), Value::Int(1), Value::Int(1)]);
            assert_eq!(rows[1], vec![Value::Int(2), Value::Int(2), Value::Int(2)]);
        }
        o => panic!("expected a query, got {o:?}"),
    }
    // The owned sequences exist under PG's derived names and keep advancing.
    assert_eq!(one_int(&mut db, "SELECT nextval('t_id_seq')"), Some(3));
    assert_eq!(one_int(&mut db, "SELECT nextval('t_b_seq')"), Some(3));
    assert_eq!(one_int(&mut db, "SELECT nextval('t_s_seq')"), Some(3));
}

/// A NULL into a serial column violates the implied NOT NULL (23502); an explicit value overrides
/// the default and does NOT advance the sequence (PG).
#[test]
fn serial_not_null_and_explicit_override() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE t (id serial PRIMARY KEY, v text)", &[])
        .unwrap();
    assert_eq!(
        err_code(&mut db, "INSERT INTO t (id, v) VALUES (NULL, 'x')"),
        "23502"
    );
    // Supply an explicit id — the sequence is untouched, so the next default is still 1.
    db.execute("INSERT INTO t (id, v) VALUES (100, 'y')", &[])
        .unwrap();
    match db
        .execute("INSERT INTO t (v) VALUES ('z') RETURNING id", &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        o => panic!("expected a query, got {o:?}"),
    }
}

/// An explicit DEFAULT on a serial column conflicts with the synthesized one — 42601 (PG).
#[test]
fn serial_with_explicit_default_is_42601() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (id serial DEFAULT 5)"),
        "42601"
    );
}

/// The auto-name collision-resolves with a numeric suffix when `<table>_<col>_seq` is taken (PG).
#[test]
fn serial_seq_name_collision_resolves() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE SEQUENCE t_id_seq", &[]).unwrap();
    db.execute("CREATE TABLE t (id serial)", &[]).unwrap();
    // The pre-existing t_id_seq forced the owned sequence to t_id_seq1.
    db.execute("INSERT INTO t (id) VALUES (DEFAULT)", &[])
        .unwrap();
    // t_id_seq (the manual one) was never advanced; t_id_seq1 produced the row's 1.
    assert_eq!(one_int(&mut db, "SELECT nextval('t_id_seq1')"), Some(2));
    assert_eq!(one_int(&mut db, "SELECT nextval('t_id_seq')"), Some(1));
}

/// DROP SEQUENCE of an OWNED (serial) sequence is 2BP01; DROP TABLE auto-drops it.
#[test]
fn owned_sequence_drop_rules() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE t (id serial PRIMARY KEY)", &[])
        .unwrap();
    // Cannot drop the owned sequence directly.
    assert_eq!(err_code(&mut db, "DROP SEQUENCE t_id_seq"), "2BP01");
    // DROP TABLE auto-drops it — afterwards the sequence name is undefined (42P01).
    db.execute("DROP TABLE t", &[]).unwrap();
    assert_eq!(err_code(&mut db, "SELECT nextval('t_id_seq')"), "42P01");
    // The auto-dropped name is free to reuse.
    db.execute("CREATE SEQUENCE t_id_seq", &[]).unwrap();
}

/// The OWNED BY link persists (format_version 13): after create + commit + reopen, DROP TABLE still
/// auto-drops the owned sequence, and DROP SEQUENCE of it is still 2BP01.
#[test]
fn owned_link_survives_reopen() {
    let path = tmp("serial_owned_reopen.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(&path, DatabaseOptions::default())
            .unwrap()
            .session(SessionOptions::default());
        db.execute("CREATE TABLE t (id serial PRIMARY KEY, v text)", &[])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES ('a')", &[]).unwrap();
        db.commit().unwrap();
    }
    {
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
        // The owner link round-tripped: still 2BP01 to drop the sequence directly.
        assert_eq!(err_code(&mut db, "DROP SEQUENCE t_id_seq"), "2BP01");
        // And DROP TABLE still auto-drops it.
        db.execute("DROP TABLE t", &[]).unwrap();
        assert_eq!(err_code(&mut db, "SELECT nextval('t_id_seq')"), "42P01");
        db.commit().unwrap();
    }
    let _ = std::fs::remove_file(&path);
}

/// `serial` is recognized only in a column-type position — a CAST to it is an undefined type.
#[test]
fn serial_is_not_a_castable_type() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    // 42704 undefined_object (serial is not a real type outside CREATE TABLE).
    assert_eq!(err_code(&mut db, "SELECT 1::serial"), "42704");
}
