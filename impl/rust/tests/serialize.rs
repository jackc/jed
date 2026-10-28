//! Database ↔ bytes — `Database::serialize` / `Database::deserialize` (spec/design/api.md §2.8).
//! Both are host-API acts the SQL corpus cannot reach, so their contract lives here: an export is the
//! from-scratch image of the committed snapshot at its version, it round-trips rows and costs, a raw
//! file's bytes load, the loaded copy is independent of the bytes and of the source, and the error
//! cases are `42704`/`0A000`/`55000`/`XX001`. Mirrors impl/go/serialize_test.go and
//! impl/ts/tests/serialize.test.ts.

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{
    AttachSource, CreateOptions, Credential, Database, DeserializeOptions, Environment,
    SessionOptions,
};

const PAGE: u32 = 4096;

fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(sibling(&path, ".lock"));
    path
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn memory() -> Database {
    Database::create(CreateOptions {
        page_size: PAGE,
        ..Default::default()
    })
    .unwrap()
}

/// Grow table `t` to 2000 wide rows, then delete all but 100: most pages end up dead.
fn grow_and_shrink(db: &Database) {
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)", &[])
        .unwrap();
    s.execute("CREATE INDEX t_n ON t (n)", &[]).unwrap();
    s.execute(
        "INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g",
        &[],
    )
    .unwrap();
    s.execute("DELETE FROM t WHERE id > 100", &[]).unwrap();
}

fn rows_and_cost(db: &Database, sql: &str) -> (Vec<Vec<Value>>, i64) {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s.query(sql, &[]).unwrap();
    let collected: Vec<Vec<Value>> = rows.by_ref().collect();
    rows.error().unwrap();
    (collected, rows.cost())
}

fn code<T>(result: jed::Result<T>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.code().to_string(),
    }
}

const PROBE: &str = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

#[test]
fn round_trip_keeps_rows_costs_and_version() {
    let db = memory();
    grow_and_shrink(&db);
    let bytes = db.serialize("main").unwrap();
    assert_eq!(bytes.len() % PAGE as usize, 0);
    assert!(
        (bytes.len() as u64) < db.page_count() as u64 * PAGE as u64,
        "the export holds no dead pages"
    );
    assert_eq!(bytes, db.to_image(PAGE, db.version()).unwrap());

    let copy = Database::deserialize(&bytes, DeserializeOptions::default()).unwrap();
    assert_eq!(copy.version(), db.version());
    assert_eq!(copy.page_size(), PAGE);
    assert_eq!(rows_and_cost(&copy, PROBE), rows_and_cost(&db, PROBE));
    // A clean image re-exports to the same bytes.
    assert_eq!(copy.serialize("main").unwrap(), bytes);

    // The copy is independent of its source in both directions.
    let mut s = copy.session(SessionOptions::default());
    s.execute("DELETE FROM t", &[]).unwrap();
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM t").0[0][0],
        Value::Int(100)
    );
    assert_eq!(copy.version(), db.version() + 1);
    let mut s = db.session(SessionOptions::default());
    s.execute("DELETE FROM t WHERE id > 50", &[]).unwrap();
    assert_eq!(
        rows_and_cost(&copy, "SELECT count(*) FROM t").0[0][0],
        Value::Int(0)
    );
}

#[test]
fn export_excludes_uncommitted_changes() {
    let db = memory();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.begin(true).unwrap();
    s.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
    // The writer holds the gate; the export never waits for it.
    let bytes = db.serialize("main").unwrap();
    s.commit().unwrap();
    let copy = Database::deserialize(&bytes, DeserializeOptions::default()).unwrap();
    assert_eq!(
        rows_and_cost(&copy, "SELECT id FROM t").0,
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn empty_database_round_trips() {
    let db = memory();
    let bytes = db.serialize("main").unwrap();
    assert_eq!(
        bytes.len(),
        3 * PAGE as usize,
        "two meta slots and the catalog"
    );
    let copy = Database::deserialize(&bytes, DeserializeOptions::default()).unwrap();
    assert_eq!(copy.version(), 0);
    assert!(copy.table_names().is_empty());
}

#[test]
fn raw_file_bytes_load_and_export_matches() {
    let path = tmp("serialize-file.jed");
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: PAGE,
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap();
    grow_and_shrink(&db);
    let exported = db.serialize("main").unwrap();
    let raw = std::fs::read(&path).unwrap();
    assert_ne!(raw, exported, "the file still holds its dead pages");

    let copy = Database::deserialize(&raw, DeserializeOptions::default()).unwrap();
    assert_eq!(copy.version(), db.version());
    assert_eq!(rows_and_cost(&copy, PROBE), rows_and_cost(&db, PROBE));
    assert_eq!(copy.serialize("main").unwrap(), exported);
    db.close().unwrap();
}

#[test]
fn attachments_by_name() {
    let db = memory();
    db.attach("scratch", AttachSource::memory(), false).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE scratch.u (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO scratch.u VALUES (7)", &[]).unwrap();
    let bytes = db.serialize("scratch").unwrap();
    let copy = Database::deserialize(&bytes, DeserializeOptions::default()).unwrap();
    assert_eq!(copy.table_names(), vec!["u".to_string()]);
    assert_eq!(
        rows_and_cost(&copy, "SELECT id FROM u").0,
        vec![vec![Value::Int(7)]]
    );

    assert_eq!(code(db.serialize("nope")), "42704");
    assert_eq!(code(db.serialize("temp")), "42704");
}

#[test]
fn encrypted_and_malformed_inputs_fail() {
    let path = tmp("serialize-encrypted.jed");
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: 1024,
        skip_fsync: true,
        environment: Environment::builder()
            .insecure_seeded_key_entropy(1)
            .build(),
        credential: Some(Credential::Key([0x4A; 32])),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(code(db.serialize("main")), "0A000");
    db.close().unwrap();
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(
        code(Database::deserialize(&raw, DeserializeOptions::default())),
        "55000"
    );

    assert_eq!(
        code(Database::deserialize(
            b"not a database",
            DeserializeOptions::default()
        )),
        "XX001"
    );
    assert_eq!(
        code(Database::deserialize(&[], DeserializeOptions::default())),
        "XX001"
    );
}

#[test]
fn storage_limit_applies_to_the_copy() {
    let db = memory();
    grow_and_shrink(&db);
    let bytes = db.serialize("main").unwrap();
    let copy = Database::deserialize(
        &bytes,
        DeserializeOptions {
            max_storage_bytes: bytes.len() as i64,
            ..Default::default()
        },
    )
    .unwrap();
    let mut s = copy.session(SessionOptions::default());
    let err = s
        .execute(
            "INSERT INTO t SELECT g, repeat('y', 300), 0 FROM generate_series(10000, 12000) g",
            &[],
        )
        .unwrap_err();
    assert_eq!(err.code(), "54P06");
}
