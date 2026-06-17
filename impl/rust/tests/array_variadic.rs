//! Array function/operator surface — AF6 (spec/design/array-functions.md §12): the `VARIADIC` call
//! syntax + variadic overload resolution, spent on the engine's first VARIADIC built-ins
//! `num_nulls` / `num_nonnulls` (count the NULL / non-NULL arguments → int32). Every expected value
//! is pinned against PostgreSQL 18, including the NULL discipline (the spread form never returns
//! NULL; the VARIADIC-array form returns NULL on a NULL whole-array operand).

use jed::{Database, Outcome, execute};

fn err(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// One-column, one-row scalar query → the rendered value (NULL renders as "NULL").
fn val(db: &mut Database, sql: &str) -> String {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message)) {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql}: expected one row");
            assert_eq!(rows[0].len(), 1, "{sql}: expected one column");
            rows[0][0].render()
        }
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

#[test]
fn spread_form_counts() {
    let mut db = Database::new();
    // num_nulls / num_nonnulls over a spread of arguments.
    assert_eq!(val(&mut db, "SELECT num_nulls(1, NULL, 3)"), "1");
    assert_eq!(val(&mut db, "SELECT num_nonnulls(1, NULL, 3)"), "2");
    // A single bare NULL is one (NULL) argument — never NULL (the non-strict discipline).
    assert_eq!(val(&mut db, "SELECT num_nulls(NULL)"), "1");
    assert_eq!(val(&mut db, "SELECT num_nonnulls(NULL)"), "0");
    // Heterogeneous arguments — the VARIADIC "any" element family.
    assert_eq!(val(&mut db, "SELECT num_nulls(1, 'a', true, NULL)"), "1");
    assert_eq!(val(&mut db, "SELECT num_nonnulls(1, 'a', true, NULL)"), "3");
    // A single non-VARIADIC array argument is ONE value (the array itself, non-NULL).
    assert_eq!(val(&mut db, "SELECT num_nulls(ARRAY[1,NULL,3])"), "0");
    assert_eq!(val(&mut db, "SELECT num_nonnulls(ARRAY[1,NULL,3])"), "1");
}

#[test]
fn variadic_array_form_counts_elements() {
    let mut db = Database::new();
    // VARIADIC arr spreads the array's elements.
    assert_eq!(
        val(&mut db, "SELECT num_nulls(VARIADIC ARRAY[1,NULL,3])"),
        "1"
    );
    assert_eq!(
        val(&mut db, "SELECT num_nonnulls(VARIADIC ARRAY[1,NULL,3])"),
        "2"
    );
    // The empty array → 0.
    assert_eq!(
        val(&mut db, "SELECT num_nulls(VARIADIC '{}'::int32[])"),
        "0"
    );
    // A multidimensional array flattens (row-major).
    assert_eq!(
        val(
            &mut db,
            "SELECT num_nulls(VARIADIC '{{1,2},{NULL,4}}'::int32[])"
        ),
        "1"
    );
    // The two forms agree.
    assert_eq!(
        val(&mut db, "SELECT num_nulls(VARIADIC ARRAY[1,NULL,3])"),
        val(&mut db, "SELECT num_nulls(1, NULL, 3)")
    );
}

#[test]
fn variadic_null_whole_array_is_null() {
    let mut db = Database::new();
    // A NULL whole-array operand → NULL (both functions) — distinct from the spread form.
    assert_eq!(
        val(&mut db, "SELECT num_nulls(VARIADIC NULL::int32[])"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT num_nonnulls(VARIADIC NULL::int32[])"),
        "NULL"
    );
}

#[test]
fn errors() {
    let mut db = Database::new();
    // VARIADIC on a non-array operand → 42804 "VARIADIC argument must be an array".
    assert_eq!(err(&mut db, "SELECT num_nulls(VARIADIC 5)"), "42804");
    // VARIADIC on a bare untyped NULL → 42804 (not the polymorphic 42P18).
    assert_eq!(err(&mut db, "SELECT num_nulls(VARIADIC NULL)"), "42804");
    // The spread form needs ≥1 argument — num_nulls() has no overload (42883).
    assert_eq!(err(&mut db, "SELECT num_nulls()"), "42883");
    // VARIADIC on a non-variadic function → 42883 (no such overload).
    assert_eq!(err(&mut db, "SELECT abs(VARIADIC ARRAY[1])"), "42883");
    // Named notation on num_nulls (no parameter names) → 42883.
    assert_eq!(err(&mut db, "SELECT num_nulls(x => 1)"), "42883");
    // A VARIADIC argument must be the last (syntax error, 42601).
    assert_eq!(
        err(&mut db, "SELECT num_nulls(VARIADIC ARRAY[1], 2)"),
        "42601"
    );
}

#[test]
fn columns_and_both_forms() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])").unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[1,NULL,3]), (2, '{}'), (3, NULL)",
    )
    .unwrap();
    // VARIADIC over a column: row 1 has one NULL element, row 2 empty → 0, row 3 NULL array → NULL.
    match execute(&mut db, "SELECT num_nulls(VARIADIC xs) FROM t ORDER BY id").unwrap() {
        Outcome::Query { rows, .. } => {
            let got: Vec<String> = rows.iter().map(|r| r[0].render()).collect();
            assert_eq!(got, vec!["1", "0", "NULL"]);
        }
        other => panic!("expected query, got {other:?}"),
    }
}
