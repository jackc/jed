//! In-place trailing-free truncation (spec/fileformat/format.md *Trailing truncation*, storage.md §6).
//! A file commit whose top pages are all on the free list lowers the page high-water and, after its
//! barrier, shortens the file to one growth step past it. Page counts, file lengths, and the reader
//! watermark are host/internal state the corpus cannot see (CLAUDE.md §10). Mirrors
//! impl/go/truncate_test.go and impl/ts/tests/truncate.test.ts.

use std::path::{Path, PathBuf};

use jed::value::Value;
use jed::{CreateOptions, Database, SessionOptions};

const PAGE: u32 = 4096;
const PROBE: &str = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_dir_all(PathBuf::from(lock));
    path
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

/// `n` single-row updates of `t`, each its own durable commit, touching the table and its index.
fn churn(db: &Database, n: usize, tag: &str) {
    let mut s = db.session(SessionOptions::default());
    for k in 0..n {
        let sql = format!(
            "UPDATE t SET pad = '{tag}{k}' || repeat('y', 200), n = n + 7 WHERE id = {}",
            1 + k % 100
        );
        s.execute(&sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    }
}

fn rows_and_cost(db: &Database, sql: &str) -> (Vec<Vec<Value>>, i64) {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    let collected: Vec<Vec<Value>> = rows.by_ref().collect();
    rows.error().unwrap();
    (collected, rows.cost())
}

/// The pages `reserve` adds growing from `pages` (pager.md §7): clamped to [16 KiB, 1 MiB] of pages.
fn growth_step(pages: u32) -> u32 {
    pages.clamp(16 * 1024 / PAGE, 1024 * 1024 / PAGE)
}

#[test]
fn trailing_truncation_shrinks_a_file_after_a_delete() {
    let path = tmp("truncate.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    let peak = db.page_count();
    let peak_length = std::fs::metadata(&path).unwrap().len();
    churn(&db, 600, "a");
    let pc = db.page_count();
    assert!(
        pc * 3 <= peak,
        "trailing truncation returns the dead top of the file: {peak} pages at peak, {pc} after churn"
    );
    let length = std::fs::metadata(&path).unwrap().len();
    let limit = (pc + growth_step(pc)) as u64 * PAGE as u64;
    assert!(
        length <= limit && length < peak_length,
        "file is {length} bytes after truncation to {pc} pages (limit {limit}, peak {peak_length})"
    );
    let probe = rows_and_cost(&db, PROBE);
    let count = rows_and_cost(&db, "SELECT count(*) FROM t");
    drop(db);

    // The truncated file reopens to the same high-water, rows, and costs.
    let re = Database::open(&path).expect("reopen");
    assert_eq!(re.page_count(), pc);
    assert_eq!(rows_and_cost(&re, PROBE), probe);
    assert_eq!(rows_and_cost(&re, "SELECT count(*) FROM t"), count);
}

#[test]
fn trailing_truncation_waits_for_an_older_reader() {
    let path = tmp("truncate-pinned.jed");
    let db = create_file(&path);
    grow_and_shrink(&db);
    // A reader pinned at the post-delete version keeps every page of that version, so nothing is
    // truncated while it is open: the high-water never goes down.
    let reader = db.read_session();
    let mut high = db.page_count();
    {
        let mut s = db.session(SessionOptions::default());
        for k in 0..400 {
            s.execute(
                &format!("UPDATE t SET n = {k} WHERE id = {}", 1 + k % 100),
                &[],
            )
            .unwrap();
            let pc = db.page_count();
            assert!(
                pc >= high,
                "commit {k} lowered the high-water from {high} to {pc} under a pinned reader"
            );
            high = pc;
        }
    }
    drop(reader);
    churn(&db, 600, "b");
    assert!(
        db.page_count() < high,
        "after the reader closed, truncation resumes: {} pages, {high} before",
        db.page_count()
    );
    assert_eq!(
        rows_and_cost(&db, "SELECT count(*) FROM t").0,
        vec![vec![Value::Int(100)]]
    );
}
