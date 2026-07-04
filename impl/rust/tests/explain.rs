//! EXPLAIN behaviours the shared corpus cannot express (spec/design/explain.md): privilege
//! delegation to the inner statement, the read/write classification via a READ ONLY transaction, that
//! ANALYZE of a write persists, and the EXPLAIN-owns-its-render-cost invariant. The plan RENDERING
//! itself is asserted in the corpus (query/explain*.test, dml/explain_dml.test), which runs on every
//! core, so it is deliberately NOT re-tested here (CLAUDE.md §10 — corpus by default).

use jed::{
    CreateOptions, Database, Outcome, Privilege, PrivilegeSet, Session, SessionOptions, Value,
};

fn ok(db: &mut Session, sql: &str) -> Outcome {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("expected ok from {sql}, got {}: {}", e.code(), e.message))
}

fn code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
        .code()
        .to_string()
}

/// The single integer of a scalar `SELECT` result.
fn scalar_int(db: &mut Session, sql: &str) -> i64 {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref v => panic!("expected an Int from {sql}, got {}", v.render()),
        },
        _ => panic!("expected a query result from {sql}"),
    }
}

/// EXPLAIN requires the INNER statement's privileges (EXPLAIN INSERT needs INSERT), matching PG —
/// even though plain EXPLAIN never executes.
#[test]
fn explain_delegates_inner_privileges() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    db.set_default_privileges(PrivilegeSet::EMPTY.with(Privilege::Select));
    ok(&mut db, "EXPLAIN SELECT v FROM t"); // SELECT privilege is held
    for sql in [
        "EXPLAIN INSERT INTO t VALUES (2, 20)",
        "EXPLAIN UPDATE t SET v = 0",
        "EXPLAIN DELETE FROM t",
    ] {
        assert_eq!(code(&mut db, sql), "42501", "{sql}: want 42501");
    }
}

/// Plain EXPLAIN of a write is a READ (it never mutates), so it is allowed in a READ ONLY
/// transaction; EXPLAIN ANALYZE of a write IS a write and is rejected 25006.
#[test]
fn explain_write_classification() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    ok(&mut db, "BEGIN READ ONLY");
    ok(&mut db, "EXPLAIN DELETE FROM t"); // a read — allowed in a read-only transaction
    assert_eq!(
        code(&mut db, "EXPLAIN ANALYZE DELETE FROM t"),
        "25006",
        "EXPLAIN ANALYZE DELETE in READ ONLY: want 25006"
    );
    ok(&mut db, "ROLLBACK");
}

/// Plain EXPLAIN of a DELETE does not mutate; EXPLAIN ANALYZE of an INSERT does (and persists).
#[test]
fn explain_analyze_executes_writes() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    ok(&mut db, "INSERT INTO t VALUES (2, 20)");
    ok(&mut db, "EXPLAIN DELETE FROM t"); // plan-only — deletes nothing
    assert_eq!(
        scalar_int(&mut db, "SELECT count(*) FROM t"),
        2,
        "plain EXPLAIN DELETE mutated"
    );
    ok(&mut db, "EXPLAIN ANALYZE INSERT INTO t VALUES (3, 30)"); // executes
    assert_eq!(
        scalar_int(&mut db, "SELECT count(*) FROM t"),
        3,
        "EXPLAIN ANALYZE INSERT did not persist"
    );
}

/// The EXPLAIN statement's OWN cost is one row_produced per emitted plan row — independent of the
/// (larger) inner cost reported inside the Analyze root.
#[test]
fn explain_owns_render_cost() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    ok(&mut db, "INSERT INTO t VALUES (2, 20)");
    let out = ok(&mut db, "EXPLAIN ANALYZE SELECT * FROM t");
    let (rows, cost) = match &out {
        Outcome::Query { rows, cost, .. } => (rows, *cost),
        _ => panic!("expected a query result"),
    };
    assert_eq!(
        cost,
        rows.len() as i64,
        "EXPLAIN render cost {cost} != plan-row count {}",
        rows.len()
    );
    // The Analyze root (row 0) reports the inner cost, which exceeds the render cost here.
    assert_eq!(
        rows[0][1].render(),
        "Analyze",
        "root node should be Analyze"
    );
}
