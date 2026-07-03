//! DROP TABLE — remove a table (its definition + all its rows) from the catalog. The
//! inverse of CREATE TABLE: a missing table is 42P01 (or a no-op under IF EXISTS)
//! (spec/design/grammar.md §13). These cover the single-table internals (catalog/row-store
//! removal, re-create-after-drop, case-insensitivity); the IF EXISTS, multi-table
//! (`DROP TABLE a, b`), and CASCADE/RESTRICT behaviors all agree with PostgreSQL and live in
//! the corpus (suites/ddl/drop_table.test).

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) -> jed::Result<Outcome> {
    db.execute(sql, &[])
}

#[test]
fn drop_removes_table_and_rows() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)").unwrap();
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    assert!(db.table("t").is_some());

    let out = run(&mut db, "DROP TABLE t").unwrap();
    assert_eq!(
        out,
        Outcome::Statement {
            cost: 0,
            rows_affected: None
        }
    );
    assert!(db.table("t").is_none(), "catalog entry gone");
    assert!(db.rows_in_key_order("t").is_none(), "row store gone");
}

#[test]
fn name_is_free_to_recreate_after_drop() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)").unwrap();
    run(&mut db, "INSERT INTO t VALUES (1, 10)").unwrap();
    run(&mut db, "DROP TABLE t").unwrap();
    // Re-create the freed name with a different shape; the new table starts empty.
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, w i64)").unwrap();
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 0);
    assert_eq!(db.table("t").unwrap().columns[1].name, "w");
}

#[test]
fn drop_is_case_insensitive() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "create table T (id i32 primary key)").unwrap();
    run(&mut db, "DROP TABLE t").unwrap();
    assert!(db.table("t").is_none());
}

#[test]
fn drop_leaves_other_tables_intact() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE a (id i32 PRIMARY KEY)").unwrap();
    run(&mut db, "CREATE TABLE b (id i32 PRIMARY KEY)").unwrap();
    run(&mut db, "INSERT INTO b VALUES (2)").unwrap();
    run(&mut db, "DROP TABLE a").unwrap();
    assert!(db.table("a").is_none());
    assert!(db.table("b").is_some());
    assert_eq!(db.rows_in_key_order("b").unwrap().len(), 1);
}

#[test]
fn syntax_errors_are_reported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A bare DROP TABLE with no name.
    assert_eq!(run(&mut db, "DROP TABLE").unwrap_err().code(), "42601");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap();
    // Trailing input after the table name.
    assert_eq!(
        run(&mut db, "DROP TABLE t extra").unwrap_err().code(),
        "42601"
    );
    // DROP INDEX is its own statement now (spec/design/indexes.md §2): a missing index is
    // 42704, not a syntax error; DROP of any other object kind is still unparsed.
    assert_eq!(run(&mut db, "DROP INDEX x").unwrap_err().code(), "42704");
    assert_eq!(run(&mut db, "DROP VIEW v").unwrap_err().code(), "42601");
}
