//! Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
//! and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

use jed::{Engine, execute};

fn db_with(stmts: &[&str]) -> Engine {
    let mut db = Engine::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn setup() -> Engine {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, 30)",
        "INSERT INTO t VALUES (4, NULL)",
    ])
}

#[test]
fn delete_from_missing_table_traps() {
    let mut db = Engine::new();
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
