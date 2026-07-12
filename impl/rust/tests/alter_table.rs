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

#[test]
fn type_and_primary_key_rewrites_match_fresh_table_bytes() {
    let image = |setup: &[&str]| {
        let mut db = Database::create(CreateOptions::default())
            .unwrap()
            .session(SessionOptions::default());
        for sql in setup {
            run(&mut db, sql);
        }
        db.to_image(8192, 1).unwrap()
    };

    assert_eq!(
        image(&[
            "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
            "INSERT INTO t VALUES (1, 2), (2, 3)",
            "ALTER TABLE t ALTER v TYPE i64 USING v + 10",
        ]),
        image(&[
            "CREATE TABLE t (id i32 PRIMARY KEY, v i64)",
            "INSERT INTO t VALUES (1, 12), (2, 13)",
        ]),
    );
    let altered = image(&[
        "CREATE TABLE t (id i32 NOT NULL, v text)",
        "INSERT INTO t VALUES (2, 'b'), (1, 'a')",
        "ALTER TABLE t ADD PRIMARY KEY (id)",
        "ALTER TABLE t DROP PRIMARY KEY",
    ]);
    let fresh = image(&[
        "CREATE TABLE t (id i32 NOT NULL, v text)",
        "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
    ]);
    assert_eq!(altered, fresh);
}
