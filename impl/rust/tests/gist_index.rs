//! GiST indexes (spec/design/gist.md) — GX1: `CREATE INDEX … USING gist` over a range column, its
//! maintenance, the planner `&&`/`@>` gather (descending the resident R-tree), and file persistence
//! (the page-5/6 R-tree, format_version 20, the close/reopen round-trip). Covers what the corpus
//! cannot: the deliberate divergences (UNIQUE/multi-column/temp → 0A000), introspection (DROP), and
//! the on-disk round-trip. Mirrored in Go/TS by the shared conformance corpus + each core's harness.

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) -> Outcome {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
}

fn ids(db: &mut Session, sql: &str) -> Vec<i64> {
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

fn err_code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

/// A table of i32 ranges, one per row, plus a NULL-range and an empty-range row.
fn ranges_db() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            page_size: 256,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
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
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
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
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
        assert_eq!(
            ids(&mut db, "SELECT id FROM t WHERE room = 20 ORDER BY id"),
            vec![2, 5, 7]
        );
    }
}

#[test]
fn gist_unique_and_multicolumn_are_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            page_size: 256,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
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
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
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
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
        assert_eq!(
            ids(
                &mut db,
                "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"
            ),
            vec![3, 7]
        );
    }
}

// ---- GX3: EXCLUDE constraints (spec/design/gist.md §7) -----------------------------------------

/// The canonical no-double-booking constraint: `EXCLUDE USING gist (room WITH =, during WITH &&)`
/// — no two rows may share a `room` AND have overlapping `during`. Needs the scalar `=` opclass
/// (GX2) for `room` and `range_ops` (GX1) for `during`.
fn booking_db() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, \
         EXCLUDE USING gist (room WITH =, during WITH &&))",
    );
    db
}

#[test]
fn exclude_rejects_conflicting_and_admits_compatible() {
    let mut db = booking_db();
    run(&mut db, "INSERT INTO booking VALUES (1, 101, '[10,20)')");
    // Same room, overlapping range → 23P01.
    assert_eq!(
        err_code(&mut db, "INSERT INTO booking VALUES (2, 101, '[15,25)')"),
        "23P01"
    );
    // Same room, NON-overlapping range → ok.
    run(&mut db, "INSERT INTO booking VALUES (2, 101, '[20,30)')");
    // Different room, overlapping range → ok (the conjunction needs BOTH).
    run(&mut db, "INSERT INTO booking VALUES (3, 102, '[10,20)')");
    // The two compatible rows landed; the conflicting one did not.
    assert_eq!(
        ids(&mut db, "SELECT id FROM booking ORDER BY id"),
        vec![1, 2, 3]
    );
}

/// Updating a range column on an EXCLUDE-constrained table re-checks the constraint over the
/// statement's end state (the GX3 + dml.update_container integration): a reschedule to a free slot
/// succeeds; one that newly overlaps a same-room booking traps 23P01; moving to a different room
/// clears the conflict. Needs the multi-column GiST index (PG needs btree_gist), so it lives here.
#[test]
fn exclude_reschedule_via_update() {
    let mut db = booking_db();
    run(
        &mut db,
        "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[30,40)')",
    );
    run(
        &mut db,
        "UPDATE booking SET during = '[50,60)' WHERE id = 1",
    );
    assert_eq!(
        err_code(
            &mut db,
            "UPDATE booking SET during = '[35,45)' WHERE id = 1"
        ),
        "23P01"
    );
    run(
        &mut db,
        "UPDATE booking SET room = 102, during = '[35,45)' WHERE id = 1",
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM booking ORDER BY id"),
        vec![1, 2]
    );
}

#[test]
fn exclude_null_and_empty_range_are_exempt() {
    let mut db = booking_db();
    run(&mut db, "INSERT INTO booking VALUES (1, 101, '[10,20)')");
    // A NULL room is exempt (the NULL rule) — never conflicts, even with an overlapping range.
    run(&mut db, "INSERT INTO booking VALUES (2, NULL, '[10,20)')");
    run(&mut db, "INSERT INTO booking VALUES (3, NULL, '[10,20)')");
    // An empty range is exempt (empty && anything is FALSE) — same room, but never conflicts.
    run(&mut db, "INSERT INTO booking VALUES (4, 101, 'empty')");
    run(&mut db, "INSERT INTO booking VALUES (5, 101, 'empty')");
    assert_eq!(
        ids(&mut db, "SELECT id FROM booking ORDER BY id"),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn exclude_in_batch_insert_conflict() {
    let mut db = booking_db();
    // Two rows in the SAME insert batch that conflict with each other → 23P01, nothing written.
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[15,25)')"
        ),
        "23P01"
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM booking ORDER BY id"),
        Vec::<i64>::new()
    );
}

#[test]
fn exclude_update_end_state_swap_succeeds() {
    let mut db = booking_db();
    run(&mut db, "INSERT INTO booking VALUES (1, 101, '[10,20)')");
    run(&mut db, "INSERT INTO booking VALUES (2, 102, '[10,20)')");
    // Swap the two rooms in one statement: the per-row transient collides (both briefly 101/102),
    // but the END STATE is conflict-free → succeeds (the documented UNIQUE end-state divergence).
    run(
        &mut db,
        "UPDATE booking SET room = CASE WHEN room = 101 THEN 102 ELSE 101 END",
    );
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM booking WHERE room = 102 ORDER BY id"
        ),
        vec![1]
    );
    // An UPDATE that creates a genuine conflict → 23P01: after the swap row1=(102,[10,20)),
    // row2=(101,[10,20)); moving row1 back to room 101 collides with row2 (same room, overlap).
    assert_eq!(
        err_code(&mut db, "UPDATE booking SET room = 101 WHERE id = 1"),
        "23P01"
    );
}

#[test]
fn single_column_range_exclude() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE rsv (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))",
    );
    run(&mut db, "INSERT INTO rsv VALUES (1, '[1,5)')");
    assert_eq!(
        err_code(&mut db, "INSERT INTO rsv VALUES (2, '[3,8)')"),
        "23P01"
    );
    run(&mut db, "INSERT INTO rsv VALUES (2, '[5,10)')"); // adjacent, not overlapping → ok
    assert_eq!(ids(&mut db, "SELECT id FROM rsv ORDER BY id"), vec![1, 2]);
}

#[test]
fn exclude_type_errors() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // `&&` over a non-range column → 42704 (no range_ops opclass for it).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE a (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH &&))"
        ),
        "42704"
    );
    // `=` over a deferred keyable (text) → 0A000.
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE b (id i32 PRIMARY KEY, s text, EXCLUDE USING gist (s WITH =))"
        ),
        "0A000"
    );
    // `=` over a no-opclass type (f64) → 42704.
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE c (id i32 PRIMARY KEY, f f64, EXCLUDE USING gist (f WITH =))"
        ),
        "42704"
    );
    // An unsupported operator → 0A000.
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE d (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH <))"
        ),
        "0A000"
    );
    // EXCLUDE on a TEMP table → 0A000 (the GiST-on-temp narrowing).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TEMP TABLE e (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))"
        ),
        "0A000"
    );
}

#[test]
fn exclude_backing_index_cannot_be_dropped() {
    let mut db = booking_db();
    // The backing GiST index shares the constraint's auto-name `<table>_<cols>_excl`.
    assert_eq!(
        err_code(&mut db, "DROP INDEX booking_room_during_excl"),
        "2BP01"
    );
}

/// An EXCLUDE constraint's backing multi-column GiST index persists (page-5/6 R-tree, format_version
/// 21) and reloads, still enforcing the conjunction across a close/reopen.
#[test]
fn exclude_file_backed_round_trip() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gist_exclude_round_trip.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            page_size: 256,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
        run(
            &mut db,
            "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, \
             EXCLUDE USING gist (room WITH =, during WITH &&))",
        );
        for (id, room, lit) in [
            (1, 101, "'[10,20)'"),
            (2, 101, "'[20,30)'"),
            (3, 102, "'[10,20)'"),
        ] {
            run(
                &mut db,
                &format!("INSERT INTO booking VALUES ({id}, {room}, {lit})"),
            );
        }
        db.commit().unwrap();
    }
    {
        let mut db = Database::open(&path)
            .unwrap()
            .session(SessionOptions::default());
        // The persisted constraint still rejects a conflict after reopen.
        assert_eq!(
            err_code(&mut db, "INSERT INTO booking VALUES (4, 101, '[15,25)')"),
            "23P01"
        );
        // And a compatible row is still admitted.
        run(&mut db, "INSERT INTO booking VALUES (4, 103, '[10,20)')");
        assert_eq!(
            ids(&mut db, "SELECT id FROM booking ORDER BY id"),
            vec![1, 2, 3, 4]
        );
    }
}
