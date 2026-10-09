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
