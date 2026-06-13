//! P6.1 part B — incremental copy-on-write commit (spec/fileformat/format.md, *Allocation &
//! incremental commit*). A commit appends only the dirty pages a mutation introduced and publishes
//! the new root by alternating the meta slot, leaving the prior snapshot's pages intact. These
//! per-core tests cover what a static golden cannot (the bytes depend on commit history): that a
//! commit grows the file incrementally rather than rewriting it, that the meta slots alternate, and
//! that a torn write of the latest commit falls back to the prior durable snapshot.

use std::path::PathBuf;

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};

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

fn ids(db: &mut Database) -> Vec<i64> {
    match execute(db, "SELECT id FROM t").unwrap() {
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
    let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)").unwrap();
    // Enough rows for a multi-level tree at 256-byte pages (≈3 records/leaf). Each insert
    // autocommits, so the file already holds many leaked pages by the end of the loop.
    let pad = "x".repeat(48);
    for i in 1..=30 {
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, 'row-{i:02}-{pad}')"),
        )
        .unwrap();
    }

    // The whole tree spans many pages; a from-scratch image (no leaks) measures it.
    let whole_pages = db.to_image(db.page_size(), db.txid()).unwrap().len() as u64 / ps;
    assert!(
        whole_pages >= 10,
        "the tree should span several pages (got {whole_pages})"
    );

    // One more row: the incremental commit appends only the rebuilt root→leaf path + catalog —
    // far fewer pages than the whole tree, and bounded by tree height, not table size. We track the
    // committed `page_count` delta, not the file length — the file is preallocated in chunks ahead
    // of the high-water (spec/design/pager.md §7), so its physical size jumps by a chunk, not by the
    // dirty-page count.
    let pc_before = db.page_count();
    execute(
        &mut db,
        &format!("INSERT INTO t VALUES (31, 'row-31-{pad}')"),
    )
    .unwrap();
    let appended = (db.page_count() - pc_before) as u64;
    assert!(
        appended >= 2,
        "the commit must append its dirty path + catalog (got {appended})"
    );
    assert!(
        appended < whole_pages,
        "an incremental commit ({appended} pages) must not rewrite the whole {whole_pages}-page tree"
    );
    assert!(
        appended <= 8,
        "the dirty path is bounded by tree height, not table size (got {appended})"
    );

    // And it reopens to the full, correct contents (leaked pages and all).
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
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
    let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)").unwrap();
    for i in 1..=30 {
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, 'row-{i:02}-{pad}')"),
        )
        .unwrap();
    }
    for i in 1..=20 {
        execute(&mut db, &format!("DELETE FROM t WHERE id = {i}")).unwrap();
    }
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(ids(&mut db), (21..=30).collect::<Vec<_>>());
}

#[test]
fn meta_slots_alternate_across_commits() {
    let path = tmp("incremental_alternation.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();

    // `create` seeds BOTH slots at txid 1, so two valid metas exist from the first moment.
    let img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 1);
    assert_eq!(slot_txid(&img, 1), 1);

    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap(); // txid 2 → slot 0
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap(); // txid 3 → slot 1
    db.close().unwrap();

    // Each commit writes only the *alternate* slot, leaving the prior published meta intact.
    let img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 2, "even txid lands in slot 0");
    assert_eq!(slot_txid(&img, 1), 3, "odd txid lands in slot 1");

    let db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), 3, "open adopts the highest valid txid");
}

#[test]
fn torn_latest_commit_falls_back_to_prior_snapshot() {
    let path = tmp("incremental_torn_meta.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap(); // txid 2 (slot 0)
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap(); // txid 3 (slot 1)
    execute(&mut db, "INSERT INTO t VALUES (2)").unwrap(); // txid 4 (slot 0) — the newest commit
    db.close().unwrap();

    // Simulate a torn write of the newest commit: corrupt slot 0's checksum (txid 4). The loader
    // must fall back to slot 1 (txid 3) — whose body pages copy-on-write never overwrote — so row
    // 2's commit vanishes but the prior snapshot (row 1 only) is intact and uncorrupted.
    let mut img = std::fs::read(&path).unwrap();
    assert_eq!(slot_txid(&img, 0), 4, "slot 0 holds the newest commit");
    img[32] ^= 0xFF; // flip a CRC byte of slot 0's meta header
    std::fs::write(&path, &img).unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), 3, "fell back to the prior committed snapshot");
    assert_eq!(
        ids(&mut db),
        vec![1],
        "only the prior snapshot's row survives the torn write"
    );
}
