//! Phase B: CREATE TABLE — parse, analyze, register in the catalog. Driven by unit
//! tests until the `core` profile is complete and the corpus runs (Phase E).

use jed::catalog::Table;
use jed::types::ScalarType;
use jed::{Database, Outcome, Session, SessionOptions};

fn create(db: &mut Session, sql: &str) -> jed::Result<Outcome> {
    db.execute(sql, &[])
}

#[test]
fn creates_table_with_resolved_types_and_pk() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    let out = create(
        &mut db,
        "CREATE TABLE nums (id i32 PRIMARY KEY, small i16, big i64)",
    )
    .unwrap();
    assert_eq!(
        out,
        Outcome::Statement {
            cost: 0,
            rows_affected: None
        }
    );

    let t: &Table = db.table("nums").expect("table registered");
    assert_eq!(t.columns.len(), 3);

    assert_eq!(t.columns[0].name, "id");
    assert_eq!(t.columns[0].ty.scalar(), ScalarType::Int32);
    assert!(t.columns[0].primary_key);
    assert!(t.columns[0].not_null, "PRIMARY KEY implies NOT NULL");

    assert_eq!(t.columns[1].ty.scalar(), ScalarType::Int16);
    assert!(!t.columns[1].primary_key);
    assert!(!t.columns[1].not_null);

    assert_eq!(t.columns[2].ty.scalar(), ScalarType::Int64);
    assert_eq!(t.primary_key_index(), Some(0));
}

#[test]
fn sql_standard_type_aliases_resolve() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    create(
        &mut db,
        "CREATE TABLE t (a smallint, b integer, c int, d bigint)",
    )
    .unwrap();
    let t = db.table("t").unwrap();
    assert_eq!(t.columns[0].ty.scalar(), ScalarType::Int16);
    assert_eq!(t.columns[1].ty.scalar(), ScalarType::Int32);
    assert_eq!(t.columns[2].ty.scalar(), ScalarType::Int32);
    assert_eq!(t.columns[3].ty.scalar(), ScalarType::Int64);
}

#[test]
fn table_and_type_names_are_case_insensitive() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    create(&mut db, "create table T (Id I32 primary key)").unwrap();
    assert!(db.table("t").is_some());
    assert!(db.table("T").is_some());
}

#[test]
fn duplicate_table_is_rejected() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    create(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap();
    let err = create(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap_err();
    assert_eq!(err.code(), "42P07");
}

#[test]
fn duplicate_column_is_rejected() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    let err = create(&mut db, "CREATE TABLE t (a i32, a i16)").unwrap_err();
    assert_eq!(err.code(), "42701");
}

#[test]
fn unknown_type_is_rejected() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    let err = create(&mut db, "CREATE TABLE t (a int128)").unwrap_err();
    assert_eq!(err.code(), "42704");
    // The old jed bit-names are a CLEAN BREAK — replaced by the i/f prefix, no longer
    // accepted (CLAUDE.md §4; types.md §11).
    assert_eq!(
        create(&mut db, "CREATE TABLE t (a int32)")
            .unwrap_err()
            .code(),
        "42704"
    );
    assert_eq!(
        create(&mut db, "CREATE TABLE t (a float64)")
            .unwrap_err()
            .code(),
        "42704"
    );
}

#[test]
fn pg_byte_shorthand_type_names_are_accepted() {
    // The i/f prefix makes jed's bit-namespace (i8…i64) lexically disjoint from PG's
    // byte-namespace, so PG's byte-shorthand is accepted as aliases (CLAUDE.md §1/§4;
    // types.md §11): int2→i16, int4→i32, int8→i64, float4→f32, float8→f64. There is no
    // int8-means-8-bit collision, and a future 8-bit i8 stays free.
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    create(
        &mut db,
        "CREATE TABLE t (a int2, b int4, c int8, d float4, e float8)",
    )
    .unwrap();
    let tbl = db.table("t").unwrap();
    let want = [
        ScalarType::Int16,
        ScalarType::Int32,
        ScalarType::Int64,
        ScalarType::Float32,
        ScalarType::Float64,
    ];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(tbl.columns[i].ty.scalar(), *w, "col {i}");
    }
}

#[test]
fn multiple_primary_keys_are_rejected() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    let err = create(
        &mut db,
        "CREATE TABLE t (a i32 PRIMARY KEY, b i32 PRIMARY KEY)",
    )
    .unwrap_err();
    assert_eq!(err.code(), "42P16");
}

#[test]
fn syntax_errors_are_reported() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        create(&mut db, "CREATE TABLE t").unwrap_err().code(),
        "42601"
    );
    assert_eq!(
        create(&mut db, "CREATE TABLE t (a i32,)")
            .unwrap_err()
            .code(),
        "42601"
    );
}
