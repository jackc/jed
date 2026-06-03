//! Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
//! and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

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

fn ids(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|r| r[0].clone()).collect()
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, 30)",
        "INSERT INTO t VALUES (4, NULL)",
    ])
}

#[test]
fn delete_by_predicate_removes_only_matching() {
    let mut db = setup();
    execute(&mut db, "DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id")),
        vec![Value::Int(1), Value::Int(3), Value::Int(4)]
    );
}

#[test]
fn delete_no_where_clears_all() {
    let mut db = setup();
    execute(&mut db, "DELETE FROM t").unwrap();
    assert!(query(&mut db, "SELECT id FROM t ORDER BY id").is_empty());
}

#[test]
fn delete_is_three_valued_only_true_matches() {
    // `v > 100` is FALSE for the present rows and UNKNOWN for the NULL row — nothing
    // is deleted.
    let mut db = setup();
    execute(&mut db, "DELETE FROM t WHERE v > 100").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM t ORDER BY id").len(), 4);
    // IS NULL removes only the NULL row.
    execute(&mut db, "DELETE FROM t WHERE v IS NULL").unwrap();
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id")),
        vec![Value::Int(1), Value::Int(2), Value::Int(3)]
    );
}

#[test]
fn delete_then_insert_no_pk_does_not_collide() {
    // The bug this fixes: a no-PK table keyed rows on `store.len()`, so after a delete
    // the next insert reused a rowid and tripped a spurious 23505.
    let mut db = db_with(&[
        "CREATE TABLE log (n int32)",
        "INSERT INTO log VALUES (100)",
        "INSERT INTO log VALUES (200)",
        "INSERT INTO log VALUES (300)",
        "DELETE FROM log WHERE n = 200",
    ]);
    // This insert must succeed (it would have collided under the old len()-based id).
    execute(&mut db, "INSERT INTO log VALUES (400)").expect("insert after delete");
    assert_eq!(
        ids(query(&mut db, "SELECT n FROM log ORDER BY n")),
        vec![Value::Int(100), Value::Int(300), Value::Int(400)]
    );
}

#[test]
fn delete_from_missing_table_traps() {
    let mut db = Database::new();
    assert_eq!(
        execute(&mut db, "DELETE FROM nope").unwrap_err().code(),
        "42P01"
    );
}

#[test]
fn delete_unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "DELETE FROM t WHERE nope = 1")
            .unwrap_err()
            .code(),
        "42703"
    );
}
