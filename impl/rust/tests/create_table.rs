//! Phase B: CREATE TABLE — parse, analyze, register in the catalog. Driven by unit
//! tests until the `core` profile is complete and the corpus runs (Phase E).

use jed::catalog::Table;
use jed::types::ScalarType;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn create(db: &mut Session, sql: &str) -> jed::Result<Outcome> {
    db.query_outcome(sql, &[])
}

#[test]
fn creates_table_with_resolved_types_and_pk() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
fn bare_names_fold_and_host_lookup_is_exact() {
    // A bare name folds to lowercase; the host API looks a name up exactly (grammar.md §3).
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    create(&mut db, "create table T (Id I32 primary key)").unwrap();
    assert!(db.table("t").is_some());
    assert!(db.table("T").is_none());
    assert_eq!(db.table("t").unwrap().columns[0].name, "id");
    create(&mut db, "create table \"T\" (\"Id\" i32 primary key)").unwrap();
    assert_eq!(db.table("T").unwrap().columns[0].name, "Id");
    assert_eq!(db.table_names(), vec!["T".to_string(), "t".to_string()]);
}

#[test]
fn duplicate_table_is_rejected() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    create(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap();
    let err = create(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap_err();
    assert_eq!(err.code(), "42P07");
}

#[test]
fn duplicate_column_is_rejected() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let err = create(&mut db, "CREATE TABLE t (a i32, a i16)").unwrap_err();
    assert_eq!(err.code(), "42701");
}

#[test]
fn unknown_type_is_rejected() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let err = create(
        &mut db,
        "CREATE TABLE t (a i32 PRIMARY KEY, b i32 PRIMARY KEY)",
    )
    .unwrap_err();
    assert_eq!(err.code(), "42P16");
}

#[test]
fn syntax_errors_are_reported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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

/// The on-disk catalog emits each name group in ascending BYTE order of the exact name
/// (format_version 37): uppercase ASCII sorts before lowercase, so `B` < `C` < `a`, and the reload
/// keeps every name exact (a bare lookup never reaches a mixed-case name).
#[test]
fn catalog_order_is_exact_name_bytes_across_reload() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "CREATE TABLE a (id i32 PRIMARY KEY)",
        "CREATE TABLE \"C\" (id i32 PRIMARY KEY)",
        "CREATE TABLE \"B\" (id i32 PRIMARY KEY, \"V\" i32, v i32)",
        "CREATE INDEX \"Zed\" ON \"B\" (v)",
        "CREATE INDEX alpha ON \"B\" (\"V\")",
        "CREATE SEQUENCE \"Seq\"",
        "CREATE SEQUENCE aseq",
    ] {
        create(&mut db, sql).unwrap();
    }
    let image = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        loaded.table_names(),
        vec!["B".to_string(), "C".to_string(), "a".to_string()]
    );
    let b = loaded.table("B").unwrap();
    let idx: Vec<&str> = b.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(idx, vec!["Zed", "alpha"]);
    assert_eq!(b.columns[1].name, "V");
    assert_eq!(b.columns[2].name, "v");
    assert!(loaded.table("b").is_none());
    assert!(create(&mut loaded, "SELECT nextval('\"Seq\"')").is_ok());
    let err = create(&mut loaded, "SELECT nextval('Seq')").unwrap_err();
    assert_eq!(err.code(), "42P01", "the bare text folds to seq");
    // Re-serializing the catalog is byte-identical (the reader trusts the emitted order); the
    // nextval above advanced "Seq", so compare against a fresh reload of the original image.
    let fresh = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(fresh.to_image(8192, 1).unwrap(), image);
}
