//! Array function/operator surface — AF4 (spec/design/array-functions.md §10): the containment /
//! overlap operators `@>` (contains), `<@` (contained by), `&&` (overlaps). Every expected value is
//! pinned against PostgreSQL 18 (the strict-element-equality NULL rule especially — §10.1 #1).
//!
//! jed types a bare integer literal / `ARRAY[…]` constructor as `i64`, so the tests pair bare
//! arrays with `i64[]` casts (matching element types); the element hint comes from the FIRST array
//! operand (§5 #8), so a typed array adapts a bare-literal sibling only when it is the left operand.

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn err(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// One-column, one-row scalar query → the rendered value (NULL renders as "NULL").
fn val(db: &mut Session, sql: &str) -> String {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
    {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql}: expected one row");
            assert_eq!(rows[0].len(), 1, "{sql}: expected one column");
            rows[0][0].render()
        }
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

#[test]
fn contains_basic() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2,3] @> ARRAY[2]"), "true");
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2,3] @> ARRAY[2,4]"), "false");
    // Order and duplicates are irrelevant (set semantics).
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2,3] @> ARRAY[3,2,1]"), "true");
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2,2,3] @> ARRAY[2,2,2]"),
        "true"
    );
    // The empty array is contained by anything; the empty container contains nothing non-empty.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2,3] @> '{}'::i64[]"), "true");
    assert_eq!(val(&mut db, "SELECT '{}'::i64[] @> ARRAY[1]"), "false");
    assert_eq!(val(&mut db, "SELECT '{}'::i64[] @> '{}'::i64[]"), "true");
    // text[] flows through the polymorphic operator too.
    assert_eq!(
        val(&mut db, "SELECT ARRAY['a','b','c'] @> ARRAY['b']"),
        "true"
    );
}

#[test]
fn contained_by_is_swapped_contains() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(val(&mut db, "SELECT ARRAY[2] <@ ARRAY[1,2,3]"), "true");
    assert_eq!(val(&mut db, "SELECT ARRAY[2,4] <@ ARRAY[1,2,3]"), "false");
    assert_eq!(val(&mut db, "SELECT '{}'::i64[] <@ ARRAY[1]"), "true");
}

#[test]
fn overlaps_basic() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] && ARRAY[2,3]"), "true");
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] && ARRAY[3,4]"), "false");
    // The empty array overlaps nothing.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] && '{}'::i64[]"), "false");
}

#[test]
fn strict_null_element_matching() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A non-NULL element is found past a NULL element in the container.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2,NULL] @> ARRAY[2]"), "true");
    // STRICT equality — a NULL element matches NOTHING, including another NULL (the inverse of the
    // search/edit functions' NOT DISTINCT FROM). All of these are FALSE, never NULL.
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2,NULL] @> '{NULL}'::i64[]"),
        "false"
    );
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2,3] @> '{NULL}'::i64[]"),
        "false"
    );
    assert_eq!(
        val(&mut db, "SELECT '{NULL,NULL}'::i64[] @> '{NULL}'::i64[]"),
        "false"
    );
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,NULL] && '{NULL}'::i64[]"),
        "false"
    );
    // Overlap still finds a shared non-NULL element alongside NULLs.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,NULL] && ARRAY[1]"), "true");
}

#[test]
fn null_whole_array_propagates() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A NULL whole-array operand → NULL (strict / propagates), unlike the non-strict builders.
    assert_eq!(val(&mut db, "SELECT NULL::i64[] @> ARRAY[1]"), "NULL");
    assert_eq!(val(&mut db, "SELECT ARRAY[1] @> NULL::i64[]"), "NULL");
    assert_eq!(val(&mut db, "SELECT NULL::i64[] && ARRAY[1]"), "NULL");
    assert_eq!(val(&mut db, "SELECT ARRAY[1] <@ NULL::i64[]"), "NULL");
}

#[test]
fn literal_adaptation_to_element_type() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // The untyped `ARRAY[…]` constructor adapts to the typed (i32[]) array's element type when the
    // typed array is the LEFT operand (the element hint comes from the first array operand, §5 #8).
    assert_eq!(val(&mut db, "SELECT '{1,2,3}'::i32[] @> ARRAY[2]"), "true");
    assert_eq!(val(&mut db, "SELECT '{2}'::i32[] <@ ARRAY[1,2,3]"), "true");
}

#[test]
fn precedence_and_associativity() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // @> shares ||'s precedence rung (left-assoc, tighter than `=`): `a || b @> c` is `(a||b) @> c`.
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2] || ARRAY[3] @> ARRAY[3]"),
        "true"
    );
    // @> binds looser than `+`: only the array operands are involved.
    assert_eq!(val(&mut db, "SELECT ARRAY[3] @> ARRAY[1 + 2]"), "true");
    // @> binds tighter than `=`: `a @> b = c` is `(a @> b) = c`.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] @> ARRAY[2] = true"), "true");
}

#[test]
fn type_errors() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A non-array operand or an element-type mismatch is 42883.
    assert_eq!(err(&mut db, "SELECT 5 @> ARRAY[1]"), "42883");
    assert_eq!(err(&mut db, "SELECT ARRAY[1] @> 5"), "42883");
    assert_eq!(err(&mut db, "SELECT ARRAY[1,2] @> ARRAY['a','b']"), "42883");
    assert_eq!(err(&mut db, "SELECT ARRAY[1] && 5"), "42883");
}

#[test]
fn lexing_lone_punctuation() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A lone `@` / `&` is a 42601 syntax error (jed has no unary-@ / bitwise-and).
    assert_eq!(err(&mut db, "SELECT 1 @ 2"), "42601");
    assert_eq!(err(&mut db, "SELECT 1 & 2"), "42601");
}
