//! Boolean as a key (spec/design/types.md §9, encoding.md §2.9) — boolean is the second
//! non-integer key type after uuid. Its `bool-byte` key (0x00 false < 0x01 true) drives a
//! boolean PRIMARY KEY, a boolean member of a composite key, and a secondary index on a
//! boolean column. The byte-exact stored key is pinned cross-core by `bool_pk_table.jed`
//! (tests/fileformat_golden.rs); these are the behavioral checks.

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn err_code(db: &mut Database, sql: &str) -> String {
    match execute(db, sql) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected query, got {other:?}"),
    }
}

/// A boolean PRIMARY KEY is accepted (the gate lifted) and CRUD works.
#[test]
fn boolean_primary_key_crud() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (k boolean PRIMARY KEY, v int32)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (FALSE, 10), (TRUE, 20)").unwrap();

    // Point lookup on the boolean PK resolves to the right row.
    assert_eq!(
        rows(&mut db, "SELECT v FROM t WHERE k = TRUE"),
        vec![vec![Value::Int(20)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT v FROM t WHERE k = FALSE"),
        vec![vec![Value::Int(10)]]
    );

    // A full scan iterates in key (byte) order: false (0x00) before true (0x01).
    assert_eq!(
        rows(&mut db, "SELECT k FROM t"),
        vec![vec![Value::Bool(false)], vec![Value::Bool(true)]]
    );
}

/// A duplicate boolean PK is a 23505 unique violation (only two keys are possible).
#[test]
fn boolean_primary_key_duplicate() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (k boolean PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (TRUE)").unwrap();
    assert_eq!(err_code(&mut db, "INSERT INTO t VALUES (TRUE)"), "23505");
    // The other value still inserts.
    execute(&mut db, "INSERT INTO t VALUES (FALSE)").unwrap();
}

/// A NULL boolean PK is rejected NOT NULL (23502), like any PK.
#[test]
fn boolean_primary_key_null_rejected() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (k boolean PRIMARY KEY)").unwrap();
    assert_eq!(err_code(&mut db, "INSERT INTO t VALUES (NULL)"), "23502");
}

/// A boolean member of a COMPOSITE primary key concatenates with the other component.
#[test]
fn boolean_composite_primary_key() {
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (a int32, b boolean, v int32, PRIMARY KEY (a, b))",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, TRUE, 10), (1, FALSE, 20), (2, FALSE, 30)",
    )
    .unwrap();
    // (1,FALSE) and (1,TRUE) are distinct keys; the same (a,b) again conflicts.
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, TRUE, 99)"),
        "23505"
    );
    // Key order: a ascending, then b false<true within an a-group.
    assert_eq!(
        rows(&mut db, "SELECT a, b FROM t"),
        vec![
            vec![Value::Int(1), Value::Bool(false)],
            vec![Value::Int(1), Value::Bool(true)],
            vec![Value::Int(2), Value::Bool(false)],
        ]
    );
}

/// A secondary index on a (nullable) boolean column is accepted and serves equality.
#[test]
fn boolean_secondary_index() {
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, flag boolean)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL), (4, TRUE)",
    )
    .unwrap();
    execute(&mut db, "CREATE INDEX i ON t (flag)").unwrap();
    let mut ids: Vec<i64> = rows(&mut db, "SELECT id FROM t WHERE flag = TRUE")
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!("expected int"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 4]);
}
