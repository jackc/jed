//! Primary-key predicate pushdown over a MULTI-LEAF B-tree (spec/design/cost.md §3 "bounded scan").
//! The conformance corpus (spec/conformance/suites/query/point_lookup.test) pins the cost contract on
//! single-leaf tables; this exercises what it cannot — a tree wide enough that the cost is visibly
//! sublinear, and a range scan that spans leaf boundaries. The direct page_read-drop check (overlap <
//! node_count) is an in-crate unit test in `pmap.rs`.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

/// A table of `n` rows (id i32 PRIMARY KEY, v i32; v == id), wide enough to span several leaves.
fn big_table(n: i64) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    let mut sql = String::from("INSERT INTO t VALUES ");
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
fn point_lookup_is_sublinear() {
    let mut db = big_table(1000);
    assert_eq!(ids(&mut db, "SELECT v FROM t WHERE id = 500"), vec![500]);
    let point = cost(&mut db, "SELECT v FROM t WHERE id = 500");
    let full = cost(&mut db, "SELECT v FROM t");
    assert!(
        point < full,
        "point cost {point} should be far below full-scan {full}"
    );
    assert!(
        point <= 50,
        "point cost {point} should be small (≈ height + a few), not ~1000"
    );
    // A miss still visits the leaf it would live in but reads no row.
    assert!(ids(&mut db, "SELECT v FROM t WHERE id = 99999").is_empty());
    let miss = cost(&mut db, "SELECT v FROM t WHERE id = 99999");
    assert!(
        miss > 0 && miss <= 50,
        "miss cost {miss} should be small and non-zero"
    );
}

#[test]
fn range_crosses_leaf_boundaries() {
    let mut db = big_table(1000);
    let got = ids(
        &mut db,
        "SELECT id FROM t WHERE id >= 300 AND id <= 700 ORDER BY id",
    );
    assert_eq!(got, (300..=700).collect::<Vec<_>>());
    let tail = ids(&mut db, "SELECT id FROM t WHERE id > 996 ORDER BY id");
    assert_eq!(tail, vec![997, 998, 999, 1000]);
    // Contradictory bound: empty, cost 0 — proved without scanning.
    assert!(ids(&mut db, "SELECT id FROM t WHERE id > 700 AND id < 300").is_empty());
    assert_eq!(
        cost(&mut db, "SELECT id FROM t WHERE id > 700 AND id < 300"),
        0
    );
}

#[test]
fn limit_short_circuit_is_sublinear() {
    let mut db = big_table(1000); // id 1..1000, v == id
    // LIMIT without ORDER BY stops the scan early: `limit` rows at sublinear cost, the PK-order prefix.
    assert_eq!(ids(&mut db, "SELECT v FROM t LIMIT 5"), vec![1, 2, 3, 4, 5]);
    let point = cost(&mut db, "SELECT v FROM t LIMIT 5");
    let full = cost(&mut db, "SELECT v FROM t");
    assert!(
        point < full,
        "LIMIT cost {point} should be far below full-scan {full}"
    );
    assert!(
        point <= 20,
        "LIMIT 5 cost {point} should be sublinear (≈ limit + node count), not ~1000"
    );
    assert_eq!(
        ids(&mut db, "SELECT v FROM t LIMIT 3 OFFSET 10"),
        vec![11, 12, 13]
    );

    // Trap windowing: streaming projects ONLY the windowed rows, so a later trapping row is never
    // reached under a LIMIT that excludes it (matches the eager window-before-project).
    let mut dz = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    dz.execute("CREATE TABLE z (id i32 PRIMARY KEY, c i32)", &[])
        .unwrap();
    dz.execute("INSERT INTO z VALUES (1, 5), (2, 0), (3, 5)", &[])
        .unwrap();
    assert_eq!(ids(&mut dz, "SELECT 100 / c FROM z LIMIT 1"), vec![20]);
    assert!(
        dz.execute("SELECT 100 / c FROM z LIMIT 2", &[]).is_err(),
        "LIMIT 2 reaches the c=0 row and must trap"
    );
}

#[test]
fn mutation_pushdown_is_sublinear() {
    let mut db = big_table(1000);
    let d = cost(&mut db, "DELETE FROM t WHERE id = 500");
    assert!(d <= 50, "DELETE point-lookup cost {d} should be sublinear");
    assert!(ids(&mut db, "SELECT id FROM t WHERE id = 500").is_empty());
    assert_eq!(ids(&mut db, "SELECT id FROM t WHERE id = 501"), vec![501]);

    let u = cost(&mut db, "UPDATE t SET v = -1 WHERE id = 700");
    assert!(u <= 50, "UPDATE point-lookup cost {u} should be sublinear");
    assert_eq!(ids(&mut db, "SELECT v FROM t WHERE id = 700"), vec![-1]);
}
