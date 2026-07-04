//! Slice 7c — file-backed sessions + the default-session bridge (spec/design/session.md §2.4/§10).
//! These per-core tests cover what the corpus cannot express (host-API surface + concurrency +
//! on-disk durability): that `Database::create`/`open` return the shared core with a stateful default
//! session whose autocommit writes persist durably and survive a reopen; that file-backed read
//! sessions fault pages concurrently with a committing writer while staying snapshot-isolated; and
//! that a read-only open rejects writes (`25006`). The logical transaction/visibility semantics
//! themselves stay in the shared concurrency corpus (suites/concurrency/).

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, OpenOptions, Session, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn count_via(db: &mut Database) -> i64 {
    let rows: Vec<_> = db.query("SELECT count(*) FROM t", &[]).unwrap().collect();
    match &rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected an i64 count, got {other:?}"),
    }
}

fn count_session(s: &mut Session) -> i64 {
    let rows: Vec<_> = s.query("SELECT count(*) FROM t", &[]).unwrap().collect();
    match &rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected an i64 count, got {other:?}"),
    }
}

#[test]
fn create_default_session_persists_and_reopens() {
    let path = tmp("file_sessions_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(db.version(), 1); // the initial empty image is committed as version 1
        db.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        assert_eq!(db.version(), 2); // the autocommit CREATE published version 2
        for i in 1..=5 {
            db.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
                .unwrap();
        }
        assert_eq!(count_via(&mut db), 5);
    } // drop closes the handle; the autocommit writes are already durable

    // A fresh handle over the same file sees every committed row (the default-session bridge over a
    // demand-paged reopen).
    let mut db = Database::open(&path).unwrap();
    assert_eq!(count_via(&mut db), 5);
    assert_eq!(db.version(), 7); // 1 (create) + 1 (CREATE TABLE) + 5 (inserts)
    let _ = std::fs::remove_file(&path);
}

#[test]
fn explicit_transaction_on_a_session_persists_then_rolls_back() {
    let path = tmp("file_sessions_explicit_tx.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            ..Default::default()
        })
        .unwrap();
        // Explicit transactions live on a Session (the persistent default-session bridge was removed
        // from `Database`): mint one over the file-backed core and drive BEGIN/COMMIT/ROLLBACK on it.
        let mut s = db.session(SessionOptions::default());
        s.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        // A committed explicit block is durable.
        s.begin(true).unwrap();
        s.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap();
        s.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap();
        s.commit().unwrap();
        assert_eq!(count_session(&mut s), 2);
        // A rolled-back block leaves nothing.
        s.begin(true).unwrap();
        s.query_outcome("INSERT INTO t VALUES (3)", &[]).unwrap();
        s.rollback().unwrap();
        assert_eq!(count_session(&mut s), 2);
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(count_via(&mut db), 2); // only the committed block survived the reopen
    let _ = std::fs::remove_file(&path);
}

#[test]
fn execute_script_on_a_file_backed_default_session_is_all_or_nothing() {
    let path = tmp("file_sessions_script.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            ..Default::default()
        })
        .unwrap();
        let summary = db
            .execute_script(
                "CREATE TABLE t (id i64 PRIMARY KEY); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
            )
            .unwrap();
        assert_eq!(summary.statements_run, 3);
        assert_eq!(count_via(&mut db), 2);
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(count_via(&mut db), 2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_only_open_rejects_writes() {
    let path = tmp("file_sessions_read_only.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            ..Default::default()
        })
        .unwrap();
        db.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        db.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap();
    }
    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            read_only: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    // Reads work; a write is 25006 on the read-only handle (it never touches the file).
    assert_eq!(count_via(&mut db), 1);
    let err = db
        .query_outcome("INSERT INTO t VALUES (2)", &[])
        .unwrap_err();
    assert_eq!(err.code(), "25006");
    // A read/write session minted from a read-only core also rejects writes.
    let mut w = db.write_session();
    assert_eq!(
        w.query_outcome("INSERT INTO t VALUES (3)", &[])
            .unwrap_err()
            .code(),
        "25006",
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn file_backed_readers_run_concurrently_with_a_committing_writer() {
    // The deep 7c requirement: file-backed read sessions fault clean pages through the shared,
    // Mutex-guarded buffer pool concurrently with a writer committing (and persisting dirty pages)
    // on another thread. Each reader pins a snapshot and must see an internally consistent count;
    // reclamation stays trivially watermark-safe (reconstruct-on-open free-list). Run under
    // `rake concurrency:race` for the data-race assertion.
    let path = tmp("file_sessions_concurrent.jed");
    let _ = std::fs::remove_file(&path);
    {
        // Small pages so the table spans several leaves (exercising real page faults), and a tiny
        // cache so reads genuinely fault from disk rather than staying fully resident.
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            page_size: 256,
        })
        .unwrap();
        db.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        db.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap();
    }

    let db = Database::open_with_options(
        &path,
        OpenOptions {
            cache_bytes: 4 * 256, // only a handful of resident leaves
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let core = db.clone();

    let writer = {
        let core = core.clone();
        std::thread::spawn(move || {
            for i in 2..=40 {
                let mut w = core.write_session();
                w.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
                    .unwrap();
                w.commit().unwrap();
            }
        })
    };

    let readers: Vec<_> = (0..6)
        .map(|_| {
            let core = core.clone();
            std::thread::spawn(move || {
                for _ in 0..40 {
                    let mut r = core.read_session();
                    let first: i64 = {
                        let rows: Vec<_> =
                            r.query("SELECT count(*) FROM t", &[]).unwrap().collect();
                        match &rows[0][0] {
                            Value::Int(n) => *n,
                            v => panic!("expected count, got {v:?}"),
                        }
                    };
                    let second: i64 = {
                        let rows: Vec<_> =
                            r.query("SELECT count(*) FROM t", &[]).unwrap().collect();
                        match &rows[0][0] {
                            Value::Int(n) => *n,
                            v => panic!("expected count, got {v:?}"),
                        }
                    };
                    assert_eq!(first, second, "a pinned snapshot must not change mid-read");
                    assert!((1..=40).contains(&first), "count {first} out of range");
                }
            })
        })
        .collect();

    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    // create (v1) + CREATE TABLE (v2) + seed INSERT (v3) + 39 writer commits (ids 2..=40) = v42.
    assert_eq!(core.version(), 42);

    // Reopen from scratch: every committed row is durable on disk.
    drop(db);
    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(count_via(&mut reopened), 40);
    let _ = std::fs::remove_file(&path);
}
