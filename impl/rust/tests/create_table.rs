//! Phase B: CREATE TABLE — parse, analyze, register in the catalog. Driven by unit
//! tests until the `core` profile is complete and the corpus runs (Phase E).

use jed::catalog::Table;
use jed::types::ScalarType;
use jed::{Database, Outcome, execute};

fn create(db: &mut Database, sql: &str) -> jed::Result<Outcome> {
    execute(db, sql)
}

#[test]
fn creates_table_with_resolved_types_and_pk() {
    let mut db = Database::new();
    let out = create(
        &mut db,
        "CREATE TABLE nums (id int32 PRIMARY KEY, small int16, big int64)",
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
    assert_eq!(t.columns[0].ty, ScalarType::Int32);
    assert!(t.columns[0].primary_key);
    assert!(t.columns[0].not_null, "PRIMARY KEY implies NOT NULL");

    assert_eq!(t.columns[1].ty, ScalarType::Int16);
    assert!(!t.columns[1].primary_key);
    assert!(!t.columns[1].not_null);

    assert_eq!(t.columns[2].ty, ScalarType::Int64);
    assert_eq!(t.primary_key_index(), Some(0));
}

#[test]
fn sql_standard_type_aliases_resolve() {
    let mut db = Database::new();
    create(
        &mut db,
        "CREATE TABLE t (a smallint, b integer, c int, d bigint)",
    )
    .unwrap();
    let t = db.table("t").unwrap();
    assert_eq!(t.columns[0].ty, ScalarType::Int16);
    assert_eq!(t.columns[1].ty, ScalarType::Int32);
    assert_eq!(t.columns[2].ty, ScalarType::Int32);
    assert_eq!(t.columns[3].ty, ScalarType::Int64);
}

#[test]
fn table_and_type_names_are_case_insensitive() {
    let mut db = Database::new();
    create(&mut db, "create table T (Id INT32 primary key)").unwrap();
    assert!(db.table("t").is_some());
    assert!(db.table("T").is_some());
}

#[test]
fn duplicate_table_is_rejected() {
    let mut db = Database::new();
    create(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap();
    let err = create(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap_err();
    assert_eq!(err.code(), "42P07");
}

#[test]
fn duplicate_column_is_rejected() {
    let mut db = Database::new();
    let err = create(&mut db, "CREATE TABLE t (a int32, a int16)").unwrap_err();
    assert_eq!(err.code(), "42701");
}

#[test]
fn unknown_type_is_rejected() {
    let mut db = Database::new();
    let err = create(&mut db, "CREATE TABLE t (a int128)").unwrap_err();
    assert_eq!(err.code(), "42704");
}

#[test]
fn pg_internal_type_names_are_not_accepted() {
    // We own our surface (CLAUDE.md §1): int2/int4/int8 are NOT type names.
    let mut db = Database::new();
    assert_eq!(
        create(&mut db, "CREATE TABLE t (a int2)")
            .unwrap_err()
            .code(),
        "42704"
    );
}

#[test]
fn multiple_primary_keys_are_rejected() {
    let mut db = Database::new();
    let err = create(
        &mut db,
        "CREATE TABLE t (a int32 PRIMARY KEY, b int32 PRIMARY KEY)",
    )
    .unwrap_err();
    assert_eq!(err.code(), "42P16");
}

#[test]
fn syntax_errors_are_reported() {
    let mut db = Database::new();
    assert_eq!(
        create(&mut db, "CREATE TABLE t").unwrap_err().code(),
        "42601"
    );
    assert_eq!(
        create(&mut db, "CREATE TABLE t (a int32,)")
            .unwrap_err()
            .code(),
        "42601"
    );
}
