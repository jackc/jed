//! GiST indexes (spec/design/gist.md) — GX1a: in-memory `CREATE INDEX … USING gist` over a range
//! column, its maintenance, and the DDL validation surface. Covers what the corpus cannot yet (the
//! deliberate GX1a narrowings — file-backed persistence is GX1b, and the planner gather is a
//! follow-on, so a query is correct by full scan + residual filter here). Mirrored in Go/TS when
//! those cores land the feature.

use std::path::PathBuf;

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};

fn run(db: &mut Database, sql: &str) -> Outcome {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
}

fn ids(db: &mut Database, sql: &str) -> Vec<i64> {
    match run(db, sql) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref v => panic!("expected an int id, got {v:?}"),
            })
            .collect(),
        _ => panic!("expected a query outcome"),
    }
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

/// A table of i32 ranges, one per row, plus a NULL-range and an empty-range row.
fn ranges_db() -> Database {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    run(&mut db, "CREATE INDEX t_r_gist ON t USING gist (r)");
    for (id, lit) in [
        (1, "'[1,5)'"),
        (2, "'[10,20)'"),
        (3, "'[3,8)'"),
        (4, "'[100,200)'"),
        (5, "'empty'"),
    ] {
        run(&mut db, &format!("INSERT INTO t VALUES ({id}, {lit})"));
    }
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

#[test]
fn create_gist_index_and_query_overlap_and_contains() {
    let mut db = ranges_db();
    // && (overlap) [4,6): [1,5) and [3,8) overlap; [10,20)/[100,200) don't; empty/NULL never do.
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"
        ),
        vec![1, 3]
    );
    // @> (contains) [4,5)={4}: both [1,5) and [3,8) contain it; [10,20)/[100,200)/empty/NULL don't.
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id"
        ),
        vec![1, 3]
    );
    // A query that hits the high cluster.
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r && i32range(150,160) ORDER BY id"
        ),
        vec![4]
    );
    // Maintenance: DELETE removes the row's index entry, then re-query.
    run(&mut db, "DELETE FROM t WHERE id = 3");
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"
        ),
        vec![1]
    );
    // A fresh INSERT adds an entry the next query sees.
    run(&mut db, "INSERT INTO t VALUES (7, '[5,12)')");
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"
        ),
        vec![7]
    );
}

#[test]
fn drop_gist_index() {
    let mut db = ranges_db();
    run(&mut db, "DROP INDEX t_r_gist");
    // The table + rows survive; the query is still correct (full scan).
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"
        ),
        vec![1, 3]
    );
}

#[test]
fn gist_on_non_range_column_is_42704() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)");
    // No default operator class for access method gist on a non-range type.
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING gist (n)"),
        "42704"
    );
}

#[test]
fn gist_unique_and_multicolumn_are_0a000() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, s i32range)",
    );
    assert_eq!(
        err_code(&mut db, "CREATE UNIQUE INDEX ON t USING gist (r)"),
        "0A000"
    );
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING gist (r, s)"),
        "0A000"
    );
}

#[test]
fn gist_unknown_access_method_is_42704() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING brin (r)"),
        "42704"
    );
}

/// GX1a narrowing: a GiST index on a file-backed database is 0A000 (persistence is GX1b — the
/// page-5/6 R-tree + format_version 20). In-memory works; a file DB fails closed and discoverably
/// rather than writing an index_kind it cannot read back.
#[test]
fn gist_on_file_backed_db_is_0a000() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gist_file_gate.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING gist (r)"),
        "0A000"
    );
}
