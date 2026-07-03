//! Composite (row) types (spec/design/composite.md) — the full S0–S6 feature: CREATE/DROP TYPE +
//! the catalog type registry + on-disk persistence (S0–S2); storable composite columns, the
//! `ROW(…)` constructor, the recursive value codec, INSERT/SELECT round-trip, and `record_out`
//! rendering (S3); parens-required field access `(expr).field` / `(expr).*` (S4); element-wise
//! comparison / lexicographic ordering / the non-recursive all-fields `IS NULL` rule / DISTINCT /
//! GROUP BY (S5); and PG-exact `record_out` (`"`→`""` doubling) + `record_in` via `'(…)'::type` /
//! `type '(…)'` (S6).

use jed::types::Type;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// Run a query and render its rows as `Vec<Vec<String>>` (each value via `render`).
fn query(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

#[test]
fn create_type_registers_fields() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE addr AS (street text NOT NULL, zip i32)",
    );
    let ct = db.composite_type("addr").expect("type addr");
    assert_eq!(ct.name, "addr");
    assert_eq!(ct.fields.len(), 2);
    assert_eq!(ct.fields[0].name, "street");
    assert_eq!(ct.fields[0].ty, Type::Scalar(jed::types::ScalarType::Text));
    assert!(ct.fields[0].not_null);
    assert_eq!(ct.fields[1].name, "zip");
    assert!(!ct.fields[1].not_null);
    // Case-insensitive lookup.
    assert!(db.composite_type("ADDR").is_some());
}

#[test]
fn drop_type_removes_it() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (a i32)");
    run(&mut db, "DROP TYPE addr");
    assert!(db.composite_type("addr").is_none());
}

/// A nested composite value round-trips and renders with the inner record quoted.
#[test]
fn nested_composite_value_roundtrip() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE point AS (x i32, y i32)");
    run(&mut db, "CREATE TYPE seg AS (a point, b point)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s seg)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))",
    );
    assert_eq!(
        query(&mut db, "SELECT s FROM t"),
        vec![vec![r#"("(1,2)","(3,4)")"#.to_string()]]
    );
}

/// Composite values survive a serialize → load round-trip (the v9 recursive value codec).
#[test]
fn composite_values_persist_through_image() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW('Main', 90210))");
    run(&mut db, "INSERT INTO p VALUES (2, ROW('Oak', NULL))");
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .expect("reload")
        .session(SessionOptions::default());
    assert_eq!(
        query(&mut loaded, "SELECT id, home FROM p ORDER BY id"),
        vec![
            vec!["1".to_string(), "(Main,90210)".to_string()],
            vec!["2".to_string(), "(Oak,)".to_string()],
        ]
    );
}

/// S4: `(expr).field` selects one field; the output column is named after the field. Works on a
/// parenthesized column, a `ROW(…)` literal, and chains through a nested composite.
#[test]
fn field_access_selects_field() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(
        &mut db,
        "CREATE TABLE person (id i32 PRIMARY KEY, home addr)",
    );
    run(&mut db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
    // Parenthesized-column field access.
    assert_eq!(
        query(&mut db, "SELECT (home).zip, (home).street FROM person"),
        vec![vec!["90210".to_string(), "Main".to_string()]]
    );
    // Field access on an anonymous ROW(…) literal (fields named f1, f2, …), no FROM.
    assert_eq!(
        query(&mut db, "SELECT (ROW('x', 7)).f2"),
        vec![vec!["7".to_string()]]
    );
}

/// S5: composite equality is element-wise 3VL (PG row comparison). `=` is FALSE if any field is
/// FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE.
#[test]
fn composite_equality_3vl() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE rec AS (a i32, b i32)");
    // Equal rows.
    assert_eq!(
        query(&mut db, "SELECT ROW(1, 2) = ROW(1, 2)"),
        vec![vec!["true".to_string()]]
    );
    // A NULL field with all-else-equal → UNKNOWN (renders NULL).
    assert_eq!(
        query(&mut db, "SELECT ROW(1, NULL) = ROW(1, 2)"),
        vec![vec!["NULL".to_string()]]
    );
    // A FALSE field dominates a NULL field → FALSE.
    assert_eq!(
        query(&mut db, "SELECT ROW(1, NULL) = ROW(2, 2)"),
        vec![vec!["false".to_string()]]
    );
    // The 3VL negation via NOT (jed has no `<>` operator).
    assert_eq!(
        query(&mut db, "SELECT NOT (ROW(1, 2) = ROW(1, 3))"),
        vec![vec!["true".to_string()]]
    );
}

/// S5: a composite column compares against a `ROW(…)` value in WHERE (element-wise), and
/// `ORDER BY` over the composite column sorts lexicographically.
#[test]
fn composite_column_compare_and_order() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW('Oak', 30))");
    run(&mut db, "INSERT INTO p VALUES (2, ROW('Oak', 10))");
    run(&mut db, "INSERT INTO p VALUES (3, ROW('Elm', 99))");
    // WHERE composite = ROW(...).
    assert_eq!(
        query(&mut db, "SELECT id FROM p WHERE home = ROW('Oak', 10)"),
        vec![vec!["2".to_string()]]
    );
    // ORDER BY composite column — lexicographic: Elm/99, Oak/10, Oak/30.
    assert_eq!(
        query(&mut db, "SELECT id FROM p ORDER BY home"),
        vec![
            vec!["3".to_string()],
            vec!["2".to_string()],
            vec!["1".to_string()]
        ]
    );
}

/// S5: the all-fields `IS NULL` rule is ONE LEVEL DEEP, not recursive (the empirically-probed
/// PG behavior — the differential oracle). A composite-valued field is a non-NULL value, so it
/// counts as PRESENT: a nested all-NULL row is therefore `IS NULL` = FALSE (the inner rows are
/// non-null values) and `IS NOT NULL` = TRUE.
#[test]
fn composite_is_null_non_recursive() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE point AS (x i32, y i32)");
    run(&mut db, "CREATE TYPE seg AS (a point, b point)");
    // The two inner rows are non-null values → the outer row is NOT all-(SQL-)null → IS NULL false,
    // IS NOT NULL true. PG does NOT recurse into the inner all-NULL rows.
    assert_eq!(
        query(
            &mut db,
            "SELECT ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NULL, ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NOT NULL"
        ),
        vec![vec!["false".to_string(), "true".to_string()]]
    );
    // A SQL-NULL field + a composite field → IS NULL false (not all null), IS NOT NULL false
    // (the NULL field is not present).
    assert_eq!(
        query(
            &mut db,
            "SELECT ROW(NULL, ROW(1, 2)) IS NULL, ROW(NULL, ROW(1, 2)) IS NOT NULL"
        ),
        vec![vec!["false".to_string(), "false".to_string()]]
    );
}

#[test]
fn nested_type_self_or_forward_reference_is_42704() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // Forward reference (point not yet defined) — and self-reference — are unknown types.
    assert_eq!(err(&mut db, "CREATE TYPE line AS (a point)"), "42704");
    assert_eq!(err(&mut db, "CREATE TYPE t AS (a t)"), "42704");
}

/// Round-trip through the on-disk image: a composite type (and a nested one) survives
/// serialize → load, byte-backed by the v9 catalog type-definition section.
#[test]
fn types_persist_through_image() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)",
    );
    run(&mut db, "CREATE TYPE line AS (a point, b point)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");

    let image = db.to_image(256, 1).unwrap();
    let loaded = Database::from_image(&image)
        .expect("reload")
        .session(SessionOptions::default());

    let point = loaded.composite_type("point").expect("point persists");
    assert_eq!(point.fields.len(), 2);
    assert!(point.fields[0].not_null);

    let line = loaded.composite_type("line").expect("line persists");
    assert_eq!(line.fields.len(), 2);
    // A nested field references its composite by name.
    assert_eq!(
        line.fields[0].ty,
        Type::Composite(jed::types::CompositeRef {
            name: "point".to_string()
        })
    );
    // The table and its row survive too.
    assert_eq!(loaded.table("t").unwrap().columns.len(), 2);
}

// --- a composite type with an array-typed field (spec/design/array.md §12 — the mirror of an
// array-of-composite element). The catalog persists the array field as type_code 15 + the inline
// element descriptor; the value codec / comparison / text-I/O all recurse for free. ---

/// `CREATE TYPE t AS (xs i32[])` registers an array-typed field.
#[test]
fn create_type_with_array_field_registers() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE poly AS (name text, pts i32[])");
    let ct = db.composite_type("poly").expect("type poly");
    assert_eq!(ct.fields.len(), 2);
    assert_eq!(ct.fields[1].name, "pts");
    assert_eq!(
        ct.fields[1].ty,
        Type::Array(Box::new(Type::Scalar(jed::types::ScalarType::Int32)))
    );
}

/// The array field survives the on-disk image round-trip byte-for-byte (the catalog code-15 field
/// entry + the recursive value codec); the in-memory type is rebuilt as an array.
#[test]
fn composite_with_array_field_image_roundtrip() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE poly AS (name text, pts i32[])");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ROW('a', ARRAY[1, 2, 3]))",
    );
    run(&mut db, "INSERT INTO t VALUES (2, ROW('b', NULL))");
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .expect("reload")
        .session(SessionOptions::default());
    let ct = loaded.composite_type("poly").expect("poly persists");
    assert_eq!(
        ct.fields[1].ty,
        Type::Array(Box::new(Type::Scalar(jed::types::ScalarType::Int32)))
    );
    assert_eq!(
        query(&mut loaded, "SELECT id, p FROM t ORDER BY id"),
        vec![vec!["1", "(a,\"{1,2,3}\")"], vec!["2", "(b,)"]]
    );
}

/// An array-of-composite field (`CREATE TYPE t AS (homes addr[])` — the doubly-nested case): the
/// catalog field carries element code 14 + name, the value codec nests array-over-composite.
#[test]
fn composite_with_array_of_composite_field() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TYPE person AS (name text, homes addr[])");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, who person)");
    // The array-of-composite field as a text literal: array_in tokenizes the braces, then routes
    // each quoted element through record_in to build the addr value.
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ROW('jo', '{\"(Main,1)\",\"(Oak,2)\"}'))",
    );
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .expect("reload")
        .session(SessionOptions::default());
    // The persisted array-of-composite field re-resolves and the value round-trips.
    assert_eq!(
        query(&mut loaded, "SELECT (who).homes[1] FROM t WHERE id = 1"),
        vec![vec!["(Main,1)"]]
    );
}

/// `DROP TYPE addr` is blocked (2BP01) while a composite type has an `addr[]` field — the
/// dependency check looks through the array level (spec/design/array.md §12).
#[test]
fn drop_type_blocked_by_array_field_dependent() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TYPE person AS (name text, homes addr[])");
    assert_eq!(err(&mut db, "DROP TYPE addr"), "2BP01");
    // Dropping the dependent first frees it.
    run(&mut db, "DROP TYPE person");
    run(&mut db, "DROP TYPE addr");
}

/// `DROP TYPE addr` is blocked while a *table column* is `addr[]` too (the same look-through).
#[test]
fn drop_type_blocked_by_array_column_dependent() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
    assert_eq!(err(&mut db, "DROP TYPE addr"), "2BP01");
}

/// A type modifier on an array field is rejected (0A000), like an array column's.
#[test]
fn array_field_type_modifier_is_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "CREATE TYPE t AS (xs decimal(10,2)[])"),
        "0A000"
    );
}

/// An unknown element type in an array field is 42704.
#[test]
fn array_field_unknown_element_is_42704() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(err(&mut db, "CREATE TYPE t AS (xs nope[])"), "42704");
}
