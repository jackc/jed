//! Crash-recovery tests driven by the **fault-injection seam** (spec/design/storage.md §7). These
//! verify the §4 commit atomicity at the **actual commit points** — mid-body, before the body sync,
//! between the body and meta syncs, and a torn meta write — which the static `torn_meta_slot*.jed`
//! goldens (a post-hoc byte corruption) cannot reach. The invariant under test: a crash **anywhere**
//! in a commit leaves the file readable as a **valid snapshot** (the prior one, or — at the last
//! barrier — the new one), never corrupt; and the free-list reconstruction (P6.2) stays correct after
//! a recovery. This is per-core, not corpus (a crash mid-commit is not SQL-level deterministic, like
//! P5.3 concurrency); the cross-core contract is the recovery *outcome*, asserted identically in Go
//! and TS (`crash_recovery_test.go`, `crash_recovery.test.ts`).

use crate::pager::{Fault, FaultPoint};
use crate::{Database, DatabaseOptions, Outcome, Result, Value, execute};

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// A fresh file-backed `t(id i32 PRIMARY KEY)` seeded with rows `1..=2`, returned with the prior
/// committed `txid`. Each autocommit `INSERT` persists durably, so the file holds a real two-row
/// commit before any fault is armed.
fn seeded(path: &std::path::Path) -> (Database, u64) {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(path, DatabaseOptions::default()).unwrap();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (2)").unwrap();
    let prior = db.txid();
    (db, prior)
}

/// The `t.id`s of a database, in primary-key order.
fn ids(db: &Database) -> Vec<i64> {
    db.rows_in_key_order("t")
        .unwrap()
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            ref v => panic!("non-int id: {v:?}"),
        })
        .collect()
}

/// Arm `fault`, then run an autocommit `INSERT (3)` that drives `persist` into it — which must fail.
fn insert_with_fault(db: &mut Database, fault: Fault) -> Result<Outcome> {
    db.arm_commit_fault(fault);
    execute(db, "INSERT INTO t VALUES (3)")
}

/// `BodyWrite(1)` — a clean crash on the first body-page write, before the body is even synced. The
/// new commit's pages are partial/unreferenced and the prior meta is untouched, so the file reopens at
/// the prior two-row snapshot.
#[test]
fn crash_mid_body_recovers_prior() {
    let path = tmp("jed_crash_mid_body.jed");
    let (mut db, prior) = seeded(&path);
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::BodyWrite(1), None)).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), prior, "fell back to the prior snapshot");
    assert_eq!(ids(&db), vec![1, 2], "the prior snapshot is intact");
    db.close().unwrap();
}

/// `BodyWrite(1)` torn — a partial first body-page write. A dirty page is always a freshly allocated
/// slot (copy-on-write never overwrites a page the prior meta references — P6.2 torn-safety), so the
/// torn page is unreferenced and the prior snapshot reopens intact.
#[test]
fn torn_body_page_recovers_prior() {
    let path = tmp("jed_torn_body.jed");
    let (mut db, prior) = seeded(&path);
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::BodyWrite(1), Some(64))).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), prior, "the torn body page is never referenced");
    assert_eq!(ids(&db), vec![1, 2]);
    db.close().unwrap();
}

/// `Sync(1)` — the body-durability barrier fails. The body pages are written-through but unsynced and
/// the meta is never written, so the prior meta still governs and the prior snapshot reopens.
#[test]
fn crash_before_body_sync_recovers_prior() {
    let path = tmp("jed_crash_body_sync.jed");
    let (mut db, prior) = seeded(&path);
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::Sync(1), None)).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), prior);
    assert_eq!(ids(&db), vec![1, 2]);
    db.close().unwrap();
}

/// `MetaWrite` — the critical between-syncs window (§4): the body is fully written **and synced**, then
/// the publish (the meta-slot write) crashes. The new body pages are durable but unreferenced; the
/// prior meta slot is untouched, so the file reopens at the prior snapshot. No corruption despite a
/// fully-durable new body on disk.
#[test]
fn crash_between_syncs_recovers_prior() {
    let path = tmp("jed_crash_between_syncs.jed");
    let (mut db, prior) = seeded(&path);
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::MetaWrite, None)).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.txid(),
        prior,
        "durable-but-unreferenced body → prior snapshot"
    );
    assert_eq!(ids(&db), vec![1, 2]);
    db.close().unwrap();
}

/// `MetaWrite` torn — a partial meta-slot write corrupts its checksum. The loader rejects the torn slot
/// (CRC mismatch) and falls back to the other, valid slot — the prior snapshot. This is the
/// `torn_meta_slot*.jed` golden's property, now exercised at the actual publish point.
#[test]
fn torn_meta_write_falls_back_to_prior() {
    let path = tmp("jed_torn_meta.jed");
    let (mut db, prior) = seeded(&path);
    // Write only the first 20 bytes of the meta page: the checksum at offset 32 keeps its old value
    // while bytes [0,32) change → CRC mismatch → the slot is invalid.
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::MetaWrite, Some(20))).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.txid(),
        prior,
        "torn meta slot rejected → fall back to the other slot"
    );
    assert_eq!(ids(&db), vec![1, 2]);
    db.close().unwrap();
}

/// `Sync(2)` — the meta is written, then its durability barrier fails. Atomicity holds either way: a
/// real power loss could keep the meta (→ new) or lose it (→ prior); the seam writes through, so the
/// reopen deterministically yields the **new** snapshot. Both are valid — the test asserts a
/// consistent, fully-readable snapshot that is exactly one of the two (never a half-published state).
#[test]
fn crash_before_meta_sync_is_atomic() {
    let path = tmp("jed_crash_meta_sync.jed");
    let (mut db, prior) = seeded(&path);
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::Sync(2), None)).is_err());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    let got = ids(&db);
    if db.txid() == prior {
        assert_eq!(got, vec![1, 2], "prior snapshot (meta lost)");
    } else {
        assert_eq!(db.txid(), prior + 1, "new snapshot (meta survived)");
        assert_eq!(got, vec![1, 2, 3], "new snapshot is fully consistent");
    }
    db.close().unwrap();
}

/// After a crash-to-prior recovery the file is fully functional: the free-list reconstructs correctly
/// on the reopen (P6.2), so subsequent commits reuse dead pages, persist durably, and round-trip — and
/// the file does not corrupt across the crash → reopen → churn → reopen cycle.
#[test]
fn recovery_then_free_list_reuse_stays_consistent() {
    let path = tmp("jed_recovery_then_reuse.jed");
    let (mut db, prior) = seeded(&path);

    // Crash between the syncs → reopen at the prior two-row snapshot.
    assert!(insert_with_fault(&mut db, Fault::new(FaultPoint::MetaWrite, None)).is_err());
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), prior);
    assert_eq!(ids(&db), vec![1, 2]);

    // Churn through several commits (frees pages a prior root abandoned, then reuses them) — all must
    // persist durably and round-trip after the recovery.
    execute(&mut db, "INSERT INTO t VALUES (3)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (4)").unwrap();
    execute(&mut db, "DELETE FROM t WHERE id = 1").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (5)").unwrap();
    let page_count_after = db.page_count;
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        ids(&db),
        vec![2, 3, 4, 5],
        "all post-recovery commits are durable and correct"
    );

    // A second churn round reuses the reconstructed free-list rather than growing the file unbounded.
    execute(&mut db, "DELETE FROM t WHERE id = 2").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (6)").unwrap();
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(ids(&db), vec![3, 4, 5, 6]);
    assert!(
        db.page_count <= page_count_after + 4,
        "free-list reuse keeps the file bounded after recovery (was {page_count_after}, now {})",
        db.page_count
    );
    db.close().unwrap();
}
