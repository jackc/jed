//! Composite **type** as a key — a column whose type is a `CREATE TYPE … AS (…)` row type used
//! as a `PRIMARY KEY` / ordered secondary index / `UNIQUE` column (the third container key,
//! `composite-field-slots`, spec/design/encoding.md §2.15 / composite.md §6). Distinct from the
//! multi-column composite PRIMARY KEY in composite_pk.rs (a flat tuple of scalar columns). Covers
//! what the corpus cannot: the stored key ORDER (the recursive per-field encoding), catalog
//! introspection, the on-disk round-trip, and the array-of-composite `0A000` narrowing. Mirrored in
//! impl/go/composite_key_test.go and impl/ts/tests/composite_key.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in sql {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

fn ids_in_key_order(db: &Session, table: &str) -> Vec<i64> {
    db.rows_in_key_order(table)
        .unwrap()
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("expected int id, got {v:?}"),
        })
        .collect()
}

/// A composite-typed column is a valid sole PRIMARY KEY, and rows iterate in the composite sort
/// key's order — lexicographic over fields, first field (text) then the tie-breaking second (i32),
/// exactly reproducing the in-memory comparator (§5) under the §2.15 memcmp key.
#[test]
fn composite_pk_orders_by_field_lexicographic() {
    let db = db_with(&[
        "CREATE TYPE addr AS (street text, zip i32)",
        "CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
        // out of order; ('Main',5) and ('Main',90210) share the first field, broken by zip
        "INSERT INTO t VALUES (1, ROW('Main', 90210))",
        "INSERT INTO t VALUES (2, ROW('Elm', 100))",
        "INSERT INTO t VALUES (3, ROW('Main', 5))",
        "INSERT INTO t VALUES (4, ROW('', -1))",
    ]);
    // '' < 'Elm' < 'Main'; within 'Main', zip 5 < 90210  => ids 4, 2, 3, 1
    assert_eq!(ids_in_key_order(&db, "t"), vec![4, 2, 3, 1]);
    assert_eq!(db.table("t").unwrap().pk_indices(), vec![1]);
    assert!(db.table("t").unwrap().columns[1].not_null);
}

/// Uniqueness is over the whole composite value: a duplicate composite traps 23505, a value that
/// differs in ANY field is distinct (a NULL field is part of the value).
#[test]
fn composite_pk_uniqueness_is_the_whole_value() {
    let mut db = db_with(&[
        "CREATE TYPE addr AS (street text, zip i32)",
        "CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
        "INSERT INTO t VALUES (1, ROW('Main', 5))",
    ]);
    db.query_outcome("INSERT INTO t VALUES (2, ROW('Main', 6))", &[])
        .unwrap(); // differs in zip: distinct
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (9, ROW('Main', 5))"),
        "23505"
    );
    // Two identical composites in one batch: all-or-nothing.
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES (7, ROW('X', 1)), (8, ROW('X', 1))"
        ),
        "23505"
    );
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 2);
}

/// A composite `UNIQUE` constraint enforces distinctness of the composite value, recursing through
/// a **nested** composite field (`line AS (a addr, b addr)`): the key encoder frames each nested
/// composite in its own §2.2 slot and recurses.
#[test]
fn composite_unique_and_nested() {
    let mut db = db_with(&[
        "CREATE TYPE addr AS (street text, zip i32)",
        "CREATE TYPE line AS (a addr, b addr)",
        "CREATE TABLE t (id i32, seg line, UNIQUE (seg))",
        "INSERT INTO t VALUES (1, ROW(ROW('Main',1), ROW('Elm',2)))",
    ]);
    // Duplicate nested composite -> 23505
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES (4, ROW(ROW('Main',1), ROW('Elm',2)))"
        ),
        "23505"
    );
    // Differs one deeply-nested field -> distinct
    db.query_outcome(
        "INSERT INTO t VALUES (5, ROW(ROW('Main',1), ROW('Elm',3)))",
        &[],
    )
    .unwrap();
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 2);
}

/// A secondary index over a composite column supports the maintenance path (INSERT/DELETE) and the
/// composite value round-trips through the on-disk image with its key order intact.
#[test]
fn composite_secondary_index_and_image_roundtrip() {
    let db = db_with(&[
        "CREATE TYPE addr AS (street text, zip i32)",
        "CREATE TABLE t (id i32 PRIMARY KEY, home addr)",
        "CREATE INDEX t_home ON t (home)",
        "INSERT INTO t VALUES (1, ROW('Main', 90210))",
        "INSERT INTO t VALUES (2, ROW('Elm', 100))",
        "INSERT INTO t VALUES (3, ROW('Main', 5))",
    ]);
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    // The index still exists and enforces nothing extra; a delete + reinsert exercises maintenance.
    loaded
        .query_outcome("DELETE FROM t WHERE id = 2", &[])
        .unwrap();
    loaded
        .query_outcome("INSERT INTO t VALUES (4, ROW('Elm', 100))", &[])
        .unwrap();
    // Ordered scan over the index column via the pk (values survive the reload).
    let n = loaded.rows_in_key_order("t").unwrap().len();
    assert_eq!(n, 3);
    assert!(
        loaded
            .table("t")
            .unwrap()
            .indexes
            .iter()
            .any(|i| i.name == "t_home")
    );
}

/// A composite that transitively contains an **array-of-composite** field is NOT keyable — the
/// array key admits only scalar elements (§2.14), so it stays the deferred `0A000` narrowing even
/// though the bare composite container is now keyable. A composite with a scalar-array field IS
/// keyable (the array element is a scalar).
#[test]
fn array_of_composite_field_is_not_keyable() {
    let mut db = db_with(&[
        "CREATE TYPE addr AS (street text, zip i32)",
        "CREATE TYPE tags AS (name text, nums i32[])", // scalar-array field: keyable
        "CREATE TYPE poly AS (name text, spots addr[])", // array-of-composite field: NOT keyable
    ]);
    // scalar-array field -> keyable composite PK
    db.query_outcome("CREATE TABLE ok (id i32, t tags, PRIMARY KEY (t))", &[])
        .unwrap();
    // array-of-composite field -> 0A000 (PK, UNIQUE, and INDEX)
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (id i32, p poly, PRIMARY KEY (p))"),
        "0A000"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (id i32, p poly, UNIQUE (p))"),
        "0A000"
    );
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, p poly)", &[])
        .unwrap();
    assert_eq!(err_code(&mut db, "CREATE INDEX t_p ON t (p)"), "0A000");
}
