//! Join base-table primary-key pushdown over a MULTI-LEAF table (spec/design/cost.md §3 "bounded
//! scan / JOIN"). The conformance corpus (spec/conformance/suites/joins/pushdown.test) pins the cost
//! contract on single-leaf tables; this exercises what it cannot — a join base table wide enough that
//! a full materialization would be expensive, so bounding it by its own primary key is the difference
//! between sublinear and a full double scan. The win is shown by contrast: `WHERE a.id = c` (a's PK,
//! bounded) vs `WHERE a.k = c` (not the PK, full scan), which return the SAME row because k == id.

use jed::value::Value;
use jed::{Database, Outcome, Session, SessionOptions};

/// `a` is `n` rows (id i32 PRIMARY KEY, k i32; k == id), wide enough to span several leaves; `b`
/// is three small rows whose k-values exist as a's k-values, so the join matches.
fn tables(n: i64) -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE a (id i32 PRIMARY KEY, k i32)", &[]).unwrap();
    db.execute("CREATE TABLE b (id i32 PRIMARY KEY, k i32)", &[]).unwrap();
    let mut sql = String::from("INSERT INTO a VALUES ");
    for i in 1..=n {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!("({i},{i})"));
    }
    db.execute(&sql, &[]).unwrap();
    db.execute("INSERT INTO b VALUES (1, 500), (2, 600), (3, 700)", &[]).unwrap();
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
fn join_pushdown_bounds_one_side_sublinear() {
    let mut db = tables(1000);
    // Both pick the single a row with id/k == 500 and join it to b(k=500); `a.id` is the PK (seeks a),
    // `a.k` is not (full scan of a). k == id, so they return the SAME row.
    let bounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500";
    let unbounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.k = 500";
    assert_eq!(ids(&mut db, bounded), vec![500]);
    assert_eq!(ids(&mut db, unbounded), vec![500]);

    let seek = cost(&mut db, bounded);
    let scan = cost(&mut db, unbounded);
    // The non-PK predicate full-scans all ~1000 a rows (and runs the ON over each); the PK pushdown
    // materializes one a row, so it is an order of magnitude cheaper.
    assert!(
        seek * 10 < scan,
        "bounded join {seek} should be far below the full-scan join {scan}"
    );
    assert!(
        seek <= 60,
        "bounded join {seek} should be sublinear (seek a + scan small b), not ~1000"
    );
}

#[test]
fn join_pushdown_miss_collapses_to_empty() {
    let mut db = tables(1000);
    // A point-lookup miss on the bounded side materializes ZERO a rows, so the nested loop has nothing
    // to drive: empty result at the cost of (a's miss page) + (b's full scan), not a 1000-row scan.
    let q = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 999999";
    assert!(ids(&mut db, q).is_empty());
    let miss = cost(&mut db, q);
    assert!(
        miss <= 60,
        "a miss-bounded join {miss} should collapse to b's small scan, not ~1000"
    );
}

#[test]
fn join_pushdown_both_sides_bounded() {
    let mut db = tables(1000);
    // Bounding BOTH tables by their own PK: a.id = 500 (one a row, k=500) and b.id = 1 (one b row,
    // k=500). They join on k. Sublinear in a's size on both counts.
    let q = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500 AND b.id = 1";
    assert_eq!(ids(&mut db, q), vec![500]); // a.id projected first
    let c = cost(&mut db, q);
    assert!(
        c <= 30,
        "both-sides-bounded join {c} should be tiny, not ~1000"
    );
}
