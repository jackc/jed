//! Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
//! and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

use jed::{CreateOptions, Database, Session, SessionOptions};

fn db_with(stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in stmts {
        db.execute(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn setup() -> Session {
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.execute("DELETE FROM nope", &[]).unwrap_err().code(),
        "42P01"
    );
}

#[test]
fn delete_unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        db.execute("DELETE FROM t WHERE nope = 1", &[])
            .unwrap_err()
            .code(),
        "42703"
    );
}
