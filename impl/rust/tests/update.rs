//! Step 6: UPDATE — in-place value replacement, old-row assignment semantics, the
//! two-phase all-or-nothing guarantee, and the rejected cases (PK column, duplicate
//! target, overflow, not-null).

use jed::{Database, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, a i16, b i16)",
        "INSERT INTO t VALUES (1, 10, 11)",
        "INSERT INTO t VALUES (2, 20, 22)",
        "INSERT INTO t VALUES (3, 30, 33)",
    ])
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
