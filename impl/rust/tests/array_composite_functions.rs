//! AF7 (spec/design/array-functions.md §13): the polymorphic array function/operator surface over a
//! COMPOSITE element type, plus `unnest(composite[])`. These complement the oracle corpus
//! (`suites/expr/array_composite_functions.test`, `suites/query/unnest_composite.test`) with the two
//! pieces the corpus can't carry: (a) the `ARRAY[ROW(…)]` constructor under a composite-column
//! context (a jed extension PG rejects without a `::addr` cast — the same path AC1's unit tests use),
//! and (b) finer assertions on the composite-specific NULL rules. Every expected value is pinned
//! against PostgreSQL 18.

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

/// One-column, one-row query → the rendered value ("NULL" for SQL-NULL).
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

/// A multi-row, one-column query → the rendered values.
fn col(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows.iter().map(|r| r[0].render()).collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn addr_db() -> Database {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    db
}

// ---- the builders / introspectors over composite elements (free via polymorphic resolution) ----

#[test]
fn builders_over_composite_elements() {
    let mut db = addr_db();
    // array_append / array_prepend / array_cat / || all manipulate the element list, element-agnostic.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_append('{"(a,1)"}'::addr[], '(b,2)'::addr)"#
        ),
        r#"{"(a,1)","(b,2)"}"#
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_prepend('(z,0)'::addr, '{"(a,1)"}'::addr[])"#
        ),
        r#"{"(z,0)","(a,1)"}"#
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_cat('{"(a,1)"}'::addr[], '{"(b,2)"}'::addr[])"#
        ),
        r#"{"(a,1)","(b,2)"}"#
    );
    assert_eq!(
        val(&mut db, r#"SELECT '{"(a,1)"}'::addr[] || '(b,2)'::addr"#),
        r#"{"(a,1)","(b,2)"}"#
    );
    // A NULL/empty array is the builder identity (the non-strict `none` discipline).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_append(NULL::addr[], '(a,1)'::addr)"#
        ),
        r#"{"(a,1)"}"#
    );
    // An element-type conflict is 42883 (the polymorphic unify rejects it).
    assert_eq!(
        err(
            &mut db,
            r#"SELECT array_cat('{"(a,1)"}'::addr[], ARRAY[1,2])"#
        ),
        "42883"
    );
}

#[test]
fn introspectors_over_composite_elements() {
    let mut db = addr_db();
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_length('{"(a,1)","(b,2)"}'::addr[], 1)"#
        ),
        "2"
    );
    assert_eq!(
        val(&mut db, r#"SELECT cardinality('{"(a,1)"}'::addr[])"#),
        "1"
    );
    assert_eq!(
        val(&mut db, r#"SELECT array_ndims('{"(a,1)"}'::addr[])"#),
        "1"
    );
    assert_eq!(
        val(&mut db, r#"SELECT array_lower('{"(a,1)"}'::addr[], 1)"#),
        "1"
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_upper('{"(a,1)","(b,2)"}'::addr[], 1)"#
        ),
        "2"
    );
    assert_eq!(
        val(&mut db, r#"SELECT array_dims('{"(a,1)","(b,2)"}'::addr[])"#),
        "[1:2]"
    );
    // num_nulls / num_nonnulls over a composite VARIADIC array count WHOLE-ELEMENT NULLs (a composite
    // with a NULL field is a present value).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT num_nulls(VARIADIC '{"(a,1)",NULL}'::addr[])"#
        ),
        "1"
    );
    assert_eq!(val(&mut db, r#"SELECT num_nonnulls('(a,)'::addr)"#), "1");
}

// ---- containment / overlap: strict at the value level, total-order at the field level ----

#[test]
fn containment_over_composite_elements() {
    let mut db = addr_db();
    // A present composite element is contained.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '{"(a,1)","(b,2)"}'::addr[] @> '{"(b,2)"}'::addr[]"#
        ),
        "true"
    );
    // A composite element with a NULL FIELD is comparable (record_eq) — @> matches it.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '{"(a,)"}'::addr[] @> '{"(a,)"}'::addr[]"#
        ),
        "true"
    );
    // But a WHOLE-element NULL matches nothing, including another NULL (strict).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '{"(a,1)",NULL}'::addr[] @> '{NULL}'::addr[]"#
        ),
        "false"
    );
    // <@ is the swap; && is overlap.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '{"(a,1)"}'::addr[] <@ '{"(a,1)","(b,2)"}'::addr[]"#
        ),
        "true"
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '{"(a,1)"}'::addr[] && '{"(a,1)","(b,2)"}'::addr[]"#
        ),
        "true"
    );
    // A NULL whole-array operand → NULL (strict).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT (NULL::addr[] @> '{"(a,1)"}'::addr[]) IS NULL"#
        ),
        "true"
    );
}

// ---- search/edit: NULL-safe element match (the inverse of containment's strict rule) ----

#[test]
fn search_edit_over_composite_elements() {
    let mut db = addr_db();
    // array_remove of a WHOLE-element NULL (NULL-safe — removes the NULL element).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_remove('{"(a,1)",NULL}'::addr[], NULL::addr)"#
        ),
        r#"{"(a,1)"}"#
    );
    // array_remove of a composite with a NULL field (NULL-safe — matches it).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_remove('{"(a,)","(b,2)"}'::addr[], '(a,)'::addr)"#
        ),
        r#"{"(b,2)"}"#
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_position('{"(a,1)","(b,2)"}'::addr[], '(b,2)'::addr)"#
        ),
        "2"
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_positions('{"(a,1)","(b,2)","(a,1)"}'::addr[], '(a,1)'::addr)"#
        ),
        "{1,3}"
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT array_replace('{"(a,1)"}'::addr[], '(a,1)'::addr, '(z,9)'::addr)"#
        ),
        r#"{"(z,9)"}"#
    );
}

// ---- the AF7 code change #2: x op ANY/ALL(composite[]) uses the composite TOTAL ORDER ----

#[test]
fn quantified_over_composite_uses_total_order_not_3vl() {
    let mut db = addr_db();
    // A present match.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '(b,2)'::addr = ANY('{"(a,1)","(b,2)"}'::addr[])"#
        ),
        "true"
    );
    // THE FIX: a composite NULL FIELD is comparable (PG record_eq), so = ANY is TRUE — NOT the
    // bare-ROW 3VL NULL that jed's composite `=` operator would give.
    assert_eq!(
        val(&mut db, r#"SELECT '(a,)'::addr = ANY('{"(a,)"}'::addr[])"#),
        "true"
    );
    // A NULL field that differs from a present field is a definite FALSE (no match).
    assert_eq!(
        val(&mut db, r#"SELECT '(a,)'::addr = ANY('{"(a,2)"}'::addr[])"#),
        "false"
    );
    // A WHOLE-element NULL is still UNKNOWN (the operator stays strict at the value level).
    assert_eq!(
        val(
            &mut db,
            r#"SELECT ('(a,1)'::addr = ANY('{NULL}'::addr[])) IS NULL"#
        ),
        "true"
    );
    // Ordering quantifiers use the composite total order: the NULL `zip` sorts last.
    assert_eq!(
        val(&mut db, r#"SELECT '(a,1)'::addr < ANY('{"(a,)"}'::addr[])"#),
        "true"
    );
    assert_eq!(
        val(&mut db, r#"SELECT '(a,)'::addr > ANY('{"(a,1)"}'::addr[])"#),
        "true"
    );
    // ALL with all-equal (incl. a NULL field) is TRUE.
    assert_eq!(
        val(
            &mut db,
            r#"SELECT '(a,)'::addr = ALL('{"(a,)","(a,)"}'::addr[])"#
        ),
        "true"
    );
    // An empty array → ANY FALSE, ALL TRUE (vacuous); a NULL array → NULL.
    assert_eq!(
        val(&mut db, r#"SELECT '(a,1)'::addr = ANY('{}'::addr[])"#),
        "false"
    );
    assert_eq!(
        val(&mut db, r#"SELECT '(a,1)'::addr = ALL('{}'::addr[])"#),
        "true"
    );
    assert_eq!(
        val(
            &mut db,
            r#"SELECT ('(a,1)'::addr = ANY(NULL::addr[])) IS NULL"#
        ),
        "true"
    );
}

// ---- the AF7 code change #1: unnest(composite[]) ----

#[test]
fn unnest_composite_array() {
    let mut db = addr_db();
    // One composite row per element, typed at the composite element type.
    let out = execute(
        &mut db,
        r#"SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])"#,
    )
    .unwrap();
    match &out {
        Outcome::Query {
            column_names,
            column_types,
            ..
        } => {
            assert_eq!(column_names, &["unnest"]);
            assert_eq!(column_types, &["addr"]);
        }
        other => panic!("expected a query, got {other:?}"),
    }
    assert_eq!(
        col(
            &mut db,
            r#"SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])"#
        ),
        vec!["(a,1)", "(b,2)"]
    );
    // A NULL element is produced as a NULL row; the empty / NULL array yield zero rows.
    assert_eq!(
        col(&mut db, r#"SELECT * FROM unnest('{"(a,1)",NULL}'::addr[])"#),
        vec!["(a,1)", "NULL"]
    );
    assert_eq!(
        val(&mut db, r#"SELECT count(*) FROM unnest('{}'::addr[])"#),
        "0"
    );
    assert_eq!(
        val(&mut db, r#"SELECT count(*) FROM unnest(NULL::addr[])"#),
        "0"
    );
    // Field access into the composite output column; a qualified whole-row reference.
    assert_eq!(
        col(
            &mut db,
            r#"SELECT (u).zip FROM unnest('{"(a,1)","(b,2)"}'::addr[]) AS u"#
        ),
        vec!["1", "2"]
    );
    // ORDER BY the whole composite column (the composite total order).
    assert_eq!(
        col(
            &mut db,
            r#"SELECT * FROM unnest('{"(b,2)","(a,1)"}'::addr[]) AS u ORDER BY u"#
        ),
        vec!["(a,1)", "(b,2)"]
    );
}

// ---- the jed extension path: ARRAY[ROW(…)] under a composite-column context (not in the PG corpus) ----

#[test]
fn array_row_constructor_under_column_context() {
    let mut db = addr_db();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, items addr[])",
    );
    // The ARRAY[ROW(…)] constructor takes the column's composite element type as context — no `::addr`
    // cast needed (PG requires the cast; this is the documented jed extension, like AC1).
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[ROW('Main', 90210), ROW('Side', 5)])",
    );
    // unnest a stored composite array via a correlated subquery (non-LATERAL: a sibling table is not a
    // legal arg, an OUTER column is).
    assert_eq!(
        col(
            &mut db,
            "SELECT (SELECT count(*) FROM unnest(o.items)) FROM t o ORDER BY id"
        ),
        vec!["2"]
    );
    // The function surface over the stored column. The OTHER operand must be a typed composite
    // literal ('{…}'::addr[] / '(…)'::addr) — a bare ARRAY[ROW(…)] / ROW(…) does not adapt to the
    // column's composite element type (elem_scalar_hint is None for a composite; a `::addr` cast is
    // the deferred composite-cast 0A000), exactly as in PostgreSQL the bare form is `record`.
    assert_eq!(
        col(&mut db, "SELECT array_length(items, 1) FROM t ORDER BY id"),
        vec!["2"]
    );
    assert_eq!(
        col(
            &mut db,
            r#"SELECT items @> '{"(Side,5)"}'::addr[] FROM t ORDER BY id"#
        ),
        vec!["true"]
    );
    assert_eq!(
        col(
            &mut db,
            r#"SELECT '(Side,5)'::addr = ANY(items) FROM t ORDER BY id"#
        ),
        vec!["true"]
    );
}

#[test]
fn unnest_non_array_is_undefined_function() {
    let mut db = addr_db();
    // A non-array argument is still 42883 (the polymorphic posture is unchanged).
    assert_eq!(err(&mut db, "SELECT * FROM unnest('(a,1)'::addr)"), "42883");
}
