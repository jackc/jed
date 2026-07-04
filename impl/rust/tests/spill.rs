//! External merge sort with spill-to-disk for `ORDER BY` (spec/design/spill.md). Spill is **not** a
//! §8 byte contract (it changes *when* rows are resident, never *what* a query observes — like the
//! buffer pool), so it is verified per-core, not in the conformance corpus: a file-backed database
//! sorting under a **tiny `work_mem`** (which forces many sorted runs to spill + a k-way merge) must
//! return **byte-identical rows and cost** to the same query run fully in memory. These tests pin
//! that invariance across several `ORDER BY` shapes, the stable-sort tie-break the merge must
//! reproduce, and that no spill temp file leaks.

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Run a query, returning `(rows, cost)`.
fn run(db: &mut Session, sql: &str) -> (Vec<Vec<Value>>, i64) {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, cost, .. } => (rows, cost),
        other => panic!("expected a query result, got {other:?}"),
    }
}

/// Populate `t(id i32 PK, k i32, s text)` with `n` rows whose `k` is deliberately unsorted and
/// has many duplicates + a repeating NULL (to exercise the stable-sort tie-break and NULL ordering),
/// and a variable-length `s` (so a spilled run carries variable-width values).
fn seed(db: &mut Session, n: i64) {
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, k i32, s text)", &[])
        .unwrap();
    for id in 0..n {
        // A scrambled key with duplicates; every 7th row's key is NULL.
        let k = if id % 7 == 0 {
            "NULL".to_string()
        } else {
            ((id * 48271) % 100).to_string()
        };
        let s = "x".repeat((id % 17) as usize); // 0..16 chars, variable width
        db.query_outcome(&format!("INSERT INTO t VALUES ({id}, {k}, '{s}')"), &[])
            .unwrap();
    }
}

/// The set of `ORDER BY` shapes spill must reproduce exactly. Each is a single-table query that
/// takes the streaming external-sort path (spill.md §5).
const SHAPES: &[&str] = &[
    "SELECT id, k FROM t ORDER BY k, id",
    "SELECT id, k FROM t ORDER BY k DESC, id DESC",
    "SELECT k, id FROM t ORDER BY k NULLS FIRST, id",
    "SELECT id FROM t ORDER BY k, id LIMIT 13",
    "SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
    "SELECT id, s FROM t WHERE k > 20 ORDER BY s, id",
    "SELECT id FROM t ORDER BY k, id OFFSET 195",
];

#[test]
fn spilling_sort_matches_in_memory_rows_and_cost() {
    let path = tmp("spill_match.jed");
    let _ = std::fs::remove_file(&path);

    // The source of truth: the same data + queries against a pure in-memory database, which never
    // spills (spill.md §2).
    let mut mem = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    seed(&mut mem, 200);

    // A file-backed database with a tiny work_mem so every shape spills many runs and k-way-merges.
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    seed(&mut db, 200);
    db.set_work_mem(128); // ~2-3 rows per run → dozens of runs, deep merge

    for sql in SHAPES {
        let (want_rows, want_cost) = run(&mut mem, sql);
        let (got_rows, got_cost) = run(&mut db, sql);
        assert_eq!(got_rows, want_rows, "rows diverged under spill for: {sql}");
        assert_eq!(got_cost, want_cost, "cost diverged under spill for: {sql}");
    }

    // The same file-backed database with spill DISABLED (work_mem 0 = unlimited) must also match —
    // the in-memory fast path and the spilling path are the same sort.
    db.set_work_mem(0);
    for sql in SHAPES {
        let (want_rows, want_cost) = run(&mut mem, sql);
        let (got_rows, got_cost) = run(&mut db, sql);
        assert_eq!(
            got_rows, want_rows,
            "rows diverged with spill off for: {sql}"
        );
        assert_eq!(
            got_cost, want_cost,
            "cost diverged with spill off for: {sql}"
        );
    }

    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn spill_leaves_no_temp_files() {
    let dir = tmp("");
    let path = dir.join("spill_cleanup.jed");
    let _ = std::fs::remove_file(&path);

    let count_spill_files = || -> usize {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("jed-spill-"))
            .count()
    };
    let before = count_spill_files();

    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    seed(&mut db, 150);
    db.set_work_mem(64); // force heavy spilling

    // A full-consume sort and an early-stopped (LIMIT) sort both clean up their runs.
    let _ = run(&mut db, "SELECT id FROM t ORDER BY k, id");
    let _ = run(&mut db, "SELECT id FROM t ORDER BY k, id LIMIT 3");
    assert_eq!(
        count_spill_files(),
        before,
        "spill run files leaked after the queries finished"
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn spilling_sort_is_stable_on_ties() {
    // Every row shares the same key, so the entire result is one big tie: a stable sort keeps the
    // scan order (primary key = id ascending). The external sort reproduces it only if the merge
    // tie-breaks by (run, position) = input order (spill.md §6).
    let path = tmp("spill_stable.jed");
    let _ = std::fs::remove_file(&path);

    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, k i32)", &[])
        .unwrap();
    for id in 0..100 {
        db.query_outcome(&format!("INSERT INTO t VALUES ({id}, 5)"), &[])
            .unwrap();
    }
    db.set_work_mem(96); // force spilling so the merge tie-break is exercised

    let (rows, _) = run(&mut db, "SELECT id FROM t ORDER BY k");
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(n) => *n,
            v => panic!("expected int, got {v:?}"),
        })
        .collect();
    let expect: Vec<i64> = (0..100).collect();
    assert_eq!(
        ids, expect,
        "a fully-tied sort must keep primary-key scan order"
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
}
