//! Set operations — UNION/INTERSECT/EXCEPT (each [ALL]). These complement the conformance corpus
//! (spec/conformance/suites/setops) with finer-grained per-feature assertions: PG precedence,
//! multiset multiplicities, integer<->decimal unification, the lhs+rhs cost contract, and the
//! error codes. See spec/design/grammar.md §25.

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

/// First-column integers of a query result (for ORDER BY-ed assertions).
fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    query(db, sql)
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        })
        .collect()
}

fn ab() -> Database {
    db_with(&[
        "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
        "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
    ])
}

#[test]
fn union_distinct_and_all() {
    let mut db = ab();
    assert_eq!(
        ints(&mut db, "SELECT k FROM a UNION SELECT k FROM b ORDER BY k"),
        vec![10, 20, 30, 40]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT k FROM a UNION ALL SELECT k FROM b ORDER BY k"
        ),
        vec![10, 20, 20, 30, 30, 40]
    );
}

#[test]
fn cost_is_sum_of_operands_window_unmetered() {
    let mut db = ab();
    // (1 page_read + 3 scan + 3 produce) per operand = 7 + 7; dedup unmetered.
    assert_eq!(
        execute(&mut db, "SELECT k FROM a UNION SELECT k FROM b")
            .unwrap()
            .cost(),
        14
    );
    // LIMIT does not lower the cost: operands fully produce, the window is unmetered.
    assert_eq!(
        execute(
            &mut db,
            "SELECT k FROM a UNION SELECT k FROM b ORDER BY k LIMIT 1"
        )
        .unwrap()
        .cost(),
        14
    );
}

#[test]
fn intersect_and_except_multiset() {
    let mut db = db_with(&[
        "CREATE TABLE l (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE r (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO l VALUES (1,1),(2,1),(3,1),(4,2),(5,3)", // 1->3, 2->1, 3->1
        "INSERT INTO r VALUES (1,1),(2,2)",                   // 1->1, 2->1
    ]);
    // min(m,n): 1->1, 2->1, 3->0
    assert_eq!(
        ints(
            &mut db,
            "SELECT k FROM l INTERSECT ALL SELECT k FROM r ORDER BY k"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT k FROM l INTERSECT SELECT k FROM r ORDER BY k"
        ),
        vec![1, 2]
    );
    // max(0,m-n): 1->2, 2->0, 3->1
    assert_eq!(
        ints(
            &mut db,
            "SELECT k FROM l EXCEPT ALL SELECT k FROM r ORDER BY k"
        ),
        vec![1, 1, 3]
    );
    assert_eq!(
        ints(&mut db, "SELECT k FROM l EXCEPT SELECT k FROM r ORDER BY k"),
        vec![3]
    );
}

#[test]
fn precedence_intersect_binds_tighter() {
    let mut db = db_with(&[
        "CREATE TABLE p (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE q (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO p VALUES (1, 1)",
        "INSERT INTO q VALUES (1, 2), (2, 3)",
        "INSERT INTO s VALUES (1, 3), (2, 4)",
    ]);
    // p UNION q INTERSECT s = p UNION (q INTERSECT s) = {1} UNION {3} = {1,3}.
    assert_eq!(
        ints(
            &mut db,
            "SELECT k FROM p UNION SELECT k FROM q INTERSECT SELECT k FROM s ORDER BY k"
        ),
        vec![1, 3]
    );
}

#[test]
fn int_decimal_unification_matches_and_types_decimal() {
    let mut db = db_with(&[
        "CREATE TABLE ai (id int32 PRIMARY KEY, n int32)",
        "CREATE TABLE ad (id int32 PRIMARY KEY, n decimal(10,2))",
        "INSERT INTO ai VALUES (1, 5), (2, 7)",
        "INSERT INTO ad VALUES (1, 5.0), (2, 9.50)",
    ]);
    // 5 (int, converted) == 5.00 (decimal): distinct set {5, 7, 9.50} — 3 decimal rows.
    let rows = query(&mut db, "SELECT n FROM ai UNION SELECT n FROM ad");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|r| matches!(r[0], Value::Decimal(_))),
        "expected all decimal values, got {rows:?}"
    );
}

#[test]
fn error_codes() {
    let mut db = db_with(&[
        "CREATE TABLE x (id int32 PRIMARY KEY, a int32, b int32)",
        "CREATE TABLE y (id int32 PRIMARY KEY, a int32, t text)",
        "INSERT INTO x VALUES (1, 10, 20)",
        "INSERT INTO y VALUES (1, 30, 'hi')",
    ]);
    let cases = [
        ("SELECT a, b FROM x UNION SELECT a FROM y", "42601"), // arity
        ("SELECT a FROM x UNION SELECT t FROM y", "42804"),    // type mismatch
        ("SELECT a FROM x ORDER BY a UNION SELECT a FROM y", "42601"), // operand ORDER BY
        (
            "SELECT a FROM x UNION SELECT a FROM y ORDER BY x.a",
            "42P01",
        ), // qualified key
        (
            "SELECT a FROM x UNION SELECT a FROM y ORDER BY nope",
            "42703",
        ), // unknown name
    ];
    for (sql, code) in cases {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), code, "{sql}");
    }
}
