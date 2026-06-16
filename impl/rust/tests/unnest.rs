//! `unnest` — the polymorphic set-returning function (AF3, spec/design/array-functions.md §9), the
//! engine's second FROM-clause SRF after generate_series. These complement the conformance corpus
//! (spec/conformance/suites/query/unnest.test) with finer-grained assertions: the generator's
//! output column name/type (the bound element type) for several element families, the NULL/empty
//! semantics, multidimensional flattening, the generated_row cost contract + the max_cost ceiling,
//! and the deferred-form / strictness errors that are NOT in the oracle corpus (the SELECT-list
//! position 42883, the bare-untyped-NULL 42P18, a wrong arity / non-array 42883).

use jed::value::Value;
use jed::{Database, Outcome, execute};

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

// ---- the generator: rows, column name, element type ----------------------------------------

#[test]
fn names_and_types_its_column_at_the_element_type() {
    let mut db = Database::new();
    // An untyped ARRAY[…] literal is int64[] (jed's literal typing), so the column is int64.
    let out = execute(&mut db, "SELECT * FROM unnest(ARRAY[10, 20, 30])").unwrap();
    match &out {
        Outcome::Query {
            column_names,
            column_types,
            ..
        } => {
            assert_eq!(column_names, &["unnest"]);
            assert_eq!(column_types, &["int64"]);
        }
        other => panic!("expected a query result, got {other:?}"),
    }
    // A typed '{…}'::int32[] literal pins the element type — the column is int32.
    let out = execute(&mut db, "SELECT * FROM unnest('{1,2,3}'::int32[])").unwrap();
    assert_eq!(out.column_types(), &["int32"]);
    // A text[] argument → a text column.
    let out = execute(&mut db, "SELECT * FROM unnest(ARRAY['a','b'])").unwrap();
    assert_eq!(out.column_types(), &["text"]);
}

// ---- NULL / empty semantics ----------------------------------------------------------------

#[test]
fn null_elements_become_null_rows() {
    let mut db = Database::new();
    // A NULL element of a non-NULL array is produced as a NULL row (ORDER BY puts it last).
    assert_eq!(
        query(
            &mut db,
            "SELECT * FROM unnest(ARRAY[1,NULL,3]) AS u ORDER BY u"
        ),
        vec![vec![Value::Int(1)], vec![Value::Int(3)], vec![Value::Null]],
    );
}

#[test]
fn empty_and_null_arrays_yield_zero_rows() {
    let mut db = Database::new();
    assert_eq!(
        query(&mut db, "SELECT * FROM unnest('{}'::int32[])"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM unnest(NULL::int32[])"),
        Vec::<Vec<Value>>::new()
    );
    // Both charge zero cost — nothing generated, nothing produced.
    assert_eq!(cost(&mut db, "SELECT * FROM unnest('{}'::int32[])"), 0);
    assert_eq!(cost(&mut db, "SELECT * FROM unnest(NULL::int32[])"), 0);
}

// ---- multidimensional flatten + custom lower bounds ----------------------------------------

#[test]
fn multidim_flattens_row_major_and_lbounds_are_dropped() {
    let mut db = Database::new();
    // A 2×2 value flattens to its 4 elements (row-major).
    assert_eq!(
        query(
            &mut db,
            "SELECT * FROM unnest(ARRAY[ARRAY[1,2],ARRAY[3,4]]) AS u ORDER BY u"
        ),
        ints(&[1, 2, 3, 4]),
    );
    // A custom lower bound is flattened away (unnest yields elements, not subscripts).
    assert_eq!(
        query(
            &mut db,
            "SELECT * FROM unnest('[5:7]={10,20,30}'::int32[]) AS u ORDER BY u"
        ),
        ints(&[10, 20, 30]),
    );
}

// ---- composition: alias, CROSS JOIN, the non-LATERAL correlated argument -------------------

#[test]
fn alias_renames_the_single_column() {
    let mut db = Database::new();
    // PG's single-column function-alias rule: `AS g` makes the column `g`, so `g.unnest` is 42703.
    assert_eq!(
        query(
            &mut db,
            "SELECT g.g FROM unnest(ARRAY[7,8]) AS g ORDER BY g.g"
        ),
        ints(&[7, 8]),
    );
    assert_eq!(
        err_code(&mut db, "SELECT g.unnest FROM unnest(ARRAY[7,8]) AS g"),
        "42703"
    );
}

#[test]
fn correlated_outer_array_column_is_a_legal_arg_but_a_sibling_is_not() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])").unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, '{30}'), (3, NULL), (4, '{}')",
    )
    .unwrap();
    // A correlated OUTER column (o.xs) resolves into the SRF arg (non-LATERAL sees params/outer).
    assert_eq!(
        query(
            &mut db,
            "SELECT id, (SELECT count(*) FROM unnest(o.xs)) AS n FROM t o ORDER BY id"
        ),
        vec![
            vec![Value::Int(1), Value::Int(2)],
            vec![Value::Int(2), Value::Int(1)],
            vec![Value::Int(3), Value::Int(0)],
            vec![Value::Int(4), Value::Int(0)],
        ],
    );
    // A SIBLING FROM table's column is NOT in scope for the SRF arg (non-LATERAL): a bare `xs`
    // does not resolve (42703), and a qualified `t.xs` finds no such FROM entry in the arg scope
    // (42P01) — both confirm the args see only params/outer, never a sibling relation.
    assert_eq!(
        err_code(&mut db, "SELECT id, u FROM t CROSS JOIN unnest(xs) AS u"),
        "42703"
    );
    assert_eq!(
        err_code(&mut db, "SELECT id, u FROM t CROSS JOIN unnest(t.xs) AS u"),
        "42P01"
    );
}

// ---- strictness + deferred-form errors (NOT in the oracle corpus) --------------------------

#[test]
fn non_array_and_wrong_arity_are_undefined_function() {
    let mut db = Database::new();
    // A non-array argument has no anyarray overload.
    assert_eq!(err_code(&mut db, "SELECT * FROM unnest(5)"), "42883");
    assert_eq!(err_code(&mut db, "SELECT * FROM unnest('hi')"), "42883");
    // unnest is single-arity (the multi-array PG form is deferred).
    assert_eq!(
        err_code(&mut db, "SELECT * FROM unnest(ARRAY[1], ARRAY[2])"),
        "42883"
    );
}

#[test]
fn bare_untyped_null_is_indeterminate_datatype() {
    let mut db = Database::new();
    // A bare untyped NULL leaves ELEM undeterminable — jed's polymorphic posture (PG would default
    // to text / report "not unique"; out of the oracle corpus). A TYPED null array resolves.
    assert_eq!(err_code(&mut db, "SELECT * FROM unnest(NULL)"), "42P18");
}

#[test]
fn select_list_srf_position_is_deferred() {
    let mut db = Database::new();
    // unnest is a FROM-clause row source only this slice; in the SELECT list it is not a scalar
    // function (the SELECT-list SRF position is deferred, like generate_series) → 42883.
    assert_eq!(err_code(&mut db, "SELECT unnest(ARRAY[1,2,3])"), "42883");
}

// ---- cost: generated_row accrual + the max_cost ceiling ------------------------------------

#[test]
fn generated_row_cost_and_ceiling() {
    let mut db = Database::new();
    // '{…}'::int32[] is a const (no operator_eval): 3 generated_row + 3 row_produced.
    assert_eq!(cost(&mut db, "SELECT * FROM unnest('{1,2,3}'::int32[])"), 6);
    // A large array aborts deterministically once accrued cost reaches the ceiling (54P01),
    // before the whole thing materializes — the guard fires mid-generation, like generate_series.
    let big = (1..=1000)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT * FROM unnest('{{{big}}}'::int32[])");
    db.set_max_cost(50);
    assert_eq!(err_code(&mut db, &sql), "54P01");
}
