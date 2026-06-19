//! Phase D/E: SELECT — projection, WHERE (=, ordering ops, IS [NOT] NULL),
//! three-valued logic, ORDER BY (NULLs last), and CAST. These complement the
//! conformance corpus with finer-grained per-feature assertions.

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and return its rows as nested Value vectors.
fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, NULL)",
    ])
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|r| r[0].clone()).collect()
}

#[test]
fn limit_caps_and_offset_skips() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, 30)",
        "INSERT INTO t VALUES (4, 40)",
        "INSERT INTO t VALUES (5, 50)",
    ]);
    // LIMIT takes the first n; OFFSET skips; the two clauses commute.
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2")),
        vec![Value::Int(1), Value::Int(2)]
    );
    assert_eq!(
        ids(query(
            &mut db,
            "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1"
        )),
        vec![Value::Int(2), Value::Int(3)]
    );
    assert_eq!(
        ids(query(
            &mut db,
            "SELECT id FROM t ORDER BY id OFFSET 1 LIMIT 2"
        )),
        vec![Value::Int(2), Value::Int(3)]
    );
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id OFFSET 3")),
        vec![Value::Int(4), Value::Int(5)]
    );
    // LIMIT 0 and an OFFSET past the end are empty (not errors); a huge LIMIT clamps.
    assert!(query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 0").is_empty());
    assert!(query(&mut db, "SELECT id FROM t ORDER BY id OFFSET 10").is_empty());
    assert_eq!(
        query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 100").len(),
        5
    );
}

#[test]
fn limit_offset_window_reduces_produced_cost() {
    // The slice runs before projection, so only windowed rows charge row_produced:
    // 1 page_read (t is one leaf) + 5 scanned + 2 produced = 8 (spec/design/cost.md §3).
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (3)",
        "INSERT INTO t VALUES (4)",
        "INSERT INTO t VALUES (5)",
    ]);
    let cost = execute(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2")
        .unwrap()
        .cost();
    assert_eq!(cost, 8);
}

#[test]
fn unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "SELECT nope FROM t").unwrap_err().code(),
        "42703"
    );
    assert_eq!(
        execute(&mut db, "SELECT id FROM t WHERE nope = 1")
            .unwrap_err()
            .code(),
        "42703"
    );
}
