//! Array function/operator surface — AF2 (spec/design/array-functions.md §8): the `||`
//! concatenation operator and the search/edit functions `array_remove`, `array_replace`,
//! `array_position`, `array_positions`. Every expected value is pinned against PostgreSQL 18.

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
fn concat_three_forms() {
    let mut db = Database::new();
    // array || array → array_cat; array || element → array_append; element || array → array_prepend.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] || ARRAY[3,4]"), "{1,2,3,4}");
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] || 3"), "{1,2,3}");
    assert_eq!(val(&mut db, "SELECT 0 || ARRAY[1,2]"), "{0,1,2}");
    // text[] flows through the polymorphic operator too.
    assert_eq!(val(&mut db, "SELECT ARRAY['a','b'] || 'c'"), "{a,b,c}");
    // The literal element / untyped constructor adapts to a narrower element type (int32[]).
    assert_eq!(val(&mut db, "SELECT '{1,2}'::int32[] || 3"), "{1,2,3}");
    assert_eq!(
        val(&mut db, "SELECT '{1,2}'::int32[] || ARRAY[7,8]"),
        "{1,2,7,8}"
    );
    // 2-D || 1-D stacks a row (array_cat along the outer dimension).
    assert_eq!(
        val(&mut db, "SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] || ARRAY[5,6]"),
        "{{1,2},{3,4},{5,6}}"
    );
}

#[test]
fn concat_null_prefers_cat() {
    let mut db = Database::new();
    // A BARE untyped NULL operand resolves to array_cat (the NULL array is the identity) — PG.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] || NULL"), "{1,2}");
    assert_eq!(val(&mut db, "SELECT NULL || ARRAY[1,2]"), "{1,2}");
    // A TYPED null array is likewise the cat identity.
    assert_eq!(val(&mut db, "SELECT ARRAY[1,2] || NULL::int64[]"), "{1,2}");
    // A TYPED null ELEMENT instead resolves to array_append — appended as a real NULL element.
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2] || NULL::int64"),
        "{1,2,NULL}"
    );
    // Both NULL arrays → NULL.
    assert_eq!(
        val(&mut db, "SELECT NULL::int64[] || NULL::int64[]"),
        "NULL"
    );
}

#[test]
fn concat_precedence_and_assoc() {
    let mut db = Database::new();
    // || binds tighter than `=`: `a || b = c` is `(a || b) = c`.
    assert_eq!(
        val(&mut db, "SELECT ARRAY[1,2] || ARRAY[3] = ARRAY[1,2,3]"),
        "true"
    );
    // Left-associative chaining: append then append; prepend then append.
    assert_eq!(val(&mut db, "SELECT ARRAY[1] || 2 || 3"), "{1,2,3}");
    assert_eq!(val(&mut db, "SELECT 0 || ARRAY[1,2] || 3"), "{0,1,2,3}");
}

#[test]
fn concat_errors() {
    let mut db = Database::new();
    // Element-type conflict / non-array / text||text — no overload (42883).
    assert_eq!(err(&mut db, "SELECT ARRAY[1,2] || ARRAY['a','b']"), "42883");
    assert_eq!(err(&mut db, "SELECT 5 || ARRAY['a','b']"), "42883");
    assert_eq!(err(&mut db, "SELECT 1 || 2"), "42883");
    // array || element on a multidimensional array → 22000 (array_append rule).
    assert_eq!(
        err(&mut db, "SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] || 9"),
        "22000"
    );
    // array || array of incompatible dimensionalities → 2202E (array_cat rule).
    assert_eq!(
        err(&mut db, "SELECT ARRAY[ARRAY[1,2]] || ARRAY[ARRAY[3,4,5]]"),
        "2202E"
    );
}

#[test]
fn array_remove_kernel() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_remove(ARRAY[1,2,3,2], 2)"),
        "{1,3}"
    );
    // NULL array → NULL; not found → unchanged; empty → empty.
    assert_eq!(
        val(&mut db, "SELECT array_remove(NULL::int32[], 2)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_remove(ARRAY[1,2,3], 9)"),
        "{1,2,3}"
    );
    assert_eq!(val(&mut db, "SELECT array_remove('{}'::int32[], 1)"), "{}");
    // NULL-safe: removing NULL drops NULL elements; removing a value keeps NULLs.
    assert_eq!(
        val(&mut db, "SELECT array_remove(ARRAY[1,NULL,2,NULL], NULL)"),
        "{1,2}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_remove(ARRAY[1,NULL,2], 1)"),
        "{NULL,2}"
    );
    // The lower bound is preserved (a removal shrinks the upper bound).
    assert_eq!(
        val(
            &mut db,
            "SELECT array_dims(array_remove('[2:4]={1,2,3}'::int32[], 2))"
        ),
        "[2:3]"
    );
    // All removed → the empty array.
    assert_eq!(
        val(&mut db, "SELECT array_remove('[5:7]={9,9,9}'::int32[], 9)"),
        "{}"
    );
    // Multidimensional → 0A000.
    assert_eq!(
        err(
            &mut db,
            "SELECT array_remove(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)"
        ),
        "0A000"
    );
}

#[test]
fn array_replace_kernel() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_replace(ARRAY[1,2,3,2], 2, 9)"),
        "{1,9,3,9}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_replace(NULL::int32[], 2, 9)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_replace(ARRAY[1,2,3], 8, 9)"),
        "{1,2,3}"
    );
    // Replace TO NULL and FROM NULL (NULL-safe match).
    assert_eq!(
        val(&mut db, "SELECT array_replace(ARRAY[1,2,3], 2, NULL)"),
        "{1,NULL,3}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_replace(ARRAY[1,NULL,3], NULL, 9)"),
        "{1,9,3}"
    );
    // Works on a multidimensional array (shape preserved).
    assert_eq!(
        val(
            &mut db,
            "SELECT array_replace(ARRAY[ARRAY[1,2],ARRAY[1,4]], 1, 0)"
        ),
        "{{0,2},{0,4}}"
    );
    // Custom lower bound preserved (array_out prints the `[l:u]=` prefix).
    assert_eq!(
        val(
            &mut db,
            "SELECT array_replace('[5:7]={10,20,10}'::int32[], 10, 99)"
        ),
        "[5:7]={99,20,99}"
    );
}

#[test]
fn array_position_kernel() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_position(ARRAY[10,20,30,20], 20)"),
        "2"
    );
    assert_eq!(
        val(&mut db, "SELECT array_position(ARRAY[10,20], 99)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_position(NULL::int32[], 5)"),
        "NULL"
    );
    assert_eq!(
        val(&mut db, "SELECT array_position('{}'::int32[], 5)"),
        "NULL"
    );
    // NULL-safe: finds a NULL element.
    assert_eq!(
        val(&mut db, "SELECT array_position(ARRAY[1,NULL,3], NULL)"),
        "2"
    );
    // The optional start subscript; the result is a SUBSCRIPT, not an offset.
    assert_eq!(
        val(&mut db, "SELECT array_position(ARRAY[10,20,30,20], 20, 3)"),
        "4"
    );
    assert_eq!(
        val(&mut db, "SELECT array_position(ARRAY[10,20,30], 20, 3)"),
        "NULL"
    );
    // Subscript space honors a custom lower bound.
    assert_eq!(
        val(
            &mut db,
            "SELECT array_position('[5:7]={10,20,30}'::int32[], 20)"
        ),
        "6"
    );
    assert_eq!(
        val(
            &mut db,
            "SELECT array_position('[5:8]={10,20,30,20}'::int32[], 20, 7)"
        ),
        "8"
    );
    // A NULL start (typed) → 22004; a multidimensional array → 0A000.
    assert_eq!(
        err(
            &mut db,
            "SELECT array_position(ARRAY[10,20,30], 20, NULL::int32)"
        ),
        "22004"
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT array_position(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)"
        ),
        "0A000"
    );
}

#[test]
fn array_positions_kernel() {
    let mut db = Database::new();
    assert_eq!(
        val(&mut db, "SELECT array_positions(ARRAY[10,20,30,20], 20)"),
        "{2,4}"
    );
    // Not found → the empty array {}; NULL array → NULL.
    assert_eq!(
        val(&mut db, "SELECT array_positions(ARRAY[10,20], 99)"),
        "{}"
    );
    assert_eq!(
        val(&mut db, "SELECT array_positions(NULL::int32[], 5)"),
        "NULL"
    );
    // NULL-safe: every NULL element's subscript.
    assert_eq!(
        val(
            &mut db,
            "SELECT array_positions(ARRAY[1,NULL,3,NULL], NULL)"
        ),
        "{2,4}"
    );
    // Subscript space honors a custom lower bound.
    assert_eq!(
        val(
            &mut db,
            "SELECT array_positions('[5:8]={10,20,30,20}'::int32[], 20)"
        ),
        "{6,8}"
    );
    // Multidimensional → 0A000.
    assert_eq!(
        err(
            &mut db,
            "SELECT array_positions(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)"
        ),
        "0A000"
    );
}
