//! Array function/operator surface — AF5 (spec/design/array-functions.md §11): the `ANY`/`ALL`/`SOME`
//! quantified array comparisons (`x = ANY(arr)`, `x op ALL(arr)`), the array spelling of `IN` and its
//! universal dual. Every expected value is pinned against PostgreSQL 18 (the three-valued NULL rules
//! especially — a NULL element / a NULL `x` / an empty array / a NULL array).
//!
//! jed types a bare integer literal / `ARRAY[…]` constructor as `i64`, so the bare cases use
//! `i64`; column adaptation (`i32` column vs a bare `ARRAY[…]`) is exercised via a table.

use jed::{Engine, Outcome, execute};

fn err(db: &mut Engine, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// One-column, one-row scalar query → the rendered value (NULL renders as "NULL").
fn val(db: &mut Engine, sql: &str) -> String {
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
fn any_equality_is_in() {
    let mut db = Engine::new();
    // x = ANY(arr) is x IN (the elements).
    assert_eq!(val(&mut db, "SELECT 1 = ANY(ARRAY[1,2,3])"), "true");
    assert_eq!(val(&mut db, "SELECT 5 = ANY(ARRAY[1,2,3])"), "false");
    // SOME is the SQL-standard synonym for ANY.
    assert_eq!(val(&mut db, "SELECT 2 = SOME(ARRAY[1,2,3])"), "true");
    // The '{…}'::T[] literal operand resolves too.
    assert_eq!(val(&mut db, "SELECT 2 = ANY('{1,2,3}'::i64[])"), "true");
    // The SUBQUERY operand form is the subquery spelling of IN: `x = ANY(SELECT …)` ≡
    // `x IN (SELECT …)` (shipped; thorough coverage in suites/subquery/quantified.test).
    assert_eq!(val(&mut db, "SELECT 1 = ANY(SELECT 1)"), "true");
}

#[test]
fn all_universal() {
    let mut db = Engine::new();
    assert_eq!(val(&mut db, "SELECT 3 = ALL(ARRAY[3,3,3])"), "true");
    assert_eq!(val(&mut db, "SELECT 3 = ALL(ARRAY[3,3,4])"), "false");
    // A FALSE element dominates a NULL element → FALSE; otherwise a NULL → NULL.
    assert_eq!(val(&mut db, "SELECT 3 = ALL(ARRAY[4,NULL])"), "false");
    assert_eq!(val(&mut db, "SELECT 3 = ALL(ARRAY[3,NULL])"), "NULL");
    // Empty array → TRUE (vacuous), even for a NULL x; NULL array → NULL.
    assert_eq!(val(&mut db, "SELECT 3 = ALL('{}'::i64[])"), "true");
    assert_eq!(val(&mut db, "SELECT NULL::i64 = ALL('{}'::i64[])"), "true");
    assert_eq!(val(&mut db, "SELECT 3 = ALL(NULL::i64[])"), "NULL");
}

#[test]
fn ordering_operators() {
    let mut db = Engine::new();
    assert_eq!(val(&mut db, "SELECT 5 < ANY(ARRAY[1,2,10])"), "true");
    assert_eq!(val(&mut db, "SELECT 5 > ALL(ARRAY[1,2,3])"), "true");
    assert_eq!(val(&mut db, "SELECT 5 <= ALL(ARRAY[5,6,7])"), "true");
    assert_eq!(val(&mut db, "SELECT 5 >= ANY(ARRAY[9,8,5])"), "true");
    assert_eq!(val(&mut db, "SELECT 5 > ALL(ARRAY[1,2,9])"), "false");
}

#[test]
fn flattens_multidim_and_custom_lbounds() {
    let mut db = Engine::new();
    // The comparison is over the FLATTENED element multiset (any dimensionality).
    assert_eq!(
        val(&mut db, "SELECT 3 = ANY(ARRAY[ARRAY[1,2],ARRAY[3,4]])"),
        "true"
    );
    assert_eq!(
        val(&mut db, "SELECT 4 = ALL(ARRAY[ARRAY[4,4],ARRAY[4,4]])"),
        "true"
    );
    // A custom lower bound is irrelevant (elements, not subscripts).
    assert_eq!(
        val(&mut db, "SELECT 20 = ANY('[5:6]={10,20}'::i64[])"),
        "true"
    );
}

#[test]
fn text_elements() {
    let mut db = Engine::new();
    assert_eq!(val(&mut db, "SELECT 'b' = ANY(ARRAY['a','b','c'])"), "true");
    assert_eq!(val(&mut db, "SELECT 'z' = ALL(ARRAY['z','z'])"), "true");
}

#[test]
fn column_literal_adaptation() {
    let mut db = Engine::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[])").unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10,20,30]), (2, ARRAY[40,50])",
    )
    .unwrap();
    // A bare integer literal adapts to the i32 element type; a bare ARRAY[…] adapts to a column.
    assert_eq!(
        val(&mut db, "SELECT 20 = ANY(xs) FROM t WHERE id = 1"),
        "true"
    );
    assert_eq!(
        val(&mut db, "SELECT count(*) FROM t WHERE 20 = ANY(xs)"),
        "1"
    );
    // A scalar i32 column vs a bare ARRAY[…] (the constructor adapts to the column's element type).
    assert_eq!(
        val(&mut db, "SELECT count(*) FROM t WHERE id = ANY(ARRAY[1,2])"),
        "2"
    );
}

#[test]
fn errors() {
    let mut db = Engine::new();
    // A non-array right side is 42809 (op ANY/ALL (array) requires array on right side).
    assert_eq!(err(&mut db, "SELECT 1 = ANY(5)"), "42809");
    // An incomparable element type is 42883 (operator does not exist).
    assert_eq!(err(&mut db, "SELECT 1 = ANY(ARRAY['a','b'])"), "42883");
    // A bare untyped NULL array operand is 42P18 (jed's indeterminate posture).
    assert_eq!(err(&mut db, "SELECT 1 = ANY(NULL)"), "42P18");
}
