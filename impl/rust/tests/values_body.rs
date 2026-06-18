//! VALUES-body derived tables — `FROM (VALUES (e…),(e…)) [AS] v(c…)` (spec/design/grammar.md §42).
//! A parenthesized `VALUES` list used as a FROM relation: a computed relation of literal rows, the
//! FROM-position sibling of `INSERT … VALUES`, reusing the derived-table seam (an anonymous,
//! always-inlined single-reference CTE). These complement the conformance corpus
//! (spec/conformance/suites/subquery/values_body.test) with finer-grained per-feature assertions:
//! the default `column1…` names + the column-rename list, general constant expressions, per-column
//! type unification across rows, composition with WHERE/ORDER BY/JOIN/aggregates, the intrinsic
//! cost, and the error / narrowing codes (42601 / 42804 / 42703 / 42803 / 42P18).

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

fn names(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { column_names, .. } => column_names,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn types(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { column_types, .. } => column_types,
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

fn cost(db: &mut Database, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

// ---- basic shape ----------------------------------------------------------------------------

#[test]
fn single_column_rows_default_name() {
    let mut db = Database::new();
    // Default column name is column1 (PostgreSQL), one row per VALUES row, in body order.
    assert_eq!(
        ints(
            &mut db,
            "SELECT column1 FROM (VALUES (1), (2), (3)) AS v ORDER BY column1"
        ),
        vec![1, 2, 3]
    );
    assert_eq!(
        names(&mut db, "SELECT * FROM (VALUES (1), (2)) AS v"),
        vec!["column1".to_string()]
    );
}

#[test]
fn multi_column_and_rename_list() {
    let mut db = Database::new();
    // Two columns -> column1, column2; the rename list renames left-to-right.
    assert_eq!(
        names(&mut db, "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v"),
        vec!["column1".to_string(), "column2".to_string()]
    );
    assert_eq!(
        names(&mut db, "SELECT * FROM (VALUES (1, 'a')) AS v(n, s)"),
        vec!["n".to_string(), "s".to_string()]
    );
    // A partial rename keeps the trailing body name (cte.md §1, the derived-table rule).
    assert_eq!(
        names(&mut db, "SELECT * FROM (VALUES (1, 'a')) AS v(n)"),
        vec!["n".to_string(), "column2".to_string()]
    );
    // Qualified by the alias.
    assert_eq!(
        ints(
            &mut db,
            "SELECT v.n FROM (VALUES (7), (8)) AS v(n) ORDER BY v.n"
        ),
        vec![7, 8]
    );
}

#[test]
fn optional_alias() {
    let mut db = Database::new();
    // No alias at all (PG 18) — bare columns still resolve by their default names.
    assert_eq!(
        ints(
            &mut db,
            "SELECT column1 FROM (VALUES (5), (6)) ORDER BY column1"
        ),
        vec![5, 6]
    );
}

// ---- general constant expressions (the PG-faithful surface, richer than INSERT … VALUES) -----

#[test]
fn general_constant_expressions() {
    let mut db = Database::new();
    // Arithmetic — constant-folded per row (richer than the literal-only INSERT … VALUES slot).
    assert_eq!(
        ints(
            &mut db,
            "SELECT column1 FROM (VALUES (1 + 1), (2 * 3), (10 - 4)) AS v ORDER BY column1"
        ),
        vec![2, 6, 6]
    );
    // A cast as a value (decimal -> int32 rounds half away from zero: 2.5 -> 3).
    assert_eq!(
        ints(&mut db, "SELECT column1 FROM (VALUES (2.5 :: int32)) AS v"),
        vec![3]
    );
    // A CASE expression as a value.
    assert_eq!(
        ints(
            &mut db,
            "SELECT column1 FROM (VALUES (CASE WHEN true THEN 1 ELSE 0 END)) AS v"
        ),
        vec![1]
    );
}

// ---- per-column type unification across rows --------------------------------------------------

#[test]
fn column_type_unification() {
    let mut db = Database::new();
    // int + int -> int (widths widen): all bare integer literals are int64 in jed.
    assert_eq!(
        types(&mut db, "SELECT column1 FROM (VALUES (1), (2)) AS v"),
        vec!["int64"]
    );
    // int + decimal -> decimal; the int value coerces.
    assert_eq!(
        types(&mut db, "SELECT column1 FROM (VALUES (1), (2.5)) AS v"),
        vec!["decimal"]
    );
    // Both rows are decimals (the int literal coerced to the unified column type); exact rendering
    // is oracle-checked in the conformance corpus.
    let rows = query(
        &mut db,
        "SELECT column1 FROM (VALUES (1), (2.5)) AS v ORDER BY column1",
    );
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|r| matches!(r[0], Value::Decimal(_))),
        "{rows:?}"
    );
    // anything + NULL keeps the other type (a NULL row stays NULL).
    assert_eq!(
        types(&mut db, "SELECT column1 FROM (VALUES (1), (NULL)) AS v"),
        vec!["int64"]
    );
    // an all-NULL column is text (unknown -> text).
    assert_eq!(
        types(&mut db, "SELECT column1 FROM (VALUES (NULL), (NULL)) AS v"),
        vec!["text"]
    );
}

// ---- composition ------------------------------------------------------------------------------

#[test]
fn composes_with_where_join_aggregate() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ]);
    // WHERE over the VALUES relation.
    assert_eq!(
        ints(
            &mut db,
            "SELECT column1 FROM (VALUES (1), (2), (3)) AS v WHERE column1 > 1 ORDER BY column1"
        ),
        vec![2, 3]
    );
    // JOIN a base table against a VALUES relation.
    assert_eq!(
        ints(
            &mut db,
            "SELECT t.id FROM t JOIN (VALUES (1), (3)) AS v(id) ON t.id = v.id ORDER BY t.id"
        ),
        vec![1, 3]
    );
    // Aggregate over a VALUES relation (max returns the int column type; sum widens to decimal).
    assert_eq!(
        ints(
            &mut db,
            "SELECT max(column1) FROM (VALUES (1), (2), (3)) AS v"
        ),
        vec![3]
    );
}

#[test]
fn reachable_inside_a_subquery() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2), (5)",
    ]);
    // A VALUES body inside an IN-subquery.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM t WHERE id IN (SELECT column1 FROM (VALUES (1), (5)) AS v) ORDER BY id"
        ),
        vec![1, 5]
    );
}

#[test]
fn params_typed_by_sibling_rows() {
    let mut db = Database::new();
    // A $1 in a column with a concrete sibling literal is typed by the unified column type.
    match execute_params(
        &mut db,
        "SELECT column1 FROM (VALUES (1), ($1)) AS v ORDER BY column1",
        &[Value::Int(7)],
    )
    .unwrap()
    {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(7)]]);
        }
        _ => panic!("expected a query"),
    }
}

// ---- cost -------------------------------------------------------------------------------------

#[test]
fn intrinsic_cost() {
    let mut db = Database::new();
    // The VALUES body charges row_produced per row (3); the outer SELECT charges row_produced per
    // output row (3) — its projection is a bare column (no operator_eval). Total 6. Deterministic,
    // cross-core identical (the jed contract; cost.md §3, the inline derived-table path).
    assert_eq!(
        cost(&mut db, "SELECT column1 FROM (VALUES (1), (2), (3)) AS v"),
        6
    );
    // A value expression adds its operator_eval: (1+1) charges one operator_eval.
    assert_eq!(
        cost(&mut db, "SELECT column1 FROM (VALUES (1 + 1)) AS v"),
        1 + 1 + 1
    );
}

// ---- errors / narrowings ----------------------------------------------------------------------

#[test]
fn errors() {
    let mut db = Database::new();
    let cases: &[(&str, &str)] = &[
        // Rows of differing arity -> 42601.
        ("SELECT * FROM (VALUES (1), (2, 3)) AS v", "42601"),
        // Columns whose row types do not unify -> 42804.
        ("SELECT * FROM (VALUES (1), ('a')) AS v", "42804"),
        // A column reference inside a value (non-LATERAL) -> 42703.
        ("SELECT * FROM (VALUES (oops)) AS v", "42703"),
        // An aggregate inside a value -> 42803.
        ("SELECT * FROM (VALUES (sum(1))) AS v", "42803"),
        // A bare $1 with no inferable type -> 42P18.
        ("SELECT * FROM (VALUES ($1)) AS v", "42P18"),
        // A trailing ORDER BY on the VALUES body is a deferred narrowing -> 42601.
        ("SELECT * FROM (VALUES (1), (2) ORDER BY 1) AS v", "42601"),
        // A column-rename list longer than the body's column count -> 42P10.
        ("SELECT * FROM (VALUES (1)) AS v(a, b)", "42P10"),
    ];
    for (sql, code) in cases {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), *code, "{sql}");
    }
}
