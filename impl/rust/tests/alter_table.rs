//! Byte-level ALTER TABLE rewrite checks that the shared SQL corpus cannot express.

use jed::{CreateOptions, Database, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

#[test]
fn add_column_rewrite_matches_fresh_table_bytes() {
    let mut altered = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut altered, "CREATE TABLE t (id i32 PRIMARY KEY)");
    run(&mut altered, "INSERT INTO t VALUES (1), (2)");
    run(&mut altered, "ALTER TABLE t ADD v i32 DEFAULT 7");

    let mut fresh = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut fresh,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)",
    );
    run(&mut fresh, "INSERT INTO t (id) VALUES (1), (2)");

    assert_eq!(
        altered.to_image(8192, 1).unwrap(),
        fresh.to_image(8192, 1).unwrap()
    );
}

#[test]
fn drop_column_rewrite_matches_fresh_table_bytes() {
    let mut altered = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut altered,
        "CREATE TABLE t (obsolete text, id i32 PRIMARY KEY, v i32 DEFAULT 7)",
    );
    run(
        &mut altered,
        "INSERT INTO t VALUES ('a', 1, 7), ('b', 2, 8)",
    );
    run(&mut altered, "ALTER TABLE t DROP obsolete");

    let mut fresh = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut fresh,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)",
    );
    run(&mut fresh, "INSERT INTO t VALUES (1, 7), (2, 8)");

    assert_eq!(
        altered.to_image(8192, 1).unwrap(),
        fresh.to_image(8192, 1).unwrap()
    );
}
