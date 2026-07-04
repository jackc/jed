//! P6.1 part B — incremental copy-on-write commit (spec/fileformat/format.md, *Allocation &
//! incremental commit*). A commit appends only the dirty pages a mutation introduced and publishes
//! the new root by alternating the meta slot, leaving the prior snapshot's pages intact. These
//! per-core tests cover what a static golden cannot (the bytes depend on commit history): that a
//! commit grows the file incrementally rather than rewriting it, that the meta slots alternate, and
//! that a torn write of the latest commit falls back to the prior durable snapshot.

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// `txid` of meta slot `slot` in a raw file image (page_size is the u32 at offset 8; the meta
/// header's txid is at offset 12 within the slot's page — spec/fileformat/format.md).
fn slot_txid(bytes: &[u8], slot: usize) -> u64 {
    let ps = be32(bytes, 8) as usize;
    be64(bytes, slot * ps + 12)
}

fn ids(db: &mut Session) -> Vec<i64> {
    match db.query_outcome("SELECT id FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Int(n) => *n,
                v => panic!("expected an int id, got {v:?}"),
            })
            .collect(),
        _ => panic!("expected a query"),
    }
}

#[test]
fn a_single_row_commit_appends_only_the_dirty_path() {
    let path = tmp("incremental_small_growth.jed");
    let _ = std::fs::remove_file(&path);
    let ps = 256u64;
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
        .unwrap();
    // Enough rows for a multi-level tree at 256-byte pages (≈3 records/leaf). Each insert
    // autocommits, so the file already holds many leaked pages by the end of the loop.
    let pad = "x".repeat(48);
    for i in 1..=30 {
        db.query_outcome(
            &format!("INSERT INTO t VALUES ({i}, 'row-{i:02}-{pad}')"),
            &[],
        )
        .unwrap();
    }

    // The whole tree spans many pages; a from-scratch image (no leaks) measures it.
    let whole_pages = db.to_image(db.page_size(), db.txid()).unwrap().len() as u64 / ps;
    assert!(
        whole_pages >= 10,
        "the tree should span several pages (got {whole_pages})"
    );

    // v25: within-session reclamation keeps the high-water bounded at ~2× the live tree across the 30
    // inserts (each insert copies its root→leaf path + catalog and *reclaims* the pages the prior root
    // abandoned) — NOT 30× the dirty-path size. So the committed `page_count` is a small multiple of
    // the whole (garbage-free) tree, proving the commit is incremental, not a whole-tree rewrite.
    let pc_before = db.page_count();
    assert!(
        (pc_before as u64) <= 3 * whole_pages,
        "within-session reclamation bounds the high-water at ~2× the {whole_pages}-page tree, not \
         monotonic churn growth (got {pc_before})"
    );
    // One more row: the incremental commit rebuilds only its root→leaf path + catalog (bounded by tree
    // height, not table size), and REUSES reclaimed free pages — so the high-water grows by at most a
    // handful of pages, and often not at all. We track the committed `page_count` delta, not the file
    // length — the file is preallocated ahead of the high-water (spec/design/pager.md §7).
    db.query_outcome(&format!("INSERT INTO t VALUES (31, 'row-31-{pad}')"), &[])
        .unwrap();
    let appended = (db.page_count() - pc_before) as u64;
    assert!(
        appended <= 8,
        "the dirty path is bounded by tree height, not table size, and reuses free pages (got \
         {appended})"
    );

    // And it reopens to the full, correct contents (leaked pages and all).
    drop(db);
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(ids(&mut db), (1..=31).collect::<Vec<_>>());
}

#[test]
fn delete_heavy_history_reopens_correctly() {
    // Deletes commit through the same incremental path but rebalance the tree (merge-then-split),
    // dirtying a different node set than inserts. Across many autocommitted inserts *and* deletes —
    // each leaking pages — the live snapshot must still reopen exactly (spec/fileformat/format.md).
    let path = tmp("incremental_deletes.jed");
    let _ = std::fs::remove_file(&path);
    let pad = "x".repeat(48);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
        .unwrap();
    for i in 1..=30 {
        db.query_outcome(
            &format!("INSERT INTO t VALUES ({i}, 'row-{i:02}-{pad}')"),
            &[],
        )
        .unwrap();
    }
    for i in 1..=20 {
        db.query_outcome(&format!("DELETE FROM t WHERE id = {i}"), &[])
            .unwrap();
    }
    drop(db);

    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(ids(&mut db), (21..=30).collect::<Vec<_>>());
}

#[test]
fn meta_slots_alternate_across_commits() {
    let path = tmp("incremental_alternation.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());

    // `create` seeds BOTH slots at txid 1, so two valid metas exist from the first moment.
    let img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 1);
    assert_eq!(slot_txid(&img, 1), 1);

    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap(); // txid 2 → slot 0
    db.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap(); // txid 3 → slot 1
    drop(db);

    // Each commit writes only the *alternate* slot, leaving the prior published meta intact.
    let img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 2, "even txid lands in slot 0");
    assert_eq!(slot_txid(&img, 1), 3, "odd txid lands in slot 1");

    let db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.txid(), 3, "open adopts the highest valid txid");
}

#[test]
fn torn_latest_commit_falls_back_to_prior_snapshot() {
    let path = tmp("incremental_torn_meta.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap(); // txid 2 (slot 0)
    db.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap(); // txid 3 (slot 1)
    db.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap(); // txid 4 (slot 0) — the newest commit
    drop(db);

    // Simulate a torn write of the newest commit: corrupt slot 0's checksum (txid 4). The loader
    // must fall back to slot 1 (txid 3) — whose body pages copy-on-write never overwrote — so row
    // 2's commit vanishes but the prior snapshot (row 1 only) is intact and uncorrupted.
    let mut img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 4, "slot 0 holds the newest commit");
    img[32] ^= 0xFF; // flip a CRC byte of slot 0's meta header
    std::fs::write(&path, &img).unwrap();

    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.txid(), 3, "fell back to the prior committed snapshot");
    assert_eq!(
        ids(&mut db),
        vec![1],
        "only the prior snapshot's row survives the torn write"
    );
}

/// The direct guard for the geometric preallocation policy (spec/design/pager.md §7): a tiny database
/// must not occupy a fixed 1 MiB on disk. A handful of rows at page_size 256 previously preallocated a
/// whole 1 MiB chunk (~4096 pages) for ~14 pages of data; with geometric growth the file stays
/// proportional — bounded by ≈2× the committed image plus the 16 KiB floor. Mirrors the Go/TS tests.
#[test]
fn small_database_file_stays_proportional() {
    let path = tmp("prealloc_small.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    for i in 0..30 {
        db.query_outcome(&format!("INSERT INTO t VALUES ({i}, {i})"), &[])
            .unwrap();
    }
    let logical = db.page_count() as u64 * db.page_size() as u64;
    drop(db);

    let physical = std::fs::metadata(&path).unwrap().len();
    assert!(
        physical < 1024 * 1024,
        "a tiny database must not preallocate a whole 1 MiB, got physical {physical}"
    );
    assert!(
        physical <= 2 * logical + 16 * 1024, // ≈2× the image + the 16 KiB floor
        "a {logical}-byte database should stay proportional, got physical {physical}"
    );
    assert!(
        physical >= logical,
        "the file must still cover the committed {logical}-byte image, got {physical}"
    );
}
