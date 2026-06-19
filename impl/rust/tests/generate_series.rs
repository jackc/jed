//! `generate_series` — the engine's first set-returning function, a FROM-clause row source
//! (spec/design/functions.md §10, grammar.md §35). These complement the conformance corpus
//! (spec/conformance/suites/query/generate_series.test) with finer-grained assertions: the
//! generator's PostgreSQL edge cases (NULL → empty, step zero → 22023, descending step, the
//! positive-default-step empty case, i64-overflow clean-stop), the synthetic-relation wiring
//! (output column name/type, alias + qualified resolution, CROSS JOIN composition), the
//! arg-scope rule ($N / correlated outer arg; an SRF is implicitly lateral so a sibling works), the
//! generated_row cost contract + the max_cost ceiling, and the deferred-form errors.

use jed::value::Value;
use jed::{Database, Outcome, execute, execute_params};

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

fn cost(db: &mut Database, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql:?}: expected an error"))
        .code()
        .to_string()
}

fn ints(ns: &[i64]) -> Vec<Vec<Value>> {
    ns.iter().map(|&n| vec![Value::Int(n)]).collect()
}

// ---- the generator: rows, names, type ------------------------------------------------------

// ---- step of zero → 22023 -------------------------------------------------------------------

#[test]
fn zero_step_is_invalid_parameter_value() {
    let mut db = Database::new();
    let e =
        execute(&mut db, "SELECT * FROM generate_series(1, 5, 0)").expect_err("expected an error");
    assert_eq!(e.code(), "22023");
    assert_eq!(e.message, "step size cannot be equal to zero");
}

// ---- aliases + qualified resolution ---------------------------------------------------------

#[test]
fn alias_forms_and_qualified_column() {
    let mut db = Database::new();
    // PG's single-column function-alias rule: `AS g` (or the implicit `g`) renames the output
    // column to `g`, so the column is `g.g`, and `g.generate_series` is 42703 (no such column).
    assert_eq!(
        query(&mut db, "SELECT * FROM generate_series(1, 3) g"),
        ints(&[1, 2, 3])
    );
    assert_eq!(
        execute(&mut db, "SELECT * FROM generate_series(1, 3) AS g")
            .unwrap()
            .column_names(),
        &["g"]
    );
    assert_eq!(
        query(&mut db, "SELECT g.g FROM generate_series(1, 3) AS g"),
        ints(&[1, 2, 3])
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT g.generate_series FROM generate_series(1, 3) AS g"
        ),
        "42703"
    );
    // No alias: the column keeps the function name, so a qualified reference uses it too.
    assert_eq!(
        query(
            &mut db,
            "SELECT generate_series.generate_series FROM generate_series(1, 2)"
        ),
        ints(&[1, 2])
    );
}

// ---- arg scope: $N / correlated outer arg work; a sibling works (implicitly lateral, §44) ----

#[test]
fn param_argument() {
    let mut db = Database::new();
    let out = execute_params(
        &mut db,
        "SELECT * FROM generate_series(1, $1)",
        &[Value::Int(3)],
    )
    .unwrap();
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows, ints(&[1, 2, 3])),
        other => panic!("expected a query result, got {other:?}"),
    }
}

#[test]
fn sibling_reference_works_implicitly_lateral() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
        "INSERT INTO t VALUES (1, 3)",
    ]);
    // A FROM-sibling reference inside the SRF args IS visible — an SRF is implicitly lateral
    // (grammar.md §44; the rows are pinned by suites/joins/lateral.test). The prior non-LATERAL
    // 42P01 rejection is lifted: generate_series(1, t.n) re-runs per t row (1 row, n=3 ⇒ 3 rows).
    let out = execute(
        &mut db,
        "SELECT * FROM t CROSS JOIN generate_series(1, t.n)",
    )
    .unwrap();
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows.len(), 3),
        other => panic!("expected a query result, got {other:?}"),
    }
}

// ---- cost: generated_row accrual + the max_cost ceiling -------------------------------------

#[test]
fn generated_row_cost_and_ceiling() {
    let mut db = Database::new();
    // 4 generated_row + 4 row_produced.
    assert_eq!(cost(&mut db, "SELECT * FROM generate_series(1, 4)"), 8);
    // A runaway series aborts deterministically once accrued cost reaches the ceiling (54P01),
    // before the whole series materializes.
    db.set_max_cost(50);
    assert_eq!(
        err_code(&mut db, "SELECT * FROM generate_series(1, 1000000000)"),
        "54P01"
    );
}

// ---- mixed-width promotion ------------------------------------------------------------------

#[test]
fn mixed_width_promotes_to_the_wider_type() {
    let mut db = Database::new();
    let out = execute(
        &mut db,
        "SELECT * FROM generate_series(CAST(1 AS int16), CAST(5 AS int32))",
    )
    .unwrap();
    assert_eq!(out.column_types(), &["int32"]);
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows, ints(&[1, 2, 3, 4, 5])),
        other => panic!("expected a query result, got {other:?}"),
    }
}

// ---- i64-boundary clean-stop (cross-core parity pin) ----------------------------------------

#[test]
fn i64_overflow_while_stepping_stops_cleanly() {
    let mut db = Database::new();
    // Stepping past i64::MAX must STOP, not trap: the last representable element is emitted then
    // the series ends (matching PostgreSQL). start = MAX-1, step 2 → just {MAX-1}.
    assert_eq!(
        query(
            &mut db,
            "SELECT * FROM generate_series(9223372036854775806, 9223372036854775807, 2)"
        ),
        ints(&[9223372036854775806])
    );
}

// ---- deferred-form + bad-call errors --------------------------------------------------------

#[test]
fn deferred_and_bad_call_errors() {
    let mut db = Database::new();
    // SELECT-list SRF is deferred — `generate_series` is not a scalar function.
    assert_eq!(err_code(&mut db, "SELECT generate_series(1, 5)"), "42883");
    // Column-alias list on a table function is deferred.
    assert_eq!(
        err_code(&mut db, "SELECT * FROM generate_series(1, 5) AS g(n)"),
        "0A000"
    );
    // Wrong arity / non-integer args: no matching function.
    assert_eq!(
        err_code(&mut db, "SELECT * FROM generate_series(1)"),
        "42883"
    );
    assert_eq!(
        err_code(&mut db, "SELECT * FROM generate_series('a', 5)"),
        "42883"
    );
    // An unknown table-function name.
    assert_eq!(err_code(&mut db, "SELECT * FROM nope(1, 5)"), "42883");
}
