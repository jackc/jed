//! Correlated primary-key pushdown over a MULTI-LEAF inner table (spec/design/cost.md §3 "bounded
//! scan / correlated"). The conformance corpus
//! (spec/conformance/suites/subquery/correlated_pushdown.test) pins the cost contract on single-leaf
//! tables; this exercises what it cannot — an inner table wide enough that re-scanning it per outer
//! row would be visibly expensive, so the per-outer-row seek is the difference between sublinear and
//! quadratic. The win is shown by contrast: `inner.pk = o.col` (bounded) vs `inner.v = o.col` (a full
//! re-scan), which return the SAME rows because v == id.

use jed::value::Value;
use jed::{Database, Outcome, Session, SessionOptions};

/// `o` is a handful of outer rows; `inr` is `n` rows (id i32 PRIMARY KEY, v i32; v == id), wide
/// enough to span several leaves. The outer k-values are all present as inner ids.
fn tables(n: i64) -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE o (id i32 PRIMARY KEY, k i32)", &[])
        .unwrap();
    db.execute("CREATE TABLE inr (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO o VALUES (1, 100), (2, 300), (3, 500), (4, 700), (5, 900)",
        &[],
    )
    .unwrap();
    let mut sql = String::from("INSERT INTO inr VALUES ");
    for i in 1..=n {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!("({i},{i})"));
    }
    db.execute(&sql, &[]).unwrap();
    db
}

fn cost(db: &mut Session, sql: &str) -> i64 {
    match db.execute(sql, &[]).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

fn ids(db: &mut Session, sql: &str) -> Vec<i64> {
    match db.execute(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                _ => panic!("expected int"),
            })
            .collect(),
        Outcome::Statement { .. } => panic!("expected a query result"),
    }
}

#[test]
fn correlated_exists_seek_is_sublinear() {
    let mut db = tables(1000);
    // Both correlate the inner to each outer row; `inr.id` is the PK (seeks), `inr.v` is not (full
    // re-scan). v == id, so they select the SAME inner rows and the SAME outer rows survive.
    let bounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)";
    let unbounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.v = o.k)";
    assert_eq!(ids(&mut db, bounded), vec![1, 2, 3, 4, 5]);
    assert_eq!(ids(&mut db, unbounded), vec![1, 2, 3, 4, 5]);

    let seek = cost(&mut db, bounded);
    let scan = cost(&mut db, unbounded);
    // The non-PK correlation re-scans all ~1000 inner rows for each of the 5 outer rows; the PK
    // pushdown seeks instead, so it is an order of magnitude cheaper.
    assert!(
        seek * 10 < scan,
        "correlated seek {seek} should be far below the per-outer-row full re-scan {scan}"
    );
    // Sublinear in the inner size: 5 outer rows, each ≈ a point lookup (height + a row), not ~1000.
    assert!(
        seek <= 400,
        "correlated seek {seek} should be sublinear (≈ outer × tree height), not ~5000"
    );
}

#[test]
fn correlated_scalar_seek_matches_unbounded_rows() {
    let mut db = tables(1000);
    // A correlated SCALAR subquery seeking the inner PK returns each outer row's inner value (or NULL
    // for a miss). Rows are identical to what a full re-scan would produce; only the cost differs.
    let got = ids(
        &mut db,
        "SELECT (SELECT inr.v FROM inr WHERE inr.id = o.k) FROM o ORDER BY o.id",
    );
    assert_eq!(got, vec![100, 300, 500, 700, 900]);

    let seek = cost(
        &mut db,
        "SELECT (SELECT inr.v FROM inr WHERE inr.id = o.k) FROM o ORDER BY o.id",
    );
    let scan = cost(
        &mut db,
        "SELECT (SELECT inr.v FROM inr WHERE inr.v = o.k) FROM o ORDER BY o.id",
    );
    assert!(
        seek * 10 < scan,
        "correlated scalar seek {seek} should be far below the full re-scan {scan}"
    );
}

#[test]
fn correlated_miss_and_null_outer_seek_nothing() {
    let mut db = tables(1000);
    // An outer k with no matching inner id is a point-lookup miss (visits the leaf, reads no row); a
    // NULL outer k is a 3VL-empty bound (reads no page, no row). Neither re-scans the inner.
    db.execute("INSERT INTO o VALUES (6, 999999), (7, NULL)", &[])
        .unwrap();
    let got = ids(
        &mut db,
        "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)",
    );
    // ids 6 (miss) and 7 (NULL) do not survive; the original five do.
    assert_eq!(got, vec![1, 2, 3, 4, 5]);

    let seek = cost(
        &mut db,
        "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)",
    );
    assert!(
        seek <= 500,
        "seek cost {seek} should stay sublinear even with a miss and a NULL outer row"
    );
}
