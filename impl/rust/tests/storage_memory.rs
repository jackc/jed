//! Host-API surface of the committed-storage limit (spec/design/memory.md §8): the create, open, and
//! attach options, the runtime setter and the `storage_bytes` gauge, the `42704` rejections, the
//! multi-root precheck, the reader watermark's hold on forced compaction (in-memory, Q4a), and the
//! file form's live-page measure (§8.7). Trip points themselves are pinned in
//! spec/conformance/suites/resource/storage_memory.test and storage_file.test.

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

/// A fresh file-backed database with the live-page self-check on (memory.md §8.7): every commit in
/// these tests also recounts its live pages by reachability and panics on a mismatch.
fn file_db(tag: &str, max_storage_bytes: i64) -> (Database, std::path::PathBuf) {
    jed::tooling::VERIFY_LIVE_PAGES.store(true, std::sync::atomic::Ordering::Relaxed);
    let path = temp_path(tag);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        max_storage_bytes,
        ..Default::default()
    })
    .unwrap();
    (db, path)
}

fn remove_file_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
}

#[test]
fn file_storage_limit_measures_live_pages() {
    let (db, path) = file_db("live", 0);
    // A fresh file holds one live page: its catalog.
    assert_eq!(db.storage_bytes("main").unwrap(), PAGE);
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    s.execute(&batch(0, 200), &[]).unwrap();
    let full = db.storage_bytes("main").unwrap();
    assert!(full > PAGE && full % PAGE == 0);
    // A delete lowers the measure at once; the file keeps its high-water and free pages.
    let high_water = db.page_count();
    s.execute("DELETE FROM t WHERE id >= 100", &[]).unwrap();
    let half = db.storage_bytes("main").unwrap();
    assert!(half < full);
    assert!(db.page_count() >= high_water);

    // The setter limits the file from the next commit; a rejected commit writes nothing.
    db.set_max_storage_bytes("main", half).unwrap();
    assert_eq!(
        s.execute(&batch(100, 100), &[]).unwrap_err().code(),
        "54P06"
    );
    assert_eq!(db.storage_bytes("main").unwrap(), half);
    // A commit that does not grow the live count is admitted at the limit.
    s.execute("UPDATE t SET v = 'short' WHERE id < 10", &[])
        .unwrap();
    let rows = s.query_rows("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(rows[0].get::<i64>(0).unwrap(), 100);
    let measured = db.storage_bytes("main").unwrap();
    drop(s);
    drop(db);

    // The count is persisted: a reopen reports it without walking the file, and the open option
    // sets the limit.
    let db = Database::open_with_options(
        &path,
        jed::OpenOptions {
            skip_fsync: true,
            max_storage_bytes: measured,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db.storage_bytes("main").unwrap(), measured);
    let mut s = db.session(SessionOptions::default());
    assert_eq!(
        s.execute(&batch(100, 100), &[]).unwrap_err().code(),
        "54P06"
    );
    db.set_max_storage_bytes("main", 0).unwrap();
    s.execute(&batch(100, 100), &[]).unwrap();
    drop(s);
    drop(db);
    remove_file_db(&path);
}

#[test]
fn file_storage_limit_counts_every_live_structure() {
    // Indexes (B-tree, GIN, GiST), overflow chains, a drop, and a host compaction, each checked by
    // the reachability recount.
    let (db, path) = file_db("structures", 0);
    let mut s = db.session(SessionOptions::default());
    s.execute(
        "CREATE TABLE t (id i32 PRIMARY KEY, k i32, v text, a i32[], r i32range)",
        &[],
    )
    .unwrap();
    s.execute("CREATE INDEX t_k ON t (k)", &[]).unwrap();
    s.execute("CREATE INDEX t_a ON t USING gin (a)", &[])
        .unwrap();
    s.execute("CREATE INDEX t_r ON t USING gist (r)", &[])
        .unwrap();
    // An incompressible value larger than a record spills into its own overflow chain per row.
    let mut x: u32 = 0x4A45_4442;
    let filler: String = (0..5000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            char::from(b'A' + (x % 26) as u8)
        })
        .collect();
    s.execute(
        "INSERT INTO t VALUES (0, 0, $1, '{0,1}', '[0,5)')",
        &[jed::Value::Text(filler)],
    )
    .unwrap();
    let rows: Vec<String> = (1..=300)
        .map(|g| format!("({g}, '{{{g},{}}}', '[{g},{})')", g + 1, g + 5))
        .collect();
    s.execute(
        &format!("INSERT INTO t (id, a, r) VALUES {}", rows.join(", ")),
        &[],
    )
    .unwrap();
    s.execute(
        "UPDATE t SET k = id % 7, v = (SELECT v FROM t WHERE id = 0)",
        &[],
    )
    .unwrap();
    let full = db.storage_bytes("main").unwrap();
    s.execute("UPDATE t SET v = left(v, 10) WHERE id % 3 = 0", &[])
        .unwrap();
    s.execute("DELETE FROM t WHERE id > 150", &[]).unwrap();
    assert!(db.storage_bytes("main").unwrap() < full);
    s.execute("CREATE TABLE u (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    s.execute("INSERT INTO u SELECT id, v FROM t", &[]).unwrap();
    s.execute("DROP TABLE t", &[]).unwrap();
    let before = db.storage_bytes("main").unwrap();
    db.compact("main").unwrap();
    // Compaction renumbers pages but keeps every live one.
    assert_eq!(db.storage_bytes("main").unwrap(), before);
    s.execute("DROP TABLE u", &[]).unwrap();
    assert_eq!(db.storage_bytes("main").unwrap(), PAGE);
    drop(s);
    drop(db);
    remove_file_db(&path);
}

#[test]
fn file_attachment_storage_limit() {
    let (mut file, path) = file_db("attach", 0);
    file.execute("CREATE TABLE a (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    drop(file);
    let host = Database::create(CreateOptions::default()).unwrap();
    host.attach(
        "f",
        AttachSource::file(&path).max_storage_bytes(4 * PAGE),
        false,
    )
    .unwrap();
    let mut s = host.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)", &[])
        .unwrap();
    let main = host.storage_bytes("main").unwrap();
    let aux = host.storage_bytes("f").unwrap();
    // A multi-root commit rejected in the file attachment publishes neither database.
    s.execute("BEGIN", &[]).unwrap();
    s.execute(&batch(0, 10), &[]).unwrap();
    s.execute(
        "INSERT INTO f.a SELECT g, repeat('y', 1000) FROM generate_series(1, 100) g",
        &[],
    )
    .unwrap();
    assert_eq!(s.execute("COMMIT", &[]).unwrap_err().code(), "54P06");
    assert_eq!(host.storage_bytes("main").unwrap(), main);
    assert_eq!(host.storage_bytes("f").unwrap(), aux);
    let rows = s
        .query_rows(
            "SELECT (SELECT count(*) FROM t), (SELECT count(*) FROM f.a)",
            (),
        )
        .unwrap();
    assert_eq!(rows[0].get::<i64>(0).unwrap(), 0);
    assert_eq!(rows[0].get::<i64>(1).unwrap(), 0);
    // A small write fits.
    s.execute("INSERT INTO f.a VALUES (1, 'z')", &[]).unwrap();
    assert!(host.storage_bytes("f").unwrap() <= 4 * PAGE);
    drop(s);
    host.detach("f").unwrap();
    drop(host);
    remove_file_db(&path);
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
