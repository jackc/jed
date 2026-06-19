//! Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean
//! oracle corpus cannot express: the **transactional-rollback divergence** (nextval rolls back —
//! a deliberate PG divergence, §5), the read-only `25006` gate, session-local `currval`, and NULL
//! propagation. The PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE)
//! lives in suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10).

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn one_int(db: &mut Database, sql: &str) -> Option<i64> {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => match &rows[0][0] {
            Value::Int(n) => Some(*n),
            Value::Null => None,
            v => panic!("expected int/null, got {v:?}"),
        },
        o => panic!("expected a query, got {o:?}"),
    }
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql).unwrap_err().state.code().to_string()
}

/// THE headline divergence (§5): a `nextval` advance inside a transaction is discarded by ROLLBACK
/// (PostgreSQL keeps it — its sequences are non-transactional). jed is deterministic instead.
#[test]
fn nextval_rolls_back() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1)); // committed: last_value 1

    execute(&mut db, "BEGIN").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2)); // working: last_value 2
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(3)); // working: last_value 3
    execute(&mut db, "ROLLBACK").unwrap();

    // jed: the in-transaction advances vanished — the committed counter is still 1, so the next
    // value is 2 (PostgreSQL would return 4 here: its advance to 3 survived the rollback).
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2));

    // A COMMITted advance, by contrast, persists (identical to PG).
    execute(&mut db, "BEGIN").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(3));
    execute(&mut db, "COMMIT").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(4));
}

/// A failed autocommit statement does not advance the sequence either (the per-statement rollback).
#[test]
fn failed_statement_does_not_advance() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s MAXVALUE 1").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1));
    // The next nextval traps 2200H — and because it failed, the counter did not move.
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "2200H");
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "2200H");
}

/// nextval is a write, so a READ ONLY transaction rejects it with 25006; currval (a pure read) is
/// allowed there (spec/design/sequences.md §4/§6).
#[test]
fn nextval_in_read_only_is_25006() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s").unwrap();
    one_int(&mut db, "SELECT nextval('s')"); // 1, defines the session value

    execute(&mut db, "BEGIN READ ONLY").unwrap();
    assert_eq!(err_code(&mut db, "SELECT nextval('s')"), "25006");
    execute(&mut db, "ROLLBACK").unwrap();

    // currval is allowed in a read-only transaction (it mutates nothing) — a fresh block, since the
    // 25006 above poisoned the previous one (any in-block error aborts it).
    execute(&mut db, "BEGIN READ ONLY").unwrap();
    assert_eq!(one_int(&mut db, "SELECT currval('s')"), Some(1));
    execute(&mut db, "ROLLBACK").unwrap();
}

/// currval is session-local and 55000 before the first nextval.
#[test]
fn currval_session_state() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s").unwrap();
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
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s START 1").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(1)); // committed last_value 1

    execute(&mut db, "BEGIN").unwrap();
    assert_eq!(one_int(&mut db, "SELECT setval('s', 99)"), Some(99)); // working last_value 99
    execute(&mut db, "ROLLBACK").unwrap();

    // jed: the setval vanished — the committed counter is still 1, so the next value is 2.
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(2));
}

/// An `ALTER SEQUENCE … RESTART` is transactional as well (the same §5 divergence).
#[test]
fn alter_restart_rolls_back() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s START 10").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(10));

    execute(&mut db, "BEGIN").unwrap();
    execute(&mut db, "ALTER SEQUENCE s RESTART WITH 100").unwrap();
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(100)); // working
    execute(&mut db, "ROLLBACK").unwrap();

    // The RESTART (and its advance) rolled back — the committed counter is still 10, next is 11.
    assert_eq!(one_int(&mut db, "SELECT nextval('s')"), Some(11));
}

/// A `nextval`'s `lastval`/`currval` session updates roll back with the transaction too (§5/§6):
/// after a rolled-back `nextval`, `lastval` reverts to its pre-transaction state. (The PG-agreeing
/// `lastval` *values* — tracking the most recent `nextval`, reflecting a `setval` on that same
/// sequence — live in the oracle corpus; this asserts only the rollback, which the corpus cannot.)
#[test]
fn lastval_rolls_back() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE a START 100").unwrap();
    execute(&mut db, "CREATE SEQUENCE b START 200").unwrap();
    one_int(&mut db, "SELECT nextval('a')"); // committed: lastval → a's 100
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(100));

    execute(&mut db, "BEGIN").unwrap();
    one_int(&mut db, "SELECT nextval('b')"); // working: lastval → b's 200
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(200));
    execute(&mut db, "ROLLBACK").unwrap();

    // The in-transaction nextval('b') vanished, so lastval reverts to a's committed 100.
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(100));
}

/// A non-RESTART `ALTER SEQUENCE` action is `0A000` in jed (only RESTART is supported this slice) —
/// a divergence from PostgreSQL, where `ALTER SEQUENCE … INCREMENT BY` is valid, so it cannot live
/// in the PG-clean oracle corpus.
#[test]
fn alter_non_restart_is_0a000() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s").unwrap();
    assert_eq!(
        err_code(&mut db, "ALTER SEQUENCE s INCREMENT BY 2"),
        "0A000"
    );
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s OWNED BY t.c"), "0A000");
    // ALTER of a non-sequence object is not a known statement at all → 42601 (no escape hatch).
    assert_eq!(err_code(&mut db, "ALTER TABLE t ADD COLUMN c i32"), "42601");
}

/// `setval`/`ALTER … RESTART` are writes — a READ ONLY transaction rejects each with 25006 (each in
/// its own block, since the error poisons the block). `lastval`/`currval` (pure reads) are allowed.
#[test]
fn setval_alter_in_read_only_is_25006() {
    let mut db = Database::new();
    execute(&mut db, "CREATE SEQUENCE s").unwrap();
    one_int(&mut db, "SELECT nextval('s')"); // 1, defines session state

    execute(&mut db, "BEGIN READ ONLY").unwrap();
    assert_eq!(err_code(&mut db, "SELECT setval('s', 5)"), "25006");
    execute(&mut db, "ROLLBACK").unwrap();

    execute(&mut db, "BEGIN READ ONLY").unwrap();
    assert_eq!(err_code(&mut db, "ALTER SEQUENCE s RESTART"), "25006");
    execute(&mut db, "ROLLBACK").unwrap();

    // lastval is allowed in a read-only block (it mutates nothing).
    execute(&mut db, "BEGIN READ ONLY").unwrap();
    assert_eq!(one_int(&mut db, "SELECT lastval()"), Some(1));
    execute(&mut db, "ROLLBACK").unwrap();
}
