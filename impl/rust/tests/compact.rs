//! Host compaction — `Database::compact` (spec/design/api.md §2.6). Compaction is a host-API act the
//! SQL corpus cannot reach, so its contract lives here: the rewrite returns dead pages and equals the
//! from-scratch image of the committed snapshot at the next version, rows/costs/prepared statements
//! survive it, a reopen sees it, and every precondition fails `42704`/`25006`/`55006` without writing.
//! Cross-process presence (another process holding the file) is covered by the shared process corpus
//! (spec/conformance/process/compact.process.toml). Mirrors impl/go/compact_test.go and
//! impl/ts/tests/compact.test.ts.

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{AttachSource, CreateOptions, Database, OpenOptions, SessionOptions};

const PAGE: u32 = 4096;

/// A path under Cargo's per-test temp dir (never the repo tree), cleared of any earlier run.
fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(sibling(&path, ".jedtmp"));
    let _ = std::fs::remove_dir_all(sibling(&path, ".lock"));
    path
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn create_file(path: &Path) -> Database {
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size: PAGE,
        skip_fsync: true,
        ..Default::default()
    })
    .expect("create file db")
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

fn code(result: jed::Result<()>) -> String {
    result.expect_err("expected an error").code().to_string()
}

const PROBE: &str = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

#[test]
fn file_compaction_returns_dead_pages_and_keeps_rows_and_costs() {
    let path = tmp("compact-file.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    let before_pages = db.page_count();
    let before_version = db.version();
    let (rows, cost) = rows_and_cost(&db, PROBE);
    let count = rows_and_cost(&db, "SELECT count(*) FROM t");

    db.compact("main").unwrap();

    let after_pages = db.page_count();
    assert!(
        after_pages * 4 < before_pages,
        "compaction returns dead pages: {before_pages} -> {after_pages}"
    );
    assert_eq!(db.version(), before_version + 1);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        after_pages as u64 * PAGE as u64,
        "the file is exactly its image, with no preallocation slack"
    );
    assert!(!sibling(&path, ".jedtmp").exists());
    assert_eq!(
        rows_and_cost(&db, PROBE),
        (rows.clone(), cost),
        "rows and cost survive"
    );
    assert_eq!(rows_and_cost(&db, "SELECT count(*) FROM t"), count);

    // The compacted file is the from-scratch image of the committed snapshot at the new version.
    let image = db.to_image(PAGE, db.version()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), image);

    // Later commits build on it, and a reopen sees both.
    let mut s = db.session(SessionOptions::default());
    s.execute("INSERT INTO t VALUES (5000, 'tail', 3)", &[])
        .unwrap();
    drop(s);
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    let (reopened, _) = rows_and_cost(&db, PROBE);
    assert_eq!(reopened.len(), rows.len() + 1);
    assert_eq!(&reopened[..rows.len()], &rows[..]);
    db.close().unwrap();
}

#[test]
fn compaction_of_a_compact_file_is_a_fixed_point() {
    let path = tmp("compact-fixed-point.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    db.compact("main").unwrap();
    let first = std::fs::read(&path).unwrap();
    db.compact("main").unwrap();
    let second = std::fs::read(&path).unwrap();
    assert_eq!(first.len(), second.len());
    // Only the version (and so the meta checksums) moved.
    assert_eq!(first[PAGE as usize * 2..], second[PAGE as usize * 2..]);
    db.close().unwrap();
}

#[test]
fn in_memory_compaction_lowers_storage_bytes() {
    let db = Database::create(CreateOptions {
        page_size: PAGE,
        ..Default::default()
    })
    .unwrap();
    grow_and_shrink(&db);
    let before = db.storage_bytes("main").unwrap();
    let (rows, cost) = rows_and_cost(&db, PROBE);
    let version = db.version();
    db.compact("MAIN").unwrap();
    assert!(db.storage_bytes("main").unwrap() * 4 < before);
    assert_eq!(db.version(), version + 1);
    assert_eq!(rows_and_cost(&db, PROBE), (rows, cost));
    let mut s = db.session(SessionOptions::default());
    s.execute("INSERT INTO t VALUES (5000, 'tail', 3)", &[])
        .unwrap();
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM t").0,
        vec![vec![Value::Int(101)]]
    );
}

#[test]
fn prepared_statements_survive_compaction() {
    let db = Database::create(CreateOptions::default()).unwrap();
    grow_and_shrink(&db);
    let mut s = db.session(SessionOptions::default());
    let stmt = s.prepare("SELECT count(*) FROM t WHERE n = $1").unwrap();
    let count = |s: &mut jed::Session| -> Vec<Vec<Value>> {
        s.query_prepared(&stmt, &[Value::Int(3)]).unwrap().collect()
    };
    let before = count(&mut s);
    db.compact("main").unwrap();
    assert_eq!(count(&mut s), before);
}

#[test]
fn compaction_preconditions_fail_without_writing() {
    let path = tmp("compact-preconditions.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    let pages = db.page_count();

    assert_eq!(code(db.compact("nope")), "42704");
    assert_eq!(code(db.compact("temp")), "42704");

    // A pinned reader blocks it, and so does an open cursor.
    let reader = db.read_session();
    assert_eq!(code(db.compact("main")), "55006");
    drop(reader);
    let mut s = db.session(SessionOptions::default());
    let mut rows = s.query("SELECT id FROM t", &[]).unwrap();
    assert!(rows.next().is_some());
    assert_eq!(code(db.compact("main")), "55006");
    drop(rows);

    // An open write transaction blocks it; compaction never waits for the writer gate.
    let mut writer = db.session(SessionOptions::default());
    writer.begin(true).unwrap();
    assert_eq!(code(db.compact("main")), "55006");
    writer.rollback().unwrap();
    drop((s, writer));

    assert_eq!(
        db.page_count(),
        pages,
        "a refused compaction writes nothing"
    );
    db.compact("main").unwrap();
    db.close().unwrap();

    let ro = Database::open_with_options(
        &path,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code(ro.compact("main")), "25006");
    ro.close().unwrap();
}

#[test]
fn attachments_compact_independently() {
    let work = tmp("compact-attachment.jed");
    {
        let db = create_file(&work);
        grow_and_shrink(&db);
        db.close().unwrap();
    }
    let db = Database::create(CreateOptions::default()).unwrap();
    db.attach("work", AttachSource::file(&work), false).unwrap();
    db.attach("scratch", AttachSource::memory(), false).unwrap();
    let main_version = db.version();
    let before = std::fs::metadata(&work).unwrap().len();
    db.compact("Work").unwrap();
    db.compact("scratch").unwrap();
    assert!(std::fs::metadata(&work).unwrap().len() * 4 < before);
    assert_eq!(db.version(), main_version, "main is untouched");
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM work.t").0,
        vec![vec![Value::Int(100)]]
    );
    let mut s = db.session(SessionOptions::default());
    s.execute("INSERT INTO work.t VALUES (5000, 'tail', 3)", &[])
        .unwrap();
    db.detach("work").unwrap();
    db.attach("frozen", AttachSource::file(&work), true)
        .unwrap();
    assert_eq!(code(db.compact("frozen")), "25006");
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM frozen.t").0,
        vec![vec![Value::Int(101)]]
    );
}

#[test]
fn a_stale_temp_file_is_ignored_and_replaced() {
    let path = tmp("compact-stale-temp.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    db.close().unwrap();
    // A crash mid-compaction leaves a partial temp file; open never reads it.
    std::fs::write(sibling(&path, ".jedtmp"), b"partial image").unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM t").0,
        vec![vec![Value::Int(100)]]
    );
    db.compact("main").unwrap();
    assert!(!sibling(&path, ".jedtmp").exists());
    db.close().unwrap();
}

#[cfg(unix)]
#[test]
fn compaction_keeps_symlinks_and_permission_bits() {
    use std::os::unix::fs::PermissionsExt;

    let path = tmp("compact-target.jed");
    let link = tmp("compact-link.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    db.close().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink(&path, &link).unwrap();

    let db = Database::open(&link).unwrap();
    db.compact("main").unwrap();
    db.close().unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let db = Database::open(&link).unwrap();
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM t").0,
        vec![vec![Value::Int(100)]]
    );
    db.close().unwrap();
}
