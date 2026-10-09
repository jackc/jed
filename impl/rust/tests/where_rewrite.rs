//! Stage-2 WHERE pushdown with BOUND PARAMETERS (spec/design/planner.md §3.2). The conformance corpus
//! pins pushdown rows/costs/EXPLAIN with literals (joins/where_pushdown.test,
//! query/where_rewrite_explain.test) but cannot bind `$N`; this checks the host-API surface it
//! cannot: a parameter operand is pushdown-safe, the pushed filter reads the bound value inside the
//! scan, the cost equals the same query spelled with the literal, and a prepared statement's cached
//! plan reuses the pushdown across executions with different values.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

const PARAM: &str =
    "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= $1 AND b.w > $2 ORDER BY a.id, b.id";

fn literal(v: i64, w: i64) -> String {
    format!(
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= {v} AND b.w > {w} ORDER BY a.id, b.id"
    )
}

fn pairs(rows: &[Vec<Value>]) -> Vec<(i64, i64)> {
    rows.iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(x), Value::Int(y)) => (*x, *y),
            _ => panic!("expected two ints"),
        })
        .collect()
}

fn query(s: &mut Session, sql: &str, params: &[Value]) -> (Vec<(i64, i64)>, i64) {
    match s.query_outcome(sql, params).unwrap() {
        Outcome::Query { rows, cost, .. } => (pairs(&rows), cost),
        Outcome::Statement { .. } => panic!("expected a query result"),
    }
}

#[test]
fn where_pushdown_bound_parameters() {
    let mut s = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32)",
        "CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32)",
        "INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40)",
        "INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1)",
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
    let stmt = s.prepare(PARAM).unwrap();
    let cases: [(i64, i64, Vec<(i64, i64)>); 3] = [
        (10, 4, vec![(1, 11), (2, 13)]),
        (20, 0, vec![(2, 13)]),
        (50, 0, vec![]),
    ];
    for (v, w, want) in cases {
        let params = [Value::Int(v), Value::Int(w)];
        let (_, literal_cost) = query(&mut s, &literal(v, w), &[]);

        let (rows, cost) = query(&mut s, PARAM, &params);
        assert_eq!(rows, want, "v={v} w={w}");
        assert_eq!(
            cost, literal_cost,
            "v={v} w={w}: a pushed $N must charge like a literal"
        );

        let mut cursor = s.query_prepared(&stmt, &params).unwrap();
        let mut prepared = Vec::new();
        for r in &mut cursor {
            prepared.push(r);
        }
        cursor.error().unwrap();
        assert_eq!(pairs(&prepared), want, "prepared v={v} w={w}");
        assert_eq!(cursor.cost(), literal_cost, "prepared v={v} w={w}");
    }
}

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Int(x) => *x,
            _ => panic!("expected an int"),
        })
        .collect()
}

/// A bound parameter in a conjunct moved into a derived body — below a grouped body's grouping, and
/// into every set-operation arm (planner.md §3.2) — or pushed from an ON (§3.3) reads the bound value where it now runs, charges exactly like the literal spelling, and a
/// prepared statement's cached plan keeps the rewrite (the body was planned again once, before bind).
#[test]
fn derived_and_on_pushdown_bound_parameters() {
    let mut s = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32)",
        "CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32)",
        "INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40)",
        "INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1)",
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
    let cases: [(&str, &str, i64, Vec<i64>); 4] = [
        (
            "SELECT d.id FROM (SELECT id, v FROM a) d WHERE d.id = $1",
            "SELECT d.id FROM (SELECT id, v FROM a) d WHERE d.id = 2",
            2,
            vec![2],
        ),
        (
            "SELECT d.k FROM (SELECT k, count(*) AS n FROM a GROUP BY k) d WHERE d.k = $1",
            "SELECT d.k FROM (SELECT k, count(*) AS n FROM a GROUP BY k) d WHERE d.k = 2",
            2,
            vec![2],
        ),
        (
            "SELECT s.id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) s WHERE s.id >= $1 ORDER BY s.id",
            "SELECT s.id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) s WHERE s.id >= 12 ORDER BY s.id",
            12,
            vec![12, 13, 14, 15],
        ),
        (
            "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k AND b.w > $1 WHERE b.id IS NOT NULL ORDER BY a.id",
            "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k AND b.w > 4 WHERE b.id IS NOT NULL ORDER BY a.id",
            4,
            vec![1, 2],
        ),
    ];
    for (param, literal, value, want) in cases {
        let stmt = s.prepare(param).unwrap();
        let literal_cost = match s.query_outcome(literal, &[]).unwrap() {
            Outcome::Query { cost, .. } => cost,
            Outcome::Statement { .. } => panic!("expected a query result"),
        };
        let params = [Value::Int(value)];
        match s.query_outcome(param, &params).unwrap() {
            Outcome::Query { rows, cost, .. } => {
                assert_eq!(ids(&rows), want, "{param}");
                assert_eq!(
                    cost, literal_cost,
                    "{param}: parameter cost vs literal cost"
                );
            }
            Outcome::Statement { .. } => panic!("expected a query result"),
        }
        for _ in 0..2 {
            let mut cursor = s.query_prepared(&stmt, &params).unwrap();
            let mut prepared = Vec::new();
            for r in &mut cursor {
                prepared.push(r);
            }
            cursor.error().unwrap();
            assert_eq!(ids(&prepared), want, "prepared {param}");
            assert_eq!(cursor.cost(), literal_cost, "prepared {param}");
        }
    }
}
