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
