//! Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
//! minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
//! connectives, operator precedence, and parentheses. These complement the conformance
//! corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

use jed::value::Value;
use jed::{Database, Outcome, Session, SessionOptions};

fn db_with(stmts: &[&str]) -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    for s in stmts {
        db.execute(s, &[]).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and return its rows.
fn query(db: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql, &[]).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

/// Run a single-row, single-column query and return the lone value.
fn scalar(db: &mut Session, sql: &str) -> Value {
    let rows = query(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?}: expected one row");
    assert_eq!(rows[0].len(), 1, "{sql:?}: expected one column");
    rows[0][0].clone()
}

#[test]
fn comparisons_project_booleans() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)",
        "INSERT INTO t VALUES (1, 5, 5)",
        "INSERT INTO t VALUES (2, 5, 9)",
        "INSERT INTO t VALUES (3, 5, NULL)",
    ]);
    // true / false / NULL (unknown, from the NULL comparison).
    assert_eq!(
        query(&mut db, "SELECT a = b FROM t ORDER BY id"),
        vec![
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Null],
        ]
    );
    // literals render canonically.
    assert_eq!(
        scalar(&mut db, "SELECT TRUE FROM t WHERE id = 1"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut db, "SELECT FALSE FROM t WHERE id = 1"),
        Value::Bool(false)
    );
    assert_eq!(Value::Bool(true).render(), "true");
    assert_eq!(Value::Bool(false).render(), "false");
}
