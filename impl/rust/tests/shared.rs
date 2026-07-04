//! Phase 5 (P5.3b): the thread-safe shared handle — concurrent readers + a single writer, lock-
//! free reads, and the oldest-live-version watermark (spec/design/transactions.md §8/§10). The SQL
//! transaction semantics are pinned by the shared conformance corpus (suites/transactions/); these
//! per-core tests cover what the corpus cannot express — concurrency: that a reader pins a
//! consistent snapshot, runs in parallel with a writer without blocking it or being blocked, and
//! that the watermark tracks live readers (the Phase-6 reclamation gate).

use jed::{CreateOptions, Database, Session};

fn count(r: &mut Session) -> i64 {
    let rows: Vec<_> = r.query("SELECT count(*) FROM t", &[]).unwrap().collect();
    match &rows[0][0] {
        jed::Value::Int(n) => *n,
        other => panic!("expected an i64 count, got {other:?}"),
    }
}

/// Seed a shared db with table `t` holding the given ids, committed via a write handle.
fn seeded(ids: &[i64]) -> Database {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut w = db.write_session();
    w.query_outcome("CREATE TABLE t (id bigint PRIMARY KEY)", &[])
        .unwrap();
    for id in ids {
        w.query_outcome(&format!("INSERT INTO t VALUES ({id})"), &[])
            .unwrap();
    }
    w.commit().unwrap();
    db
}

#[test]
fn write_then_read_sees_committed_rows() {
    let db = seeded(&[1, 2, 3]);
    assert_eq!(db.version(), 1); // one commit ⇒ version 1
    let mut r = db.read_session();
    assert_eq!(count(&mut r), 3);
}

#[test]
fn read_handle_rejects_writes() {
    let db = seeded(&[1]);
    let mut r = db.read_session();
    // A write through a read handle is 25006 (read-only snapshot) — and does not poison it.
    let err = r
        .query_outcome("INSERT INTO t VALUES (2)", &[])
        .unwrap_err();
    assert_eq!(err.code(), "25006");
    assert_eq!(count(&mut r), 1); // still usable, still the pinned snapshot
}

#[test]
fn reader_does_not_block_on_an_open_writer() {
    // A reader running while a writer holds an *open, uncommitted* transaction must not block and
    // must see the pre-commit (committed) state — the core "readers parallel with a writer" claim.
    let db = seeded(&[1]);
    let mut w = db.write_session();
    w.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap(); // staged, not yet committed
    let mut r = db.read_session(); // does NOT block on the open writer
    assert_eq!(count(&mut r), 1); // sees only the committed row, not the writer's staged one
    w.commit().unwrap();
    assert_eq!(count(&mut r), 1); // the already-pinned reader is unaffected by the later commit
    let mut r2 = db.read_session();
    assert_eq!(count(&mut r2), 2); // a fresh reader sees the new row
}

#[test]
fn pinned_reader_is_isolated_from_a_concurrent_writer_thread() {
    // The writer runs on its own OS thread and commits; the reader pinned beforehand still sees
    // its original snapshot, and a reader opened afterward sees the writer's commit.
    let db = seeded(&[1]);
    let mut pinned = db.read_session(); // pins version 1 (one row)

    let writer = {
        let db = db.clone();
        std::thread::spawn(move || {
            let mut w = db.write_session();
            w.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap();
            w.commit().unwrap();
        })
    };
    writer.join().unwrap();

    assert_eq!(count(&mut pinned), 1); // snapshot isolation: pinned reader unchanged
    assert_eq!(db.version(), 2); // the writer's commit advanced the published version
    let mut fresh = db.read_session();
    assert_eq!(count(&mut fresh), 2); // a fresh reader sees both rows
}

#[test]
fn many_reader_threads_run_in_parallel_with_a_writer() {
    // Fan out reader threads while a writer thread commits repeatedly. Each reader pins a
    // consistent snapshot (a count that never changes mid-read) and never blocks. Exercised under
    // `cargo test` and, in CI, under a thread sanitizer / `--test-threads`; the assertion is that
    // every reader observes an internally-consistent snapshot.
    let db = seeded(&[1]);

    let writer = {
        let db = db.clone();
        std::thread::spawn(move || {
            for i in 2..=20 {
                let mut w = db.write_session();
                w.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
                    .unwrap();
                w.commit().unwrap();
            }
        })
    };

    let readers: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let mut r = db.read_session();
                    let first = count(&mut r);
                    let second = count(&mut r); // same pinned snapshot ⇒ identical
                    assert_eq!(first, second, "a pinned snapshot must not change mid-read");
                    assert!(
                        (1..=20).contains(&first),
                        "count {first} out of expected range"
                    );
                }
            })
        })
        .collect();

    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    // The seed committed once (version 1); the writer committed 19 more times (ids 2..=20).
    assert_eq!(db.version(), 20);
    let mut r = db.read_session();
    assert_eq!(count(&mut r), 20);
}

#[test]
fn oldest_live_txid_tracks_pinned_readers() {
    let db = seeded(&[1]); // version 1
    assert_eq!(db.version(), 1);
    assert_eq!(db.oldest_live_txid(), 1); // no readers ⇒ the committed version

    let r1 = db.read_session(); // pins version 1
    assert_eq!(db.oldest_live_txid(), 1);

    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap();
        w.commit().unwrap(); // version 2
    }
    assert_eq!(db.version(), 2);
    assert_eq!(db.oldest_live_txid(), 1); // r1 still pins v1 ⇒ watermark held at 1

    let r2 = db.read_session(); // pins version 2
    assert_eq!(db.oldest_live_txid(), 1); // still held by r1

    drop(r1);
    assert_eq!(db.oldest_live_txid(), 2); // r1 gone ⇒ watermark advances to r2's version

    drop(r2);
    assert_eq!(db.oldest_live_txid(), 2); // no readers ⇒ the committed version
}

#[test]
fn rolled_back_writer_publishes_nothing() {
    let db = seeded(&[1]);
    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap();
        w.rollback().unwrap();
    }
    let mut r = db.read_session();
    assert_eq!(count(&mut r), 1); // the rolled-back insert never became visible
    assert_eq!(db.version(), 1); // version unchanged by a rollback

    // A dropped (un-ended) writer also rolls back, and releases the gate for the next writer.
    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (3)", &[]).unwrap();
        // dropped here without commit
    }
    let mut r2 = db.read_session();
    assert_eq!(count(&mut r2), 1);
    assert_eq!(db.version(), 1);
}
