//! Collation host API + persistence (spec/design/collation.md §1/§4.2): the host-loaded surface —
//! `load_unicode_data` (the `JUCD` bundle load seam), `set_default_collation` / `default_collation`,
//! the per-file `db.collations()` (what the database REFERENCES) vs the engine-global
//! `db.loaded_collations()` (what a loaded bundle PROVIDES), per-column / per-database default
//! inheritance, collated keys, and the reference-only **file round-trip** (`format_version` 18,
//! `entry_kind = 3` metadata entries). These are the host-API + persistence behaviors the conformance
//! corpus cannot express (CLAUDE.md §10); the in-memory SQL behavior a collation drives (COLLATE /
//! ORDER BY / derivation / 42P21 / 42P22) lives in suites/collation/collate.test, which runs on every
//! core. There is **no `import_collation`**: the bare binary carries no Unicode data and the host
//! loads jed's own pinned bundle bytes (the SQLite model, §9/§16), then uses collations by name.
//! Mirrored by impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

/// Load jed's pinned production `JUCD` bundle (spec/collation/fixtures/unicode.jucd) into the
/// engine-global loaded set — what a production host does once at startup via `db.LoadUnicodeData`
/// before opening files / running collated queries (collation.md §4). Idempotent (global, first-wins).
fn load_unicode() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation/fixtures/unicode.jucd");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    jed::load_unicode_data(&bytes).expect("load unicode.jucd");
}

fn query(db: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match db
        .query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
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

// ---- the loaded set (the engine-global property a bundle provides) ----

#[test]
fn loaded_collations_is_the_real_set() {
    // `db.loaded_collations()` reports what a loaded bundle PROVIDES — after loading jed's pinned
    // production bundle, the real version-pinned set (`es`, `unicode`), ascending by name, no
    // `is_default` (an engine property, not a per-db one). `C` is built in and never listed. The pin
    // is UCA/UCD 17.0.0 (spec/collation/17.0.0).
    load_unicode();
    let db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let v = db.loaded_collations();
    let names: Vec<&str> = v.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["es", "unicode"]);
    assert!(v.iter().all(|c| !c.is_default));
    assert_eq!(v[1].name, "unicode");
    assert_eq!(v[1].unicode_version, "17.0.0");
    assert!(jed::collation::loaded_collation("unicode").is_some());
    assert!(jed::collation::loaded_collation("es").is_some());
    assert!(jed::collation::loaded_collation("C").is_none());
}

// ---- using a loaded collation needs NO import ----

#[test]
fn loaded_collation_used_in_an_expression() {
    // `COLLATE "unicode"` resolves from the engine's loaded set with no import: 'ä' < 'z' is true
    // under the root (ä sorts near a), the opposite of the C byte order where it is false. A transient
    // query COLLATE does not make the database REFERENCE the collation, so db.collations() stays empty.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.collations().len(), 0);
    assert_eq!(
        query(&mut db, "SELECT 'ä' < 'z' COLLATE \"unicode\""),
        vec![vec![Value::Bool(true)]]
    );
}

#[test]
fn es_orders_enye_as_a_distinct_letter() {
    // The `es` tailoring (&N<ñ<<<Ñ) makes ñ a distinct PRIMARY letter after n: 'nz' < 'ña' (n < ñ),
    // whereas under the untailored root ñ is n+accent so 'ña' < 'nz'. The Spanish-collation headline.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        query(&mut db, "SELECT 'nz' < 'ña' COLLATE \"es\""),
        vec![vec![Value::Bool(true)]]
    );
    assert_eq!(
        query(&mut db, "SELECT 'nz' < 'ña' COLLATE \"unicode\""),
        vec![vec![Value::Bool(false)]]
    );
}

#[test]
fn unknown_collation_is_42704() {
    // A collation neither loaded nor referenced is 42704 (the loaded-set fallback must not mask it).
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.query_outcome("SELECT 'x' COLLATE \"no-such-collation\"", &[])
            .unwrap_err()
            .code(),
        "42704"
    );
}

#[test]
fn per_column_collation_orders_implicitly_and_is_referenced() {
    // A column declared `COLLATE "unicode"` (loaded, no import) sorts by that collation with no
    // explicit COLLATE on the query — unicode puts ä next to a. Because the SCHEMA now references
    // unicode, db.collations() (the per-file view) lists exactly it.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome(
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"unicode\")",
        &[],
    )
    .unwrap();
    db.query_outcome("INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')", &[])
        .unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    let refs = db.collations();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "unicode");
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
    // Two columns with DIFFERENT implicit (loaded) collations compared with no explicit COLLATE →
    // 42P22 (PG-matching). C counts as a distinct implicit collation, so unicode vs C also conflicts.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome(
        "CREATE TABLE t (a text COLLATE \"unicode\", b text COLLATE \"es\", c text COLLATE \"C\")",
        &[],
    )
    .unwrap();
    db.query_outcome("INSERT INTO t VALUES ('a','z','b')", &[])
        .unwrap();
    assert_eq!(
        db.query_outcome("SELECT a < b FROM t", &[])
            .unwrap_err()
            .code(),
        "42P22"
    );
    assert_eq!(
        db.query_outcome("SELECT a < c FROM t", &[])
            .unwrap_err()
            .code(),
        "42P22"
    );
    // An explicit COLLATE on one side breaks the tie (no error): a='a' < (b='z') = true.
    assert_eq!(
        query(&mut db, "SELECT a < b COLLATE \"unicode\" FROM t"),
        vec![vec![Value::Bool(true)]]
    );
    // The table references both vendored collations → db.collations() lists them (sorted).
    let names: Vec<String> = db.collations().into_iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["es".to_string(), "unicode".to_string()]);
}

#[test]
fn non_text_collate_is_42804_unknown_name_42704() {
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.query_outcome("CREATE TABLE t (a i32 COLLATE \"unicode\")", &[])
            .unwrap_err()
            .code(),
        "42804"
    );
    assert_eq!(
        db.query_outcome("CREATE TABLE t (a text COLLATE \"nope\")", &[])
            .unwrap_err()
            .code(),
        "42704"
    );
}

// ---- the per-database default (over the loaded set, no import) ----

#[test]
fn default_collation_inherited_by_unannotated_column() {
    // set_default_collation moves the per-database default to a LOADED collation (no import); an
    // un-annotated text column created AFTER inherits it (frozen), one created BEFORE keeps C.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.default_collation(), "C");
    db.query_outcome("CREATE TABLE before (id i32 PRIMARY KEY, name text)", &[])
        .unwrap();
    db.set_default_collation("unicode").unwrap();
    assert_eq!(db.default_collation(), "unicode");
    db.query_outcome("CREATE TABLE after (id i32 PRIMARY KEY, name text)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')", &[])
        .unwrap();
    // `after.name` inherited unicode → ä sorts next to a even with no COLLATE clause.
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM after ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    // `before.name` was frozen at C → byte order.
    db.query_outcome("INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')", &[])
        .unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM before ORDER BY name")),
        vec!["a", "z", "ä"]
    );
    // The default makes unicode referenced (is_default true).
    let refs = db.collations();
    assert_eq!(refs.len(), 1);
    assert!(refs[0].is_default);
}

#[test]
fn set_default_unknown_is_42704() {
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    // physically iterates in COLLATION order. unicode (loaded, no import): a < A < b < Z; C bytes:
    // A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome(
        "CREATE TABLE t (name text COLLATE \"unicode\" PRIMARY KEY)",
        &[],
    )
    .unwrap();
    db.query_outcome("INSERT INTO t VALUES ('Z'),('a'),('b'),('A')", &[])
        .unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM t")),
        vec!["a", "A", "b", "Z"]
    );
    db.query_outcome("CREATE TABLE c (name text PRIMARY KEY)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO c VALUES ('Z'),('a'),('b'),('A')", &[])
        .unwrap();
    assert_eq!(
        texts(query(&mut db, "SELECT name FROM c")),
        vec!["A", "Z", "a", "b"]
    );
}

#[test]
fn collated_unique_dedups_by_byte_identity() {
    // A collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A' are
    // DISTINCT, both admitted — collation.md §7), like a C unique key; only a byte-duplicate violates.
    load_unicode();
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome(
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"unicode\" UNIQUE)",
        &[],
    )
    .unwrap();
    db.query_outcome("INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')", &[])
        .unwrap();
    assert_eq!(
        db.query_outcome("INSERT INTO t VALUES (4,'a')", &[])
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
    // a metadata REFERENCE entry (no table); on reopen the table is resolved from a loaded bundle (the
    // host must have loaded one providing it BEFORE open — collation.md §4/§9).
    load_unicode();
    let path = tmp("collation_refonly_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.set_default_collation("unicode").unwrap(); // loaded — no import
    db.query_outcome(
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"unicode\", plain text)",
        &[],
    )
    .unwrap();
    db.query_outcome(
        "INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')",
        &[],
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);

    let mut re = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(re.default_collation(), "unicode");
    // The database still references unicode (per-file view) — resolved from the vendored set.
    let refs = re.collations();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "unicode");
    assert_eq!(refs[0].unicode_version, "17.0.0");
    assert!(refs[0].is_default);
    assert_eq!(
        texts(query(&mut re, "SELECT name FROM t ORDER BY name")),
        vec!["a", "ä", "z"]
    );
    // `plain` (un-annotated) inherited the default (unicode) at create → also unicode order.
    assert_eq!(
        texts(query(&mut re, "SELECT plain FROM t ORDER BY plain")),
        vec!["a", "ä", "z"]
    );
    drop(re);
    let _ = std::fs::remove_file(&path);
}
