//! Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6) — a host act the SQL
//! corpus cannot reach, so its byte-level contract lives here (CLAUDE.md §10): the compacted form is
//! exactly the file compaction leaves, the page-identical form copies every reachable page at its own
//! index and zeroes the rest, and the preconditions fail without writing. Backups through a page client
//! of another core's owner are the process corpus's (spec/conformance/process/backup.process.toml).
//! Mirrors impl/go/backup_test.go and impl/ts/tests/backup.test.ts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{CreateOptions, Database, OpenOptions, SessionOptions};

const PAGE: u32 = 4096;
const PS: usize = PAGE as usize;

/// The probe reads through the secondary index, so its cost covers index and table pages.
const PROBE: &str = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

/// A fresh directory under Cargo's per-test temp dir (never the repo tree).
fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn exec(db: &Database, sql: &str) {
    let mut s = db.session(SessionOptions::default());
    s.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

/// A file database with dead pages, a persisted free list, overflow chains, a secondary index, and
/// a GiST index, so a page-identical backup must walk every kind of page.
fn backup_fixture(path: &Path) -> Database {
    let db = Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: PAGE,
        skip_fsync: true,
        ..Default::default()
    })
    .expect("create file db");
    for sql in [
        "CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)",
        "CREATE INDEX t_n ON t (n)",
        "INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g",
        "DELETE FROM t WHERE id > 100",
        "CREATE TABLE big (id i64 PRIMARY KEY, body text)",
        "INSERT INTO big SELECT g, (SELECT string_agg(md5((g * 1000 + k)::text), '') FROM generate_series(1, 300) k) FROM generate_series(1, 20) g",
        "CREATE TABLE spans (id i64 PRIMARY KEY, r i64range)",
        "INSERT INTO spans SELECT g, i64range(g, g + 10) FROM generate_series(1, 300) g",
        "CREATE INDEX spans_r ON spans USING gist (r)",
        "UPDATE t SET n = n + 1 WHERE id % 2 = 0",
        "DELETE FROM big WHERE id > 10",
        "UPDATE t SET n = n + 1 WHERE id = 1",
    ] {
        exec(&db, sql);
    }
    db
}

/// The rendered rows and the cost of `sql`.
fn rows_and_cost(db: &Database, sql: &str) -> (Vec<Vec<Value>>, i64) {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    let collected: Vec<Vec<Value>> = rows.by_ref().collect();
    rows.error().unwrap();
    (collected, rows.cost())
}

fn count_rows(db: &Database, table: &str) -> i64 {
    match &rows_and_cost(db, &format!("SELECT count(*) FROM {table}")).0[0][0] {
        Value::Int(n) => *n,
        other => panic!("count(*) returned {other:?}"),
    }
}

fn code(result: jed::Result<()>) -> String {
    result.expect_err("expected an error").code().to_string()
}

fn open_copy(path: &Path) -> Database {
    Database::open_with_options(
        path,
        OpenOptions {
            skip_fsync: true,
            ..Default::default()
        },
    )
    .expect("open the backup")
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The database file up to the committed meta's page count (the file may carry preallocation past
/// it), and the committed meta slot's bytes. The newer of the two slots is the committed one (a
/// fixture file always has both slots valid).
fn committed_image(path: &Path) -> (Vec<u8>, Vec<u8>) {
    let image = std::fs::read(path).unwrap();
    let ps = be32(&image, 8) as usize;
    let slot = if be64(&image[ps..], 12) > be64(&image, 12) {
        1
    } else {
        0
    };
    let meta = image[slot * ps..(slot + 1) * ps].to_vec();
    let pages = be32(&meta, 24) as usize;
    (image[..pages * ps].to_vec(), meta)
}

#[test]
fn compacted_backup_is_the_compacted_file() {
    let dir = tmp_dir("backup-compacted");
    let path = dir.join("db.jed");
    let db = backup_fixture(&path);
    let dest = dir.join("backup.jed");
    db.backup_for_testing(&dest, false).unwrap();
    let backup = std::fs::read(&dest).unwrap();
    db.compact("main").unwrap();
    let (compacted, _) = committed_image(&path);
    assert!(
        backup == compacted,
        "the compacted backup ({} bytes) differs from the compacted file ({} bytes)",
        backup.len(),
        compacted.len()
    );
    assert!(
        !sibling(&dest, ".jedtmp").exists(),
        "the backup's temp file was left behind"
    );
    db.close().unwrap();
}

#[test]
fn page_identical_backup_copies_reachable_pages_at_their_indices() {
    let dir = tmp_dir("backup-page-identical");
    let path = dir.join("db.jed");
    let db = backup_fixture(&path);
    let dest = dir.join("backup.jed");
    db.backup_for_testing(&dest, true).unwrap();
    let (source, meta) = committed_image(&path);
    let txid = be64(&meta, 12);
    let page_count = be32(&meta, 24);
    assert_ne!(be32(&meta, 28), 0, "the fixture must persist a free list");
    let backup = std::fs::read(&dest).unwrap();
    assert_eq!(
        backup.len(),
        source.len(),
        "the backup spans the snapshot's page count"
    );
    let (pages, version) = db.backup_pages_for_testing().unwrap();
    assert_eq!(version, txid);
    let reached: HashSet<u32> = pages.into_iter().collect();
    let zero = vec![0u8; PS];
    let (mut copied, mut zeroed) = (0, 0);
    for p in 2..page_count as usize {
        let got = &backup[p * PS..(p + 1) * PS];
        if reached.contains(&(p as u32)) {
            copied += 1;
            assert!(
                got == &source[p * PS..(p + 1) * PS],
                "page {p} differs from the source"
            );
        } else {
            zeroed += 1;
            assert!(got == &zero[..], "unreachable page {p} is not zero");
        }
    }
    assert!(copied > 0);
    assert!(zeroed > 0, "the fixture must leave dead pages");
    // Both slots carry the bootstrap meta: the committed meta's fields, zero manifest fields and
    // tail, and a fresh checksum.
    let (slot0, slot1) = (&backup[..PS], &backup[PS..2 * PS]);
    assert!(
        slot0 == slot1,
        "both meta slots must carry the bootstrap meta"
    );
    assert_eq!(
        slot0[..32],
        meta[..32],
        "magic, version, page size, txid, root, pages, free head"
    );
    assert!(
        slot0[36..56].iter().all(|&b| b == 0),
        "zero manifest fields"
    );
    assert_eq!(slot0[56..60], meta[56..60], "the live page count");
    assert!(slot0[60..].iter().all(|&b| b == 0), "a zero tail");

    // The copy opens as the same database at the same version, with the same rows and costs.
    let copy = open_copy(&dest);
    assert_eq!(copy.txid(), txid, "the copy is at the snapshot's txid");
    for sql in [
        PROBE,
        "SELECT count(*), sum(length(body)) FROM big",
        "SELECT count(*) FROM spans WHERE r && i64range(100, 105)",
    ] {
        assert_eq!(rows_and_cost(&copy, sql), rows_and_cost(&db, sql), "{sql}");
    }
    // It keeps the source's free list and stays writable.
    exec(&copy, "INSERT INTO t VALUES (9001, 'tail', 1)");
    copy.close().unwrap();
    db.close().unwrap();
}

#[test]
fn backup_does_not_wait_for_the_writer_or_readers() {
    let dir = tmp_dir("backup-concurrent");
    let db = backup_fixture(&dir.join("db.jed"));
    let mut writer = db.session(SessionOptions::default());
    writer.begin(true).unwrap();
    writer
        .execute("INSERT INTO t VALUES (5000, 'pending', 1)", &[])
        .unwrap();
    let mut reader = db.read_session();
    reader.execute("SELECT count(*) FROM t", &[]).unwrap();
    for (i, page_identical) in [true, false].into_iter().enumerate() {
        let dest = dir.join(format!("backup{i}.jed"));
        db.backup_for_testing(&dest, page_identical)
            .unwrap_or_else(|e| {
                panic!("backup (page-identical {page_identical}) under an open writer: {e}")
            });
        let copy = open_copy(&dest);
        assert_eq!(
            count_rows(&copy, "t"),
            100,
            "the backup holds the committed rows, not the pending insert"
        );
        copy.close().unwrap();
    }
    drop(reader);
    writer.commit().unwrap();
    drop(writer);
    db.close().unwrap();
}

#[test]
fn backup_preconditions() {
    let dir = tmp_dir("backup-preconditions");
    let db = backup_fixture(&dir.join("db.jed"));
    let dest = dir.join("taken.jed");
    std::fs::write(&dest, b"keep").unwrap();
    for page_identical in [true, false] {
        assert_eq!(
            code(db.backup_for_testing(&dest, page_identical)),
            "58P02",
            "an existing destination"
        );
    }
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"keep",
        "a refused backup overwrote its destination"
    );
    db.close().unwrap();

    let mem = Database::create(CreateOptions {
        page_size: PAGE,
        ..Default::default()
    })
    .unwrap();
    exec(&mem, "CREATE TABLE m (id i64 PRIMARY KEY, v text)");
    exec(
        &mem,
        "INSERT INTO m SELECT g, g::text FROM generate_series(1, 50) g",
    );
    assert_eq!(
        code(mem.backup_for_testing(dir.join("mem.jed"), true)),
        "0A000",
        "a page-identical backup of an in-memory database"
    );
    // The compacted form copies an in-memory database to a file (SQLite's VACUUM INTO from :memory:).
    let mem_dest = dir.join("mem-compacted.jed");
    mem.backup_for_testing(&mem_dest, false).unwrap();
    let copy = open_copy(&mem_dest);
    assert_eq!(count_rows(&copy, "m"), 50, "the in-memory backup's rows");
    copy.close().unwrap();
}
