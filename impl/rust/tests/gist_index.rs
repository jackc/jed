//! GiST indexes (spec/design/gist.md) — GX1: `CREATE INDEX … USING gist` over a range column, its
//! maintenance, the planner `&&`/`@>` gather (descending the resident R-tree), and file persistence
//! (the page-5/6 R-tree, format_version 20, the close/reopen round-trip). Covers what the corpus
//! cannot: the deliberate divergences (UNIQUE/multi-column/temp → 0A000), introspection (DROP), and
//! the on-disk round-trip. Mirrored in Go/TS by the shared conformance corpus + each core's harness.

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

/// GX1 narrowing (gist.md §11): a GiST index on a TEMP table is 0A000 — its resident R-tree would
/// live on the temp/shared-temp snapshot (deferred). It fails closed rather than silently dropping
/// the acceleration. (File persistence, by contrast, landed in GX1b — see the round-trip below.)
#[test]
fn gist_on_temp_table_is_0a000() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TEMP TABLE t (id i32 PRIMARY KEY, r i32range)",
    );
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING gist (r)"),
        "0A000"
    );
}

/// GX1b: a GiST index persists to the page-5/6 R-tree (format_version 20) and reloads correctly —
/// the index survives a close/reopen and still accelerates `&&`/`@>` to the same rows. Exercises the
/// serialize path (commit), the demand-paged load path (`open` → `open_paged`'s eager GiST load),
/// and the resident-tree rebuild on open. Maintenance after reopen is also covered.
#[test]
fn gist_file_backed_round_trip() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gist_round_trip.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
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
        // Accelerated query before close.
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"
            ),
            vec![1, 3]
        );
    }
    // Reopen: the persisted R-tree loads, the resident tree is rebuilt, the query still works.
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"
            ),
            vec![1, 3]
        );
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id"
            ),
            vec![1, 3]
        );
        // Maintenance after reopen: a fresh INSERT updates the (loaded) index, the next query sees it.
        run(&mut db, "INSERT INTO t VALUES (7, '[5,7)')");
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"
            ),
            vec![3, 7]
        );
    }
    // And once more, after the maintenance commit, to prove the rewritten tree persists.
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"
            ),
            vec![3, 7]
        );
    }
}
