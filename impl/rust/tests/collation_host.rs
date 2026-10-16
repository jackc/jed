//! Collation host API + persistence (spec/design/collation.md §1/§4.2): the host-loaded surface —
//! the environment's `load_unicode_data` (the `JUCD` bundle load), `set_default_collation` / `default_collation`,
//! the per-file `db.collations()` (what the database REFERENCES) vs the environment's
//! `db.loaded_collations()` (what a loaded bundle PROVIDES), per-column / per-database default
//! inheritance, collated keys, and the reference-only **file round-trip** (`format_version` 18,
//! `entry_kind = 3` metadata entries). These are the host-API + persistence behaviors the conformance
//! corpus cannot express (CLAUDE.md §10); the in-memory SQL behavior a collation drives (COLLATE /
//! ORDER BY / derivation / 42P21 / 42P22) lives in suites/collation/collate.test, which runs on every
//! core. There is **no `import_collation`**: the bare binary carries no Unicode data and the host
//! loads jed's own pinned bundle bytes (the SQLite model, §9/§16), then uses collations by name.
//! Mirrored by impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Environment, OpenOptions, Outcome, Session, SessionOptions};

use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

/// Remove a closed test database and the `<path>.lock/` coordination bundle opening it created
/// beside it (spec/design/locking.md).
fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
}

/// An environment that loaded jed's pinned production `JUCD` bundle
/// (spec/collation/fixtures/unicode.jucd) — what a production host builds once at startup and passes
/// to every create/open before running collated queries (collation.md §4, environment.md).
fn unicode_env() -> Environment {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation/fixtures/unicode.jucd");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    Environment::builder()
        .load_unicode_data(&bytes)
        .expect("load unicode.jucd")
        .build()
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

// ---- the loaded set (the environment property a bundle provides) ----

#[test]
fn loaded_collations_is_the_real_set() {
    // `db.loaded_collations()` reports what a loaded bundle PROVIDES — after loading jed's pinned
    // production bundle, the real version-pinned set (`es`, `unicode`), ascending by name, no
    // `is_default` (an engine property, not a per-db one). `C` is built in and never listed. The pin
    // is UCA/UCD 17.0.0 (spec/collation/17.0.0).
    let db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    let v = db.loaded_collations();
    let names: Vec<&str> = v.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["es", "unicode"]);
    assert!(v.iter().all(|c| !c.is_default));
    assert_eq!(v[1].name, "unicode");
    assert_eq!(v[1].unicode_version, "17.0.0");
    assert!(db.environment().collation("unicode").is_some());
    assert!(db.environment().collation("es").is_some());
    assert!(db.environment().collation("C").is_none());
}

// ---- using a loaded collation needs NO import ----

#[test]
fn loaded_collation_used_in_an_expression() {
    // `COLLATE "unicode"` resolves from the engine's loaded set with no import: 'ä' < 'z' is true
    // under the root (ä sorts near a), the opposite of the C byte order where it is false. A transient
    // query COLLATE does not make the database REFERENCE the collation, so db.collations() stays empty.
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    assert_eq!(
        db.set_default_collation("nope").unwrap_err().code(),
        "42704"
    );
    db.set_default_collation("C").unwrap(); // C always resolves (resets to byte order)
}

#[test]
fn collation_host_ops_rejected_in_read_only_or_failed_block() {
    // A host collation op inside an open block follows the block's statement rules: a READ ONLY
    // block is 25006 and an aborted block is 25P02, never a success that COMMIT then discards.
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.begin(false).unwrap();
    assert_eq!(
        db.set_default_collation("unicode").unwrap_err().code(),
        "25006"
    );
    assert_eq!(db.upgrade_collations().unwrap_err().code(), "25006");
    db.commit().unwrap();
    db.begin(true).unwrap();
    assert!(db.query_outcome("SELECT * FROM absent", &[]).is_err());
    assert_eq!(
        db.set_default_collation("unicode").unwrap_err().code(),
        "25P02"
    );
    assert_eq!(db.upgrade_collations().unwrap_err().code(), "25P02");
    db.rollback().unwrap();
    assert_eq!(db.default_collation(), "C");
}

// ---- collated keys (slice 1e, on-disk/internal — the corpus cannot express it) ----

#[test]
fn collated_primary_key_is_stored_in_collation_order() {
    // A collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so the B-tree
    // physically iterates in COLLATION order. unicode (loaded, no import): a < A < b < Z; C bytes:
    // A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
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
    let path = tmp("collation_refonly_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        skip_fsync: true,
        page_size: 256,
        environment: unicode_env(),
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

    let mut re = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            environment: unicode_env(),
            ..OpenOptions::default()
        },
    )
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
    remove_db(&path);
}

// ---- text[] collation (format_version 36 — collation.md §1 "A text[] carries its elements' collation") ----

fn array_texts(rows: Vec<Vec<Value>>) -> Vec<String> {
    rows.into_iter()
        .map(|r| match &r[0] {
            v @ Value::Array(_) => v.render(),
            other => panic!("expected an array, got {other:?}"),
        })
        .collect()
}

#[test]
fn text_array_collation_inherits_default_and_round_trips() {
    // An un-annotated text[] column inherits the per-database default (frozen), and a collated text[]
    // PRIMARY KEY stores its elements' sort keys — both survive a close + reopen (the array column
    // entry's has_collation bit + name), and a write after reopen keys under the same collation.
    let path = tmp("text_array_collation.jed");
    remove_db(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        page_size: 256,
        environment: unicode_env(),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.set_default_collation("unicode").unwrap();
    db.query_outcome(
        "CREATE TABLE t (k text[] PRIMARY KEY, c text[] COLLATE \"C\")",
        &[],
    )
    .unwrap();
    db.query_outcome(
        "INSERT INTO t VALUES ('{b}','{b}'),('{A}','{A}'),('{a}','{a}')",
        &[],
    )
    .unwrap();
    db.commit().unwrap();
    drop(db);

    let mut re = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            environment: unicode_env(),
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    re.query_outcome("INSERT INTO t VALUES ('{B}','{B}')", &[])
        .unwrap();
    // No ORDER BY: a single-table scan returns the stored (unicode-keyed) PK order.
    assert_eq!(
        array_texts(query(&mut re, "SELECT k FROM t")),
        vec!["{a}", "{A}", "{b}", "{B}"]
    );
    assert_eq!(
        array_texts(query(&mut re, "SELECT c FROM t ORDER BY c")),
        vec!["{A}", "{B}", "{a}", "{b}"]
    );
    drop(re);
    remove_db(&path);
}

#[test]
fn text_array_collation_conflict_is_raised_at_plan_time() {
    // A deliberate PG divergence (collation.md §15): PostgreSQL raises 42P22 for `a = c` over two
    // differently-collated text[] values only once its element texteq runs, so an empty table (or
    // arrays of different lengths) succeeds there; jed derives the conflict when the query is planned.
    let mut db = Database::create(CreateOptions {
        environment: unicode_env(),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome(
        "CREATE TABLE t (a text[] COLLATE \"unicode\", c text[] COLLATE \"C\")",
        &[],
    )
    .unwrap();
    assert_eq!(
        db.query_outcome("SELECT a = c FROM t", &[])
            .unwrap_err()
            .code(),
        "42P22"
    );
}
