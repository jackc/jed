//! Array function/operator surface — AF1 (spec/design/array-functions.md): the polymorphic
//! `anyarray`/`anyelement` resolution plus the introspection (`array_ndims`/`array_length`/
//! `array_lower`/`array_upper`/`cardinality`/`array_dims`) and builder (`array_append`/
//! `array_prepend`/`array_cat`) functions. Every expected value is pinned against PostgreSQL 18.

use jed::{Database, Outcome, execute};

fn run(db: &mut Database, sql: &str) {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// One-column, one-row scalar query → the rendered value (NULL renders as "NULL" via `render`).
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
fn introspection_one_dim() {
    let mut db = Database::new();
    assert_eq!(val(&mut db, "SELECT array_length(ARRAY[10,20,30], 1)"), "3");
    assert_eq!(
        val(&mut db, "SELECT array_length(ARRAY[10,20,30], 2)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_length(ARRAY[10,20,30], 0)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_length(ARRAY[10,20,30], -1)"),
        "NULL"
    );
    assert_eq!(val(&mut db, "SELECT cardinality(ARRAY[10,20,30])"), "3");
    assert_eq!(val(&mut db, "SELECT array_ndims(ARRAY[10,20,30])"), "1");
    assert_eq!(val(&mut db, "SELECT array_dims(ARRAY[10,20,30])"), "[1:3]");
    assert_eq!(val(&mut db, "SELECT array_lower(ARRAY[10,20,30], 1)"), "1");
    assert_eq!(val(&mut db, "SELECT array_upper(ARRAY[10,20,30], 1)"), "3");
}

#[test]
fn introspection_empty_array() {
    let mut db = Database::new();
    // The empty array {}: length/ndims/dims/lower/upper are NULL, cardinality is 0.
    assert_eq!(
        val(&mut db, "SELECT array_length('{}'::int32[], 1)"),
        "NULL"
    );
    assert_eq!(val(&mut db, "SELECT array_ndims('{}'::int32[])"), "NULL");
    assert_eq!(val(&mut db, "SELECT array_dims('{}'::int32[])"), "NULL");
    assert_eq!(val(&mut db, "SELECT array_lower('{}'::int32[], 1)"), "NULL");
    assert_eq!(val(&mut db, "SELECT array_upper('{}'::int32[], 1)"), "NULL");
    assert_eq!(val(&mut db, "SELECT cardinality('{}'::int32[])"), "0");
}

#[test]
fn introspection_null_array_and_dim() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_length(NULL::int32[], 1)"),
        "NULL"
    );
    assert_eq!(val(&mut db, "SELECT cardinality(NULL::int32[])"), "NULL");
    assert_eq!(val(&mut db, "SELECT array_ndims(NULL::int32[])"), "NULL");
    // A NULL dimension argument propagates to NULL. jed requires the cast (an untyped bare NULL in
    // a typed slot is 42883 — jed's existing strictness, e.g. `round(5, NULL)`; a divergence from PG).
    assert_eq!(
        val(&mut db, "SELECT array_length(ARRAY[1,2,3], NULL::int32)"),
        "NULL"
    );
}

#[test]
fn introspection_custom_lower_bound_and_multidim() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_lower('[2:4]={7,8,9}'::int32[], 1)"),
        "2"
    );
    assert_eq!(
        val(&mut db, "SELECT array_upper('[2:4]={7,8,9}'::int32[], 1)"),
        "4"
    );
    assert_eq!(
        val(&mut db, "SELECT array_dims('[2:4]={7,8,9}'::int32[])"),
        "[2:4]"
    );
    let two_d = "ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]]";
    assert_eq!(val(&mut db, &format!("SELECT array_ndims({two_d})")), "2");
    assert_eq!(
        val(&mut db, &format!("SELECT array_length({two_d}, 1)")),
        "2"
    );
    assert_eq!(
        val(&mut db, &format!("SELECT array_length({two_d}, 2)")),
        "3"
    );
    assert_eq!(val(&mut db, &format!("SELECT cardinality({two_d})")), "6");
    assert_eq!(
        val(&mut db, &format!("SELECT array_dims({two_d})")),
        "[1:2][1:3]"
    );
}

#[test]
fn builders_append_prepend() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_append(ARRAY[1,2,3], 4)"),
        "{1,2,3,4}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_prepend(0, ARRAY[1,2,3])"),
        "{0,1,2,3}"
    );
    // Non-strict: a NULL or empty array yields the singleton {e}.
    assert_eq!(val(&mut db, "SELECT array_append(NULL::int32[], 5)"), "{5}");
    assert_eq!(val(&mut db, "SELECT array_append('{}'::int32[], 5)"), "{5}");
    assert_eq!(
        val(&mut db, "SELECT array_prepend(5, '{}'::int32[])"),
        "{5}"
    );
    // A NULL element is appended as a real NULL element.
    assert_eq!(
        val(&mut db, "SELECT array_append(ARRAY[1,2], NULL)"),
        "{1,2,NULL}"
    );
    // Custom lower bounds are preserved; the opposite bound grows.
    assert_eq!(
        val(
            &mut db,
            "SELECT array_dims(array_append('[2:4]={7,8,9}'::int32[], 10))"
        ),
        "[2:5]"
    );
    assert_eq!(
        val(
            &mut db,
            "SELECT array_dims(array_prepend(6, '[2:4]={7,8,9}'::int32[]))"
        ),
        "[2:5]"
    );
}

#[test]
fn builders_cat() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_cat(ARRAY[1,2], ARRAY[3,4])"),
        "{1,2,3,4}"
    );
    // Identity: NULL/empty operand. The element type is taken from the first array argument, so a
    // typed/column array goes first (the untyped ARRAY[…] then adapts to it).
    assert_eq!(
        val(&mut db, "SELECT array_cat(NULL::int64[], ARRAY[1,2])"),
        "{1,2}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_cat(ARRAY[1,2], NULL::int64[])"),
        "{1,2}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_cat(NULL::int64[], NULL::int64[])"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_cat('{}'::int64[], '{}'::int64[])"),
        "{}"
    );
    // Multidim: 2-D ∥ 1-D appends a row.
    assert_eq!(
        val(
            &mut db,
            "SELECT array_cat(ARRAY[ARRAY[1,2],ARRAY[3,4]], ARRAY[5,6])"
        ),
        "{{1,2},{3,4},{5,6}}"
    );
    assert_eq!(
        val(
            &mut db,
            "SELECT array_cat(ARRAY[5,6], ARRAY[ARRAY[1,2],ARRAY[3,4]])"
        ),
        "{{5,6},{1,2},{3,4}}"
    );
}

#[test]
fn error_cases() {
    let mut db = Database::new();
    // array_append/prepend reject a multidimensional array (22000).
    assert_eq!(
        err(
            &mut db,
            "SELECT array_append(ARRAY[ARRAY[1,2],ARRAY[3,4]], 9)"
        ),
        "22000"
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT array_prepend(9, ARRAY[ARRAY[1,2],ARRAY[3,4]])"
        ),
        "22000"
    );
    // array_cat of incompatible dimensionalities (2202E).
    assert_eq!(
        err(
            &mut db,
            "SELECT array_cat(ARRAY[ARRAY[1,2]], ARRAY[ARRAY[3,4,5]])"
        ),
        "2202E"
    );
    // An element-type conflict matches no overload (42883).
    assert_eq!(
        err(&mut db, "SELECT array_cat(ARRAY[1,2], ARRAY['a','b'])"),
        "42883"
    );
    // A non-array where anyarray is required (42883).
    assert_eq!(err(&mut db, "SELECT array_length(5, 1)"), "42883");
    // An element that does not unify with the array's element type (42883).
    assert_eq!(
        err(&mut db, "SELECT array_append(ARRAY[1,2], 'x')"),
        "42883"
    );
}

#[test]
fn result_types_polymorphic() {
    let mut db = Database::new();
    // text[] flows through the builders; introspection returns int32/text regardless of element.
    assert_eq!(
        val(&mut db, "SELECT array_append(ARRAY['a','b'], 'c')"),
        "{a,b,c}"
    );
    assert_eq!(val(&mut db, "SELECT array_length(ARRAY['a','b'], 1)"), "2");
    assert_eq!(
        val(&mut db, "SELECT array_dims(ARRAY['a','b','c'])"),
        "[1:3]"
    );
}
