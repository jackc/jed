//! Collation host API + persistence (spec/design/collation.md §1/§4.2): the reference-only surface —
//! `set_default_collation` / `default_collation`, the per-file `db.collations()` (what the database
//! REFERENCES) vs the build-global `jed::vendored_collations()` (what the engine VENDORS), per-column
//! / per-database default inheritance, collated keys, and the reference-only **file round-trip**
//! (`format_version` 18, `entry_kind = 3` metadata entries). These are the host-API + persistence
//! behaviors the conformance corpus cannot express (CLAUDE.md §10); the in-memory SQL behavior a
//! collation drives (COLLATE / ORDER BY / derivation / 42P21 / 42P22) lives in
//! suites/collation/collate.test, which runs on every core. There is **no `import_collation`**: a
//! collation is vendored into the binary and used by name (the reference-only pivot, §4.2). Mirrored
//! by impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn texts(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

// ---- the vendored set (the engine-global build property) ----

#[test]
fn vendored_collations_is_the_dev_fixtures() {
    // `jed::vendored_collations()` reports what THIS BUILD provides — the dev fixture set, ascending by
    // name, no `is_default` (a build property, not a per-db one). `C` is built in and never listed.
    let v = jed::vendored_collations();
    let names: Vec<&str> = v.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["dev-nordic", "dev-root"]);
    assert!(v.iter().all(|c| !c.is_default));
    assert_eq!(v[1].name, "dev-root");
    assert_eq!(v[1].unicode_version, "0.0.0-dev");
    assert!(jed::collation::vendored_collation("dev-root").is_some());
    assert!(jed::collation::vendored_collation("C").is_none());
}

// ---- using a vendored collation needs NO import ----

#[test]
fn vendored_collation_used_in_an_expression() {
    // `COLLATE "dev-root"` resolves from the binary's vendored set with no import: 'ä' < 'z' is true
    // under dev-root (ä near a), the opposite of the C byte order where it is false. A transient query
    // COLLATE does not make the database REFERENCE the collation, so db.collations() stays empty.
    let mut db = Database::new();
    assert_eq!(db.collations().len(), 0);
    assert_eq!(
        query(&mut db, "SELECT 'ä' < 'z' COLLATE \"dev-root\""),
        vec![vec![Value::Bool(true)]]
    );
}

#[test]
fn unknown_collation_is_42704() {
    // A collation neither vendored nor referenced is 42704 (the vendored fallback must not mask it).
    let mut db = Database::new();
    assert_eq!(
        execute(&mut db, "SELECT 'x' COLLATE \"no-such-collation\"")
            .unwrap_err()
            .code(),
        "42704"
    );
}

#[test]
fn per_column_collation_orders_implicitly_and_is_referenced() {
    // A column declared `COLLATE "dev-root"` (vendored, no import) sorts by that collation with no
    // explicit COLLATE on the query — dev-root puts ä next to a. Because the SCHEMA now references
    // dev-root, db.collations() (the per-file view) lists exactly it.
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"dev-root\")",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')").unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    let refs = db.collations();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "dev-root");
    assert!(!refs[0].is_default); // referenced by a column, but not the db default
    // An explicit COLLATE "C" on the query overrides back to byte order (ä is 2-byte UTF-8 → after z).
    assert_eq!(
        texts(query(
            &mut db,
            "SELECT name FROM t ORDER BY name COLLATE \"C\""
        )),
        vec!["a", "z", "ä"]
    );
}

#[test]
fn implicit_conflict_is_42p22() {
    // Two columns with DIFFERENT implicit (vendored) collations compared with no explicit COLLATE →
    // 42P22 (PG-matching). C counts as a distinct implicit collation, so dev-root vs C also conflicts.
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (a text COLLATE \"dev-root\", b text COLLATE \"dev-nordic\", c text COLLATE \"C\")",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES ('a','z','b')").unwrap();
    assert_eq!(
        execute(&mut db, "SELECT a < b FROM t").unwrap_err().code(),
        "42P22"
    );
    assert_eq!(
        execute(&mut db, "SELECT a < c FROM t").unwrap_err().code(),
        "42P22"
    );
    // An explicit COLLATE on one side breaks the tie (no error): a='a' < (b='z') = true.
    assert_eq!(
        query(&mut db, "SELECT a < b COLLATE \"dev-root\" FROM t"),
        vec![vec![Value::Bool(true)]]
    );
    // The table references both vendored collations → db.collations() lists them (sorted).
    let names: Vec<String> = db.collations().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec!["dev-nordic".to_string(), "dev-root".to_string()]
    );
}

#[test]
fn non_text_collate_is_42804_unknown_name_42704() {
    let mut db = Database::new();
    assert_eq!(
        execute(&mut db, "CREATE TABLE t (a i32 COLLATE \"dev-root\")")
            .unwrap_err()
            .code(),
        "42804"
    );
    assert_eq!(
        execute(&mut db, "CREATE TABLE t (a text COLLATE \"nope\")")
            .unwrap_err()
            .code(),
        "42704"
    );
}

// ---- the per-database default (over the vendored set, no import) ----

#[test]
fn default_collation_inherited_by_unannotated_column() {
    // set_default_collation moves the per-database default to a VENDORED collation (no import); an
    // un-annotated text column created AFTER inherits it (frozen), one created BEFORE keeps C.
    let mut db = Database::new();
    assert_eq!(db.default_collation(), "C");
    execute(
        &mut db,
        "CREATE TABLE before (id i32 PRIMARY KEY, name text)",
    )
    .unwrap();
    db.set_default_collation("dev-root").unwrap();
    assert_eq!(db.default_collation(), "dev-root");
    execute(
        &mut db,
        "CREATE TABLE after (id i32 PRIMARY KEY, name text)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')").unwrap();
    // `after.name` inherited dev-root → ä sorts next to a even with no COLLATE clause.
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM after ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    // `before.name` was frozen at C → byte order.
    execute(&mut db, "INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')").unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM before ORDER BY name")),
        vec!["a", "z", "ä"]
    );
    // The default makes dev-root referenced (is_default true).
    let refs = db.collations();
    assert_eq!(refs.len(), 1);
    assert!(refs[0].is_default);
}

#[test]
fn set_default_unknown_is_42704() {
    let mut db = Database::new();
    assert_eq!(
        db.set_default_collation("nope").unwrap_err().code(),
        "42704"
    );
    db.set_default_collation("C").unwrap(); // C always resolves (resets to byte order)
}

// ---- collated keys (slice 1e, on-disk/internal — the corpus cannot express it) ----

#[test]
fn collated_primary_key_is_stored_in_collation_order() {
    // A collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so the B-tree
    // physically iterates in COLLATION order. dev-root (vendored, no import): a < A < b < Z; C bytes:
    // A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (name text COLLATE \"dev-root\" PRIMARY KEY)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES ('Z'),('a'),('b'),('A')").unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t")),
        vec!["a", "A", "b", "Z"]
    );
    execute(&mut db, "CREATE TABLE c (name text PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO c VALUES ('Z'),('a'),('b'),('A')").unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM c")),
        vec!["A", "Z", "a", "b"]
    );
}

#[test]
fn collated_unique_dedups_by_byte_identity() {
    // A collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A' are
    // DISTINCT, both admitted — collation.md §7), like a C unique key; only a byte-duplicate violates.
    let mut db = Database::new();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"dev-root\" UNIQUE)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')").unwrap();
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (4,'a')")
            .unwrap_err()
            .code(),
        "23505"
    );
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t ORDER BY name")),
        vec!["a", "A", "b"]
    );
}

// ---- reference-only file round-trip (format_version 18) ----

#[test]
fn reference_only_file_round_trip() {
    // A collated table + the per-database default survive a close + paged reopen. The file stores only
    // a metadata REFERENCE entry (no table); on reopen the table is resolved from the vendored set.
    let path = tmp("collation_refonly_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    db.set_default_collation("dev-root").unwrap(); // vendored — no import
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"dev-root\", plain text)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')",
    )
    .unwrap();
    db.commit().unwrap();
    db.close().unwrap();

    let mut re = Database::open(&path).unwrap();
    assert_eq!(re.default_collation(), "dev-root");
    // The database still references dev-root (per-file view) — resolved from the vendored set.
    let refs = re.collations();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "dev-root");
    assert_eq!(refs[0].unicode_version, "0.0.0-dev");
    assert!(refs[0].is_default);
    assert_eq!(
        texts(query(&mut re, "SELECT name FROM t ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    // `plain` (un-annotated) inherited the default (dev-root) at create → also dev-root order.
    assert_eq!(
        texts(query(&mut re, "SELECT plain FROM t ORDER BY plain")),
        vec!["a", "ä", "z"]
    );
    re.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
