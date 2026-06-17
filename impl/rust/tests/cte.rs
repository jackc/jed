//! Common table expressions — `WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>`,
//! non-recursive (spec/design/cte.md). These complement the conformance corpus
//! (spec/conformance/suites/cte) with finer-grained per-feature assertions: the inline-vs-
//! materialize cost split, forward-only visibility, base-table shadowing, the column-rename list,
//! set-op / aggregate / JOIN bodies, CTE references inside a nested subquery, and the error /
//! narrowing codes (42712 / 42P01 / 42P10 / 42703 / 0A000 / 42601).

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

fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    query(db, sql)
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        })
        .collect()
}

fn names(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { column_names, .. } => column_names,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

/// A 3-row, single-node table `t(id, n)` = {(1,10),(2,20),(3,30)}.
fn t3() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ])
}

#[test]
fn single_reference_inlines() {
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id"
        ),
        vec![1, 2, 3]
    );
    // A single reference INLINES: body (page_read 1 + 3 storage_row_read + 3 row_produced = 7) +
    // the outer's 3 row_produced = 10. No cte_scan_row (cost.md §3).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id"
        ),
        10
    );
}

#[test]
fn multiple_references_materialize() {
    let mut db = t3();
    // Two references MATERIALIZE: body once (7) + 6 cte_scan_row (two 3-row buffer scans) + 9
    // row_produced (3x3 product) = 22.
    let sql = "WITH c AS (SELECT id FROM t) SELECT a.id AS x, b.id AS y FROM c a CROSS JOIN c b";
    assert_eq!(query(&mut db, sql).len(), 9);
    assert_eq!(cost(&mut db, sql), 22);
}

#[test]
fn unreferenced_cte_is_not_executed() {
    let mut db = t3();
    // An unreferenced CTE is planned/type-checked but not executed: only SELECT 1's row_produced.
    assert_eq!(cost(&mut db, "WITH c AS (SELECT id FROM t) SELECT 1"), 1);
}

#[test]
fn materialized_hint_forces_buffering() {
    let mut db = t3();
    // MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
    // row_produced = 13 (vs the inlined 10).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"
        ),
        13
    );
    // NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
    // bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"
        ),
        23
    );
}

#[test]
fn later_cte_references_earlier() {
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT id, n FROM t), d AS (SELECT n * 2 AS m FROM c) SELECT m FROM d ORDER BY m"
        ),
        vec![20, 40, 60]
    );
}

#[test]
fn column_rename_list() {
    let mut db = t3();
    assert_eq!(
        names(
            &mut db,
            "WITH c (a, b) AS (SELECT id, n FROM t) SELECT a, b FROM c ORDER BY a"
        ),
        vec!["a", "b"]
    );
    // Fewer aliases than body columns: a partial rename — the first column becomes `a`, the second
    // keeps its body name `n` (PostgreSQL).
    assert_eq!(
        names(
            &mut db,
            "WITH c (a) AS (SELECT id, n FROM t) SELECT * FROM c ORDER BY a"
        ),
        vec!["a", "n"]
    );
}

#[test]
fn set_op_and_aggregate_bodies() {
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT n FROM t WHERE id = 1 UNION ALL SELECT n FROM t WHERE id = 2) SELECT n FROM c ORDER BY n"
        ),
        vec![10, 20]
    );
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT count(*) AS k FROM t) SELECT k FROM c"
        ),
        vec![3]
    );
}

#[test]
fn join_of_two_ctes() {
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT id, n FROM t), d AS (SELECT id FROM t WHERE n >= 20) \
             SELECT c.n FROM c JOIN d ON c.id = d.id ORDER BY c.id"
        ),
        vec![20, 30]
    );
}

#[test]
fn referenced_in_nested_subquery() {
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH c AS (SELECT n FROM t) SELECT id FROM t WHERE n = (SELECT max(n) FROM c) ORDER BY id"
        ),
        vec![3]
    );
}

#[test]
fn shadows_base_table_outside_body_not_inside() {
    // The CTE `t` shadows the base table in the outer query, but its OWN body resolves the base
    // table (the binding is not in scope for itself — spec/design/cte.md §2).
    let mut db = t3();
    assert_eq!(
        ints(
            &mut db,
            "WITH t AS (SELECT n + 100 AS n FROM t) SELECT n FROM t ORDER BY n"
        ),
        vec![110, 120, 130]
    );
}

#[test]
fn error_codes() {
    let mut db = t3();
    let cases = [
        // Duplicate CTE name in one list.
        (
            "WITH c AS (SELECT id FROM t), c AS (SELECT id FROM t) SELECT id FROM c",
            "42712",
        ),
        // Self-reference (non-recursive) — no base table `c`.
        ("WITH c AS (SELECT id FROM c) SELECT id FROM c", "42P01"),
        // Forward reference to a later CTE.
        (
            "WITH c AS (SELECT id FROM d), d AS (SELECT id FROM t) SELECT id FROM c",
            "42P01",
        ),
        // Column-rename arity: too MANY aliases is 42P10 (too few is a legal partial rename).
        (
            "WITH c (a, b, x) AS (SELECT id, n FROM t) SELECT a FROM c",
            "42P10",
        ),
        // A body resolves only its own scope — an unknown column is the ordinary 42703.
        (
            "WITH c AS (SELECT missing FROM t) SELECT id FROM c",
            "42703",
        ),
        // WITH RECURSIVE is deferred.
        (
            "WITH RECURSIVE c AS (SELECT id FROM t) SELECT id FROM c",
            "0A000",
        ),
        // A nested WITH (top-level-only narrowing) is a syntax error.
        (
            "WITH a AS (WITH b AS (SELECT id FROM t) SELECT id FROM b) SELECT id FROM a",
            "42601",
        ),
    ];
    for (sql, code) in cases {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), code, "{sql}");
    }
}
