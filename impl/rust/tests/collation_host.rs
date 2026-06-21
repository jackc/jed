//! Collation host API (spec/design/collation.md §1/§4): `db.import_collation` (1c) plus the slice-1d
//! host surface — `export_collation`, `set_default_collation` / `default_collation`, `collations`,
//! per-database default inheritance, and the baked **file round-trip** (`format_version` 17,
//! `entry_kind = 3`). These are the host-API + persistence behaviors the conformance corpus cannot
//! express (CLAUDE.md §10); the in-memory SQL behavior a loaded collation drives (COLLATE / ORDER BY
//! / derivation / 42P21 / 42P22) lives in suites/collation/collate.test, which runs on every core.
//! Mirrored by impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.

use jed::collation::compile_collation;
use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};
use std::path::{Path, PathBuf};

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn dev_root() -> jed::collation::Collation {
    compile_collation("dev-root", &spec("collation/fixtures/dev-root.allkeys")).unwrap()
}

/// The dev-nordic collation: the root weights plus the dev-nordic LDML tailoring (ä ö after z) — a
/// genuinely different table from dev-root, for the implicit-conflict (42P22) and default cases.
fn dev_nordic() -> jed::collation::Collation {
    let def = format!(
        "{}\n{}",
        spec("collation/fixtures/dev-root.allkeys"),
        spec("collation/fixtures/dev-nordic.ldml")
    );
    compile_collation("dev-nordic", &def).unwrap()
}

/// A collation under the name "dev-root" but with the dev-nordic *table* (a different content hash)
/// — the conflicting import.
fn dev_root_named_but_nordic_table() -> jed::collation::Collation {
    let def = format!(
        "{}\n{}",
        spec("collation/fixtures/dev-root.allkeys"),
        spec("collation/fixtures/dev-nordic.ldml")
    );
    compile_collation("dev-root", &def).unwrap()
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

#[test]
fn import_then_use_in_a_query() {
    let mut db = Database::new();
    assert_eq!(db.import_collation(dev_root()).unwrap(), "dev-root");
    // The imported collation is usable by name: 'ä' < 'z' is true under dev-root (ä near a),
    // the opposite of the C byte order where it is false.
    let rows = query(&mut db, "SELECT 'ä' < 'z' COLLATE \"dev-root\"");
    assert_eq!(rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn import_is_idempotent_by_name_and_hash() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    // Re-importing the identical (name, content) collation is a no-op success, returning the name.
    assert_eq!(db.import_collation(dev_root()).unwrap(), "dev-root");
}

#[test]
fn import_conflict_same_name_different_table_is_42710() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    // A DIFFERENT table under a name already in use is a conflict (it would invalidate any structure
    // built under that name — collation.md §4).
    let err = db
        .import_collation(dev_root_named_but_nordic_table())
        .unwrap_err();
    assert_eq!(err.code(), "42710");
}

#[test]
fn importing_C_is_rejected() {
    let mut db = Database::new();
    // `C` is table-free and built in; it is never imported (collation.md §4).
    let c = compile_collation("C", &spec("collation/fixtures/dev-root.allkeys")).unwrap();
    let err = db.import_collation(c).unwrap_err();
    assert_eq!(err.code(), "42710");
}

// ---- slice 1d ----

fn texts(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn per_column_collation_orders_implicitly() {
    // A column declared `COLLATE "dev-root"` sorts by that collation with NO explicit COLLATE on the
    // query — the whole point of per-column collations (collation.md §1). dev-root puts ä next to a.
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"dev-root\")",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')").unwrap();
    let rows = texts(query(&mut db, "SELECT name FROM t ORDER BY name"));
    assert_eq!(rows, vec!["a", "ä", "z"]);
    // An explicit COLLATE "C" on the query overrides back to byte order (ä is a 2-byte UTF-8 → after z).
    let rows = texts(query(
        &mut db,
        "SELECT name FROM t ORDER BY name COLLATE \"C\"",
    ));
    assert_eq!(rows, vec!["a", "z", "ä"]);
}

#[test]
fn implicit_conflict_is_42p22() {
    // Two columns with DIFFERENT implicit collations compared with no explicit COLLATE → 42P22
    // (PG-matching). C counts as a distinct implicit collation, so dev-root vs C also conflicts.
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    db.import_collation(dev_nordic()).unwrap();
    execute(
        &mut db,
        "CREATE TABLE t (a text COLLATE \"dev-root\", b text COLLATE \"dev-nordic\", c text COLLATE \"C\")",
    )
    .unwrap();
    // Values use only dev-mapped code points (a, b, z). The 42P22 cases fail at resolve (before any
    // eval), so the row values matter only for the explicit-override case.
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
    let rows = query(&mut db, "SELECT a < b COLLATE \"dev-root\" FROM t");
    assert_eq!(rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn non_text_collate_column_is_42804_unknown_name_42704() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
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

#[test]
fn default_collation_inherited_by_unannotated_column() {
    // SetDefaultCollation moves the per-database default; an un-annotated text column created AFTER
    // inherits it (frozen). A column created BEFORE keeps C (collation.md §1, PG-matching).
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
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
}

#[test]
fn set_default_unknown_is_42704() {
    let mut db = Database::new();
    assert_eq!(
        db.set_default_collation("nope").unwrap_err().code(),
        "42704"
    );
    // C always resolves (resets to byte order).
    db.set_default_collation("C").unwrap();
}

#[test]
fn export_round_trips_and_introspects() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    // Export pulls the collation back; re-importing it is idempotent (identical content hash).
    let exported = db.export_collation("dev-root").unwrap();
    assert_eq!(exported.name, "dev-root");
    let mut db2 = Database::new();
    assert_eq!(db2.import_collation(exported).unwrap(), "dev-root");
    // Unknown / built-in C → 42704 (nothing to export).
    assert_eq!(db.export_collation("nope").unwrap_err().code(), "42704");
    assert_eq!(db.export_collation("C").unwrap_err().code(), "42704");
    // Introspection lists the loaded collation with its default flag.
    db.set_default_collation("dev-root").unwrap();
    let infos = db.collations();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "dev-root");
    assert!(infos[0].is_default);
}

#[test]
fn collated_primary_key_is_stored_in_collation_order() {
    // slice 1e: a collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12),
    // so the B-tree physically iterates in COLLATION order — not the C byte order. A no-ORDER-BY
    // single-table scan returns jed's stored (key) order, so this asserts the *key* is collated
    // (distinct from the in-memory ORDER BY sorter that 1c already had). On-disk/internal property
    // the corpus cannot express (CLAUDE.md §10). dev-root: a < A < b < Z; C bytes: A < Z < a < b.
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
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
    // Contrast: a C-keyed table iterates in raw byte order.
    execute(&mut db, "CREATE TABLE c (name text PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO c VALUES ('Z'),('a'),('b'),('A')").unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM c")),
        vec!["A", "Z", "a", "b"]
    );
}

#[test]
fn collated_secondary_index_and_unique_keys() {
    // slice 1e: a collated secondary index iterates by collation order (an unindexed-scan-equivalent
    // result), and a collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and
    // 'A' are DISTINCT, both admitted — collation.md §7), exactly like a C unique key.
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    execute(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"dev-root\" UNIQUE)",
    )
    .unwrap();
    // 'a' and 'A' are distinct under a deterministic collation → both accepted.
    execute(&mut db, "INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')").unwrap();
    // A duplicate (byte-identical) value violates the collated UNIQUE constraint.
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (4,'a')")
            .unwrap_err()
            .code(),
        "23505"
    );
    // ORDER BY over the collated column is collation order even without an explicit COLLATE.
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t ORDER BY name")),
        vec!["a", "A", "b"]
    );
}

#[test]
fn baked_file_round_trip() {
    // The full slice-1d persistence path: a collation + a collated table + the per-database default
    // are baked into the file (format_version 17, entry_kind 3) and survive a close + paged reopen.
    let path = tmp("collation_baked_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    db.import_collation(dev_root()).unwrap();
    db.set_default_collation("dev-root").unwrap();
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
    // The baked collation is back, still the default.
    assert_eq!(re.default_collation(), "dev-root");
    assert_eq!(re.collations().len(), 1);
    // The collated column still sorts by dev-root (the snapshot was read before the table entry).
    assert_eq!(
        texts(query(&mut re, "SELECT name FROM t ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    // `plain` (un-annotated) inherited the default (dev-root) at create time → also dev-root order.
    assert_eq!(
        texts(query(&mut re, "SELECT plain FROM t ORDER BY plain")),
        vec!["a", "ä", "z"]
    );
    re.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

// ---- slice 2: vendored / reference-only read path (collation.md §2/§3/§9) ----
//
// In the reference-only model a collation is **vendored into the binary**, so it is usable WITHOUT
// any `db.import_collation` — the database references it by name and the table comes from the
// vendored set. These assert that no-import path directly (the corpus still imports, so it cannot
// express this — CLAUDE.md §10). dev-root and dev-nordic are the vendored dev fixtures (§14, 2a).

#[test]
fn vendored_collation_used_without_import() {
    // No import: `COLLATE "dev-root"` resolves from the binary's vendored set. 'ä' < 'z' is true
    // under dev-root (ä near a), the opposite of C byte order — proving the vendored table is used.
    let mut db = Database::new();
    assert_eq!(db.collations().len(), 0); // nothing imported / referenced by this database
    let rows = query(&mut db, "SELECT 'ä' < 'z' COLLATE \"dev-root\"");
    assert_eq!(rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn vendored_per_column_collation_without_import() {
    // A `COLLATE "dev-root"` column works with NO import: CREATE TABLE validation and the key
    // encoder both fall back to the vendored set, so the collated ORDER BY (and the collated key)
    // use dev-root order — ä between a and z.
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
    // The database referenced dev-root by name but never baked it — collations() (referenced set)
    // stays empty; the table came from the vendored set.
    assert_eq!(db.collations().len(), 0);
}

#[test]
fn unknown_collation_still_42704() {
    // The vendored fallback must not mask an unknown name: a collation neither referenced nor
    // vendored is still 42704 (undefined_object).
    let mut db = Database::new();
    let err = execute(&mut db, "SELECT 'x' COLLATE \"no-such-collation\"").unwrap_err();
    assert_eq!(err.code(), "42704");
}

#[test]
fn vendored_set_is_the_dev_fixtures() {
    // The binary vendors the dev fixture set, ascending by name (deterministic, no hash-iteration
    // leak — §8). `C` is never vendored (table-free, built in).
    let names: Vec<String> = jed::collation::vendored_collations()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(
        names,
        vec!["dev-nordic".to_string(), "dev-root".to_string()]
    );
    assert!(jed::collation::vendored_collation("dev-root").is_some());
    assert!(jed::collation::vendored_collation("C").is_none());
}
