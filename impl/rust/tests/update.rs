//! Step 6: UPDATE — in-place value replacement, old-row assignment semantics, the
//! two-phase all-or-nothing guarantee, and the rejected cases (PK column, duplicate
//! target, overflow, not-null).

use abide::value::Value;
use abide::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, a int16, b int16)",
        "INSERT INTO t VALUES (1, 10, 11)",
        "INSERT INTO t VALUES (2, 20, 22)",
        "INSERT INTO t VALUES (3, 30, 33)",
    ])
}

#[test]
fn update_one_row_by_key() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET a = 99 WHERE id = 2").unwrap();
    assert_eq!(
        query(&mut db, "SELECT a FROM t WHERE id = 2"),
        vec![vec![Value::Int(99)]]
    );
    // other rows untouched
    assert_eq!(
        query(&mut db, "SELECT a FROM t WHERE id = 1"),
        vec![vec![Value::Int(10)]]
    );
}

#[test]
fn update_swap_reads_old_row() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET a = b, b = a WHERE id = 1").unwrap();
    assert_eq!(
        query(&mut db, "SELECT a, b FROM t WHERE id = 1"),
        vec![vec![Value::Int(11), Value::Int(10)]]
    );
}

#[test]
fn update_no_where_touches_every_row() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET b = 0").unwrap();
    let rows = query(&mut db, "SELECT b FROM t ORDER BY id");
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(0)],
            vec![Value::Int(0)],
            vec![Value::Int(0)]
        ]
    );
}

#[test]
fn update_to_null_in_nullable_column() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET a = NULL WHERE id = 3").unwrap();
    assert_eq!(
        query(&mut db, "SELECT a FROM t WHERE id = 3"),
        vec![vec![Value::Null]]
    );
}

#[test]
fn update_primary_key_column_is_unsupported() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "UPDATE t SET id = 5 WHERE id = 2")
            .unwrap_err()
            .code(),
        "0A000"
    );
    // row unchanged
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE id = 2"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn update_duplicate_target_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "UPDATE t SET a = 1, a = 2 WHERE id = 1")
            .unwrap_err()
            .code(),
        "42701"
    );
}

#[test]
fn update_overflow_traps_and_row_unchanged() {
    let mut db = setup();
    // int16 max is 32767
    assert_eq!(
        execute(&mut db, "UPDATE t SET a = 40000 WHERE id = 2")
            .unwrap_err()
            .code(),
        "22003"
    );
    assert_eq!(
        query(&mut db, "SELECT a FROM t WHERE id = 2"),
        vec![vec![Value::Int(20)]]
    );
}

#[test]
fn update_column_source_rechecks_target_range() {
    // Assigning a wider column into a narrower one re-checks the TARGET range.
    let mut db = db_with(&[
        "CREATE TABLE w (id int32 PRIMARY KEY, small int16, big int64)",
        "INSERT INTO w VALUES (1, 5, 100000)",
    ]);
    assert_eq!(
        execute(&mut db, "UPDATE w SET small = big WHERE id = 1")
            .unwrap_err()
            .code(),
        "22003"
    );
    assert_eq!(
        query(&mut db, "SELECT small FROM w WHERE id = 1"),
        vec![vec![Value::Int(5)]]
    );
}

#[test]
fn update_is_all_or_nothing_across_rows() {
    // Row 2's source overflows int16, so NO row is modified — not even rows 1 and 3.
    let mut db = db_with(&[
        "CREATE TABLE m (id int32 PRIMARY KEY, n int16, src int64)",
        "INSERT INTO m VALUES (1, 1, 5)",
        "INSERT INTO m VALUES (2, 2, 99999)",
        "INSERT INTO m VALUES (3, 3, 7)",
    ]);
    assert_eq!(
        execute(&mut db, "UPDATE m SET n = src").unwrap_err().code(),
        "22003"
    );
    let rows = query(&mut db, "SELECT n FROM m ORDER BY id");
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ]
    );
}

#[test]
fn update_missing_table_traps() {
    let mut db = Database::new();
    assert_eq!(
        execute(&mut db, "UPDATE nope SET a = 1")
            .unwrap_err()
            .code(),
        "42P01"
    );
}

#[test]
fn update_unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "UPDATE t SET nope = 1")
            .unwrap_err()
            .code(),
        "42703"
    );
}
