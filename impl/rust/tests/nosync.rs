//! fsync=off (api.md §2.1) is a DEV/TESTING durability knob: a commit writes the identical bytes in
//! the same order but skips the `fdatasync` barrier. It must be byte/result-NEUTRAL — a database built
//! with it holds the exact same on-disk image and reads back identically; only the flush-to-platter is
//! skipped (so the data survives a process crash but not an OS crash). The conformance disk harness
//! runs with it to cut the fsync-per-commit cost. The corpus cannot express fsync timing or file-byte
//! identity, so this is a per-core unit test (CLAUDE.md §10). Mirrors impl/go/nosync_test.go and
//! impl/ts/tests/nosync.test.ts.

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, OpenOptions, Outcome, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("expected an int, got {other:?}"),
    }
}

/// Build a file database at `path` with the given `no_fsync` setting, run a fixed deterministic
/// workload (DDL + inserts + an update + a delete, autocommitted across many commits), and close it.
/// Deterministic (no clock/entropy), so two runs differing only in `no_fsync` must produce
/// byte-identical files.
fn build_sample_db(path: &PathBuf, no_fsync: bool) {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        no_fsync,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)", &[])
        .unwrap();
    for i in 1..=50 {
        db.query_outcome(
            &format!("INSERT INTO t VALUES ({i}, {}, 'row-{i}')", i * 10),
            &[],
        )
        .unwrap();
    }
    db.query_outcome("UPDATE t SET v = v + 1 WHERE id % 2 = 0", &[])
        .unwrap();
    db.query_outcome("DELETE FROM t WHERE id > 40", &[])
        .unwrap();
    drop(db);
}

/// A database built with fsync=off reopens in the same process (the OS page cache holds the un-synced
/// writes) with its committed state fully intact — fsync=off forfeits durability only across an OS
/// crash, not a clean close + reopen.
#[test]
fn no_fsync_round_trips() {
    let path = tmp("nosync_roundtrip.jed");
    build_sample_db(&path, true);
    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            no_fsync: true,
            ..Default::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    let rows = match db
        .query_outcome("SELECT id, v FROM t ORDER BY id", &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected a query"),
    };
    assert_eq!(rows.len(), 40, "50 inserted, ids 41..=50 deleted");
    // id=1 is odd → v=10; id=2 is even → v = 20 + 1 = 21 after the UPDATE.
    assert_eq!((int(&rows[0][0]), int(&rows[0][1])), (1, 10));
    assert_eq!((int(&rows[1][0]), int(&rows[1][1])), (2, 21));
}

/// The load-bearing guarantee: fsync=off changes only *when* bytes are flushed, never *which* bytes.
/// The same deterministic workload built with fsync on and off yields byte-identical files (so no
/// golden churn, no format bump, cross-core byte-identity preserved).
#[test]
fn no_fsync_byte_identical() {
    let on = tmp("nosync_on.jed");
    let off = tmp("nosync_off.jed");
    build_sample_db(&on, false);
    build_sample_db(&off, true);
    let a = std::fs::read(&on).unwrap();
    let b = std::fs::read(&off).unwrap();
    assert_eq!(
        a,
        b,
        "fsync=off changed the on-disk image: fsync-on={} bytes, fsync-off={} bytes",
        a.len(),
        b.len()
    );
}
