//! Composite (row) types (spec/design/composite.md) — the full S0–S6 feature: CREATE/DROP TYPE +
//! the catalog type registry + on-disk persistence (S0–S2); storable composite columns, the
//! `ROW(…)` constructor, the recursive value codec, INSERT/SELECT round-trip, and `record_out`
//! rendering (S3); parens-required field access `(expr).field` / `(expr).*` (S4); element-wise
//! comparison / lexicographic ordering / the non-recursive all-fields `IS NULL` rule / DISTINCT /
//! GROUP BY (S5); and PG-exact `record_out` (`"`→`""` doubling) + `record_in` via `'(…)'::type` /
//! `type '(…)'` (S6).

use jed::types::Type;
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

/// Run a query and render its rows as `Vec<Vec<String>>` (each value via `render`).
fn query(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

#[test]
fn create_type_registers_fields() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TYPE addr AS (street text NOT NULL, zip int32)",
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
fn duplicate_type_name_is_42710() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (a int32)");
    assert_eq!(err(&mut db, "CREATE TYPE addr AS (b int32)"), "42710");
}

#[test]
fn unknown_field_type_is_42704() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "CREATE TYPE t AS (a nosuchtype)"), "42704");
}

#[test]
fn duplicate_field_name_is_42701() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "CREATE TYPE t AS (a int32, a int64)"), "42701");
}

#[test]
fn drop_type_removes_it() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (a int32)");
    run(&mut db, "DROP TYPE addr");
    assert!(db.composite_type("addr").is_none());
}

#[test]
fn drop_missing_type_is_42704_unless_if_exists() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "DROP TYPE nope"), "42704");
    run(&mut db, "DROP TYPE IF EXISTS nope"); // no-op success
}

#[test]
fn drop_type_with_dependent_field_is_2bp01() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE point AS (x int32, y int32)");
    run(&mut db, "CREATE TYPE line AS (a point, b point)");
    // `point` is referenced by `line`'s fields.
    assert_eq!(err(&mut db, "DROP TYPE point"), "2BP01");
    // Dropping the dependent first frees it.
    run(&mut db, "DROP TYPE line");
    run(&mut db, "DROP TYPE point");
}

/// S3: a composite column is storable. `ROW(…)` INSERT then `SELECT` round-trips the value and
/// `record_out` renders it `(Main,90210)`.
#[test]
fn composite_column_row_roundtrip() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(
        &mut db,
        "CREATE TABLE person (id int32 PRIMARY KEY, home addr)",
    );
    run(&mut db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
    assert_eq!(
        query(&mut db, "SELECT id, home FROM person"),
        vec![vec!["1".to_string(), "(Main,90210)".to_string()]]
    );
}

/// A composite `PRIMARY KEY` stays rejected (`0A000`) — the key encoding is authored but
/// unexercised (spec/design/composite.md §6).
#[test]
fn composite_primary_key_is_0a000() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (a int32)");
    assert_eq!(
        err(&mut db, "CREATE TABLE t (home addr PRIMARY KEY)"),
        "0A000"
    );
}

/// `record_out` field quoting (spec/design/composite.md §8, PG-exact): a field containing a
/// delimiter / quote / whitespace is double-quoted; inside the quotes PostgreSQL **doubles** an
/// embedded `"` → `""` and `\` → `\\` (NOT backslash-escaping). A NULL field is empty; the empty
/// string is `""`.
#[test]
fn record_out_quoting_and_nulls() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE rec AS (a text, b int32)");
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, r rec)");
    run(&mut db, "INSERT INTO t VALUES (1, ROW('a b', 1))"); // space → quoted
    run(&mut db, "INSERT INTO t VALUES (2, ROW('x,y', 2))"); // comma → quoted
    run(&mut db, "INSERT INTO t VALUES (3, ROW('', 3))"); // empty string → quoted ""
    run(&mut db, "INSERT INTO t VALUES (4, ROW('q\"s', 4))"); // embedded quote → doubled
    run(&mut db, "INSERT INTO t VALUES (5, ROW('plain', NULL))"); // NULL field → empty
    run(&mut db, "INSERT INTO t VALUES (6, ROW('a\\b', 7))"); // embedded backslash → doubled
    let rows = query(&mut db, "SELECT r FROM t ORDER BY id");
    assert_eq!(rows[0][0], r#"("a b",1)"#);
    assert_eq!(rows[1][0], r#"("x,y",2)"#);
    assert_eq!(rows[2][0], r#"("",3)"#);
    assert_eq!(rows[3][0], r#"("q""s",4)"#); // PG: doubled quote
    assert_eq!(rows[4][0], "(plain,)");
    assert_eq!(rows[5][0], r#"("a\\b",7)"#); // PG: doubled backslash
}

/// S6: `record_in` round-trips `record_out`. A `'(…)'::type` cast and the `type '(…)'` typed
/// literal parse a composite text literal back into the value (the inverse of `record_out`).
#[test]
fn record_in_roundtrip() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    // The cast spelling and the typed-literal spelling are equivalent.
    assert_eq!(
        query(&mut db, "SELECT '(Main,90210)'::addr"),
        vec![vec!["(Main,90210)".to_string()]]
    );
    assert_eq!(
        query(&mut db, "SELECT addr '(Main,90210)'"),
        vec![vec!["(Main,90210)".to_string()]]
    );
    // Quoted field with comma; unquoted-empty → NULL; quoted-empty → empty string; doubled quote.
    assert_eq!(
        query(&mut db, "SELECT '(\"x,y\",2)'::addr"),
        vec![vec![r#"("x,y",2)"#.to_string()]]
    );
    assert_eq!(
        query(&mut db, "SELECT ('(,5)'::addr).street IS NULL"),
        vec![vec!["true".to_string()]]
    );
    // Field access on a parsed literal pulls the coerced field value.
    assert_eq!(
        query(&mut db, "SELECT ('(Main,90210)'::addr).zip"),
        vec![vec!["90210".to_string()]]
    );
}

/// S6: a nested composite text literal parses recursively (the inner record is a quoted token).
#[test]
fn record_in_nested() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE point AS (x int32, y int32)");
    run(&mut db, "CREATE TYPE seg AS (a point, b point)");
    assert_eq!(
        query(&mut db, r#"SELECT '("(1,2)","(3,4)")'::seg"#),
        vec![vec![r#"("(1,2)","(3,4)")"#.to_string()]]
    );
}

/// S6 errors: a malformed composite literal / wrong field count is 22P02; a bad field value
/// surfaces that field's parse error (e.g. 22P02 for a non-integer zip).
#[test]
fn record_in_errors() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    assert_eq!(err(&mut db, "SELECT '(Main)'::addr"), "22P02"); // too few fields
    assert_eq!(err(&mut db, "SELECT '(a,b,c)'::addr"), "22P02"); // too many fields
    assert_eq!(err(&mut db, "SELECT 'not a record'::addr"), "22P02"); // no parens
    assert_eq!(err(&mut db, "SELECT '(Main,notanint)'::addr"), "22P02"); // bad field
}

/// A nested composite value round-trips and renders with the inner record quoted.
#[test]
fn nested_composite_value_roundtrip() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE point AS (x int32, y int32)");
    run(&mut db, "CREATE TYPE seg AS (a point, b point)");
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, s seg)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))",
    );
    assert_eq!(
        query(&mut db, "SELECT s FROM t"),
        vec![vec![r#"("(1,2)","(3,4)")"#.to_string()]]
    );
}

/// A whole-value-NULL composite column stores and renders as NULL.
#[test]
fn whole_composite_null() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO t (id) VALUES (1)"); // home omitted → NULL
    assert_eq!(
        query(&mut db, "SELECT home FROM t"),
        vec![vec!["NULL".to_string()]]
    );
}

/// Composite values survive a serialize → load round-trip (the v9 recursive value codec).
#[test]
fn composite_values_persist_through_image() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(&mut db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW('Main', 90210))");
    run(&mut db, "INSERT INTO p VALUES (2, ROW('Oak', NULL))");
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image).expect("reload");
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
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(
        &mut db,
        "CREATE TABLE person (id int32 PRIMARY KEY, home addr)",
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

/// S4: field access on a column is **parens-required** (PostgreSQL): `(home).zip` and
/// `(t.home).zip` work; the unparenthesized `home.zip` / `t.home.zip` are NOT field access — they
/// resolve as (multi-part) column references and fail (`home` is no relation → 42P01). A bare
/// qualified column `person.home` (no field) reads the whole composite column.
#[test]
fn field_access_requires_parens() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(
        &mut db,
        "CREATE TABLE person (id int32 PRIMARY KEY, home addr)",
    );
    run(&mut db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
    // `(home).zip`: parenthesized base → field access.
    assert_eq!(
        query(&mut db, "SELECT (home).zip FROM person"),
        vec![vec!["90210".to_string()]]
    );
    // `person.home`: `person` IS the relation → reads the whole composite column.
    assert_eq!(
        query(&mut db, "SELECT person.home FROM person"),
        vec![vec!["(Main,90210)".to_string()]]
    );
    // `(t.home).zip`: parenthesized qualified column → field access.
    assert_eq!(
        query(&mut db, "SELECT (t.home).zip FROM person t"),
        vec![vec!["90210".to_string()]]
    );
    // Unparenthesized `home.zip`: `home` is no relation → 42P01 (NOT field access — PG-exact).
    assert_eq!(err(&mut db, "SELECT home.zip FROM person"), "42P01");
}

/// S4: `(expr).*` expands a composite into one output column per field, in declaration order.
#[test]
fn field_star_expands_all_fields() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(
        &mut db,
        "CREATE TABLE person (id int32 PRIMARY KEY, home addr)",
    );
    run(&mut db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
    assert_eq!(
        query(&mut db, "SELECT id, (home).* FROM person"),
        vec![vec![
            "1".to_string(),
            "Main".to_string(),
            "90210".to_string()
        ]]
    );
}

/// S4 errors: an unknown field is 42703; field access on a non-composite is 42809.
#[test]
fn field_access_errors() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(
        &mut db,
        "CREATE TABLE person (id int32 PRIMARY KEY, home addr)",
    );
    run(&mut db, "INSERT INTO person VALUES (1, ROW('Main', 90210))");
    assert_eq!(err(&mut db, "SELECT (home).nope FROM person"), "42703");
    assert_eq!(err(&mut db, "SELECT (id).zip FROM person"), "42809");
    // A bare qualifier that is neither a relation nor a column is still a missing-FROM-entry (42P01).
    assert_eq!(err(&mut db, "SELECT nosuch.col FROM person"), "42P01");
}

/// S5: composite equality is element-wise 3VL (PG row comparison). `=` is FALSE if any field is
/// FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE.
#[test]
fn composite_equality_3vl() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE rec AS (a int32, b int32)");
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

/// S5: composite ordering `< <= > >=` is lexicographic — the first non-equal field decides.
#[test]
fn composite_ordering_lexicographic() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE rec AS (a int32, b int32)");
    assert_eq!(
        query(&mut db, "SELECT ROW(1, 2) < ROW(1, 3)"),
        vec![vec!["true".to_string()]]
    );
    assert_eq!(
        query(&mut db, "SELECT ROW(2, 1) < ROW(1, 9)"),
        vec![vec!["false".to_string()]]
    );
    assert_eq!(
        query(&mut db, "SELECT ROW(1, 2) >= ROW(1, 2)"),
        vec![vec!["true".to_string()]]
    );
}

/// S5: a composite column compares against a `ROW(…)` value in WHERE (element-wise), and
/// `ORDER BY` over the composite column sorts lexicographically.
#[test]
fn composite_column_compare_and_order() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(&mut db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
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

/// S5: PG's all-fields `IS NULL` / `IS NOT NULL` rule — they are NOT negations. A partially-NULL
/// row is FALSE for both; an all-NULL row IS NULL; a whole-value NULL IS NULL.
#[test]
fn composite_is_null_all_fields() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE rec AS (a int32, b int32)");
    // All fields present → IS NOT NULL true, IS NULL false.
    assert_eq!(
        query(&mut db, "SELECT ROW(1, 2) IS NULL, ROW(1, 2) IS NOT NULL"),
        vec![vec!["false".to_string(), "true".to_string()]]
    );
    // Partially NULL → FALSE for both (the PG gotcha).
    assert_eq!(
        query(
            &mut db,
            "SELECT ROW(1, NULL) IS NULL, ROW(1, NULL) IS NOT NULL"
        ),
        vec![vec!["false".to_string(), "false".to_string()]]
    );
    // All fields NULL → IS NULL true, IS NOT NULL false.
    assert_eq!(
        query(
            &mut db,
            "SELECT ROW(NULL, NULL) IS NULL, ROW(NULL, NULL) IS NOT NULL"
        ),
        vec![vec!["true".to_string(), "false".to_string()]]
    );
}

/// S5: the all-fields `IS NULL` rule is ONE LEVEL DEEP, not recursive (the empirically-probed
/// PG behavior — the differential oracle). A composite-valued field is a non-NULL value, so it
/// counts as PRESENT: a nested all-NULL row is therefore `IS NULL` = FALSE (the inner rows are
/// non-null values) and `IS NOT NULL` = TRUE.
#[test]
fn composite_is_null_non_recursive() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE point AS (x int32, y int32)");
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

/// S5: DISTINCT and GROUP BY over a composite column use the recursive value key (NULL-safe).
#[test]
fn composite_distinct_and_group_by() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip int32)");
    run(&mut db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW('Oak', 10))");
    run(&mut db, "INSERT INTO p VALUES (2, ROW('Oak', 10))");
    run(&mut db, "INSERT INTO p VALUES (3, ROW('Elm', 20))");
    // DISTINCT collapses the two identical Oak/10 rows → 2 distinct composites.
    assert_eq!(
        query(&mut db, "SELECT DISTINCT home FROM p ORDER BY home"),
        vec![vec!["(Elm,20)".to_string()], vec!["(Oak,10)".to_string()]]
    );
    // GROUP BY the composite column → count per group.
    assert_eq!(
        query(
            &mut db,
            "SELECT home, count(*) FROM p GROUP BY home ORDER BY home"
        ),
        vec![
            vec!["(Elm,20)".to_string(), "1".to_string()],
            vec!["(Oak,10)".to_string(), "2".to_string()]
        ]
    );
}

/// S5: a composite compared with a non-composite, or with a different-arity row, is 42804.
#[test]
fn composite_comparison_type_errors() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE rec AS (a int32, b int32)");
    run(&mut db, "CREATE TABLE p (id int32 PRIMARY KEY, r rec)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW(1, 2))");
    // Composite vs scalar.
    assert_eq!(err(&mut db, "SELECT r = 1 FROM p"), "42804");
    // Different row sizes.
    assert_eq!(err(&mut db, "SELECT ROW(1, 2) = ROW(1, 2, 3)"), "42804");
}

#[test]
fn cascade_is_0a000() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (a int32)");
    assert_eq!(err(&mut db, "DROP TYPE addr CASCADE"), "0A000");
}

#[test]
fn nested_type_self_or_forward_reference_is_42704() {
    let mut db = Database::new();
    // Forward reference (point not yet defined) — and self-reference — are unknown types.
    assert_eq!(err(&mut db, "CREATE TYPE line AS (a point)"), "42704");
    assert_eq!(err(&mut db, "CREATE TYPE t AS (a t)"), "42704");
}

/// Round-trip through the on-disk image: a composite type (and a nested one) survives
/// serialize → load, byte-backed by the v9 catalog type-definition section.
#[test]
fn types_persist_through_image() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TYPE point AS (x int32 NOT NULL, y int32 NOT NULL)",
    );
    run(&mut db, "CREATE TYPE line AS (a point, b point)");
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");

    let image = db.to_image(256, 1).unwrap();
    let loaded = Database::from_image(&image).expect("reload");

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
