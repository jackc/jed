//! Host-API surface of the committed-storage limit (spec/design/memory.md §8, Q4a): the create and
//! attach options, the runtime setter and the `storage_bytes` gauge, the `0A000`/`42704` rejections,
//! the multi-root precheck, and the reader watermark's hold on forced compaction. Trip points
//! themselves are pinned in spec/conformance/suites/resource/storage_memory.test.

use jed::{AttachSource, CreateOptions, Database, SessionOptions};

const PAGE: i64 = 8192;

fn batch(lo: i32, n: i32) -> String {
    format!(
        "INSERT INTO t SELECT g, repeat('x', 1000) FROM generate_series({lo}, {}) g",
        lo + n - 1
    )
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "jed-storage-memory-{tag}-{}.jed",
        std::process::id()
    ))
}

#[test]
fn storage_limit_options_and_gauge() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let fresh = db.storage_bytes("main").unwrap();
    assert!(
        fresh > 0 && fresh % PAGE == 0,
        "a fresh image holds its meta and catalog pages"
    );
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    // Unlimited by default: a large insert commits.
    s.execute(&batch(0, 200), &[]).unwrap();
    let full = db.storage_bytes("MAIN").unwrap();
    assert!(full > fresh);
    // Lowering the limit below the current size blocks growth, not the database.
    db.set_max_storage_bytes("main", fresh).unwrap();
    let err = s.execute(&batch(200, 200), &[]).unwrap_err();
    assert_eq!(err.code(), "54P06");
    assert_eq!(
        db.storage_bytes("main").unwrap(),
        full,
        "a rejected commit writes nothing"
    );
    db.set_max_storage_bytes("main", -1).unwrap();
    s.execute(&batch(200, 200), &[]).unwrap();

    // The create option limits from the first commit.
    let db = Database::create(CreateOptions {
        max_storage_bytes: 8 * PAGE,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    assert_eq!(s.execute(&batch(0, 200), &[]).unwrap_err().code(), "54P06");

    // An in-memory attachment's limit, set on attach or later.
    db.attach("aux", AttachSource::memory().max_storage_bytes(PAGE), false)
        .unwrap();
    assert_eq!(
        s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)", &[])
            .unwrap_err()
            .code(),
        "54P06"
    );
    db.set_max_storage_bytes("aux", 0).unwrap();
    s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    assert!(db.storage_bytes("aux").unwrap() > PAGE);

    // Unknown databases and the session-local temp domain are not attachments.
    assert_eq!(db.storage_bytes("nope").unwrap_err().code(), "42704");
    assert_eq!(
        db.set_max_storage_bytes("temp", PAGE).unwrap_err().code(),
        "42704"
    );
}

#[test]
fn storage_limit_rejects_file_backings() {
    let path = temp_path("create");
    let _ = std::fs::remove_file(&path);
    let err = Database::create(CreateOptions {
        path: Some(path.clone()),
        max_storage_bytes: PAGE,
        ..Default::default()
    })
    .err()
    .expect("0A000");
    assert_eq!(err.code(), "0A000");
    assert!(!path.exists(), "a rejected create makes no file");

    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        db.set_max_storage_bytes("main", PAGE).unwrap_err().code(),
        "0A000"
    );
    db.set_max_storage_bytes("main", 0).unwrap();
    assert!(db.storage_bytes("main").unwrap() > 0);
    drop(db);

    let host = Database::create(CreateOptions::default()).unwrap();
    let err = host
        .attach(
            "f",
            AttachSource::file(&path).max_storage_bytes(PAGE),
            false,
        )
        .unwrap_err();
    assert_eq!(err.code(), "0A000");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn multi_root_rejection_packs_no_attachment() {
    let db = Database::create(CreateOptions::default()).unwrap();
    db.attach("aux", AttachSource::memory(), false).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    let main = db.storage_bytes("main").unwrap();
    let aux = db.storage_bytes("aux").unwrap();
    db.set_max_storage_bytes("main", main).unwrap();
    s.execute("BEGIN", &[]).unwrap();
    s.execute(
        "INSERT INTO aux.a SELECT g, repeat('y', 1000) FROM generate_series(1, 50) g",
        &[],
    )
    .unwrap();
    s.execute(&batch(0, 50), &[]).unwrap();
    assert_eq!(s.execute("COMMIT", &[]).unwrap_err().code(), "54P06");
    // Main is rejected after the attachment's precheck, before any domain packs a page.
    assert_eq!(db.storage_bytes("aux").unwrap(), aux);
    assert_eq!(db.storage_bytes("main").unwrap(), main);
    let rows = s.query_rows("SELECT count(*) FROM aux.a", ()).unwrap();
    assert_eq!(rows[0].get::<i64>(0).unwrap(), 0);
}

#[test]
fn pinned_reader_blocks_forced_compaction() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut w = db.session(SessionOptions::default());
    w.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    w.execute(&batch(0, 100), &[]).unwrap();
    db.set_max_storage_bytes("main", db.storage_bytes("main").unwrap())
        .unwrap();
    // A reader pinned at the current version keeps every page a compaction would keep: the delete
    // (admitted over the limit) orphans pages, and the next commit's forced compaction is allowed
    // only once no reader pins a version older than the committed one.
    let mut r = db.read_session();
    w.execute("DELETE FROM t WHERE id >= 50", &[]).unwrap();
    assert_eq!(w.execute(&batch(50, 50), &[]).unwrap_err().code(), "54P06");
    r.close();
    w.execute(&batch(50, 50), &[]).unwrap();
}
