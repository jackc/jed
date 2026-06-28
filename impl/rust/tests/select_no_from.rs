//! FROM-less SELECT — the select list evaluates over ONE virtual zero-column row, no table
//! access (spec/design/grammar.md §34). These complement the conformance corpus
//! (spec/conformance/suites/query/select_no_from.test) with finer-grained assertions: the
//! virtual-row pipeline (WHERE / aggregates / DISTINCT / HAVING / LIMIT compose), the zero-scan
//! cost contract (SELECT 1 = exactly 1 row_produced — spec/design/cost.md §3), composition in
//! set operations / subqueries (correlated included) / INSERT ... SELECT, and the error surface
//! (SELECT * → 42601 with PostgreSQL's exact message; a bare column — including the
//! `SELECT distinct` lookahead consequence — → 42703; an untyped $1 → 42P18).

use jed::value::Value;
use jed::{Engine, Outcome, execute, execute_params};

fn db_with(stmts: &[&str]) -> Engine {
    let mut db = Engine::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn query(db: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn cost(db: &mut Engine, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

fn err_code(db: &mut Engine, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql:?}: expected an error"))
        .code()
        .to_string()
}

// ---- the virtual row ------------------------------------------------------------------------

#[test]
fn literal_select_returns_one_row_costing_one_row_produced() {
    let mut db = Engine::new();
    let out = execute(&mut db, "SELECT 1").unwrap();
    match &out {
        Outcome::Query {
            column_names, rows, ..
        } => {
            assert_eq!(column_names, &["?column?"]);
            assert_eq!(rows, &[vec![Value::Int(1)]]);
        }
        other => panic!("expected a query result, got {other:?}"),
    }
    // No relation, no scan: zero page_read/storage_row_read — just the one row_produced.
    assert_eq!(out.cost(), 1);
}

#[test]
fn expression_select_charges_its_operator_evals() {
    let mut db = Engine::new();
    assert_eq!(query(&mut db, "SELECT 1 + 2"), vec![vec![Value::Int(3)]]);
    // 1 operator_eval (the `+` node) + 1 row_produced.
    assert_eq!(cost(&mut db, "SELECT 1 + 2"), 2);
}

#[test]
fn where_filters_the_virtual_row() {
    let mut db = Engine::new();
    assert_eq!(
        query(&mut db, "SELECT 1 WHERE false"),
        Vec::<Vec<Value>>::new()
    );
    // The constant filter is a leaf (no operator_eval) and no row is produced.
    assert_eq!(cost(&mut db, "SELECT 1 WHERE false"), 0);
    assert_eq!(
        query(&mut db, "SELECT 1 WHERE 1 = 1"),
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(cost(&mut db, "SELECT 1 WHERE 1 = 1"), 2); // the `=` + the produced row
}

#[test]
fn aggregates_fold_the_single_group() {
    let mut db = Engine::new();
    // The virtual row is the one input row of the whole-table group (aggregates.md §4).
    assert_eq!(query(&mut db, "SELECT count(*)"), vec![vec![Value::Int(1)]]);
    assert_eq!(cost(&mut db, "SELECT count(*)"), 2); // 1 aggregate_accumulate + 1 row_produced
    // A false WHERE empties the input but the single group still emits.
    assert_eq!(
        query(&mut db, "SELECT count(*) WHERE false"),
        vec![vec![Value::Int(0)]]
    );
    assert_eq!(cost(&mut db, "SELECT count(*) WHERE false"), 1);
    assert_eq!(query(&mut db, "SELECT max(5)"), vec![vec![Value::Int(5)]]);
    // HAVING filters the single group away.
    assert_eq!(
        query(&mut db, "SELECT 1 HAVING false"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn distinct_and_limit_apply_to_the_single_row() {
    let mut db = Engine::new();
    assert_eq!(
        query(&mut db, "SELECT DISTINCT 1"),
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(query(&mut db, "SELECT 1 LIMIT 0"), Vec::<Vec<Value>>::new());
    assert_eq!(
        query(&mut db, "SELECT 1 OFFSET 1"),
        Vec::<Vec<Value>>::new()
    );
}

// ---- composition ----------------------------------------------------------------------------

#[test]
fn set_operation_operands() {
    let mut db = Engine::new();
    let mut got: Vec<i64> = query(&mut db, "SELECT 1 UNION SELECT 2")
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
    // Each operand costs 1; the combine is unmetered (cost.md §3).
    assert_eq!(cost(&mut db, "SELECT 1 UNION SELECT 2"), 2);
}

#[test]
fn subqueries_uncorrelated_and_correlated() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2)",
    ]);
    // Uncorrelated FROM-less inner: folded once.
    assert_eq!(
        query(&mut db, "SELECT (SELECT 1)"),
        vec![vec![Value::Int(1)]]
    );
    // Correlated FROM-less inner: the zero-relation scope resolves o.id purely outward,
    // re-executed per outer row.
    assert_eq!(
        query(&mut db, "SELECT (SELECT o.id) FROM t o ORDER BY id"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    // 1 page_read + 2 storage_row_read + per outer row (×2): the subquery node's
    // operator_eval + the inner row_produced; + 2 outer row_produced = 9.
    assert_eq!(
        cost(&mut db, "SELECT (SELECT o.id) FROM t o ORDER BY id"),
        9
    );
}

#[test]
fn insert_select_source() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let out = execute(&mut db, "INSERT INTO t SELECT 3").unwrap();
    assert_eq!(out.cost(), 1); // exactly the embedded SELECT's cost
    assert_eq!(
        query(&mut db, "SELECT id FROM t"),
        vec![vec![Value::Int(3)]]
    );
}

// ---- errors ---------------------------------------------------------------------------------

#[test]
fn star_with_no_tables_is_42601_with_pg_message() {
    let mut db = Engine::new();
    let err = execute(&mut db, "SELECT *").unwrap_err();
    assert_eq!(err.code(), "42601");
    assert_eq!(
        err.message,
        "SELECT * with no tables specified is not valid"
    );
}

#[test]
fn bare_columns_resolve_nothing() {
    let mut db = Engine::new();
    assert_eq!(err_code(&mut db, "SELECT nope"), "42703");
    // The DISTINCT two-token lookahead is unchanged: at end of input the word is a column
    // reference, not the modifier (grammar.md §34 — previously died at the FROM expect).
    assert_eq!(err_code(&mut db, "SELECT distinct"), "42703");
    assert_eq!(err_code(&mut db, "SELECT from"), "42703");
    // GROUP BY / ORDER BY keys are table columns only — always 42703 on a lone FROM-less SELECT.
    assert_eq!(err_code(&mut db, "SELECT 1 GROUP BY nope"), "42703");
    assert_eq!(err_code(&mut db, "SELECT 1 ORDER BY nope"), "42703");
}

#[test]
fn untyped_param_is_42p18_and_a_sibling_operand_types_it() {
    let mut db = Engine::new();
    let err = execute_params(&mut db, "SELECT $1", &[Value::Int(7)]).unwrap_err();
    assert_eq!(err.code(), "42P18");
    // The sibling-operand rule (grammar.md §5) works without a FROM.
    let out = execute_params(&mut db, "SELECT $1 + 1", &[Value::Int(7)]).unwrap();
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(8)]]),
        other => panic!("expected a query result, got {other:?}"),
    }
}
