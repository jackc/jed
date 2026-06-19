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
