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
fn gist_on_unsupported_type_is_42704() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, f f64)");
    // No GiST opclass at all for a non-keyable, non-range type (float) — 42704 (gist.md §6).
    assert_eq!(
        err_code(&mut db, "CREATE INDEX ON t USING gist (f)"),
        "42704"
    );
}

/// GX2 narrowing (gist.md §6/§11): the scalar `=` opclass ships the FIXED-WIDTH keyables first; a
/// keyable-but-deferred scalar (text/bytea/decimal/interval) is `0A000` ("not supported yet"), on
/// the roadmap like each GIN element type — NOT `42704` (which means no opclass exists at all).
#[test]
fn gist_on_deferred_scalar_is_0a000() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, s text, b bytea, d decimal, v interval)",
    );
    for col in ["s", "b", "d", "v"] {
        assert_eq!(
            err_code(&mut db, &format!("CREATE INDEX ON t USING gist ({col})")),
            "0A000",
            "column {col} should be 0A000 (deferred keyable)"
        );
    }
}

/// GX2: the scalar `=` opclass (the in-core `btree_gist`). A GiST index over a fixed-width keyable
/// scalar column accelerates `=` — the planner descends the resident R-tree and re-applies `=` as
/// the residual, so the rows are identical to a full scan (duplicates and all), across INSERT /
/// UPDATE / DELETE maintenance. The column is non-PK and has only a GiST index, so the GiST `=`
/// gather (not a PK/btree bound) is what fires.
#[test]
fn scalar_gist_equal_gather() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)");
    run(&mut db, "CREATE INDEX t_room_gist ON t USING gist (room)");
    for (id, room) in [(1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 10)] {
        run(&mut db, &format!("INSERT INTO t VALUES ({id}, {room})"));
    }
    run(&mut db, "INSERT INTO t VALUES (7, NULL)"); // a NULL room is not indexed
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 10 ORDER BY id"),
        vec![1, 3, 6]
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
        vec![2, 5]
    );
    // `= NULL` is 3VL-unknown → no rows; a value with no row → no rows.
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = NULL ORDER BY id"),
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 99 ORDER BY id"),
        Vec::<i64>::new()
    );
    // Maintenance: DELETE then re-query, INSERT then re-query, UPDATE the indexed column.
    run(&mut db, "DELETE FROM t WHERE id = 3");
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 10 ORDER BY id"),
        vec![1, 6]
    );
    run(&mut db, "INSERT INTO t VALUES (8, 10)");
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 10 ORDER BY id"),
        vec![1, 6, 8]
    );
    run(&mut db, "UPDATE t SET room = 20 WHERE id = 1");
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
        vec![1, 2, 5]
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE room = 10 ORDER BY id"),
        vec![6, 8]
    );
}

/// GX2: a scalar `=` GiST index persists (page-5/6 R-tree, format_version 20 — the scalar bound is a
/// `[min,max]` key blob, distinguished from a range bound by the indexed column's catalog type) and
/// reloads, still accelerating `=` to the same rows across a close/reopen + post-reopen maintenance.
#[test]
fn scalar_gist_file_backed_round_trip() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gist_scalar_round_trip.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
        run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)");
        run(&mut db, "CREATE INDEX t_room_gist ON t USING gist (room)");
        for (id, room) in [(1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 10)] {
            run(&mut db, &format!("INSERT INTO t VALUES ({id}, {room})"));
        }
        assert_eq!(
            ids(&mut db, "SELECT id FROM t WHERE room = 10 ORDER BY id"),
            vec![1, 3, 6]
        );
    }
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
            vec![2, 5]
        );
        run(&mut db, "INSERT INTO t VALUES (7, 20)");
        assert_eq!(
            ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
            vec![2, 5, 7]
        );
    }
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
            vec![2, 5, 7]
        );
    }
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
