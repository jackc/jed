//! Free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit allocator
//! reuses pages a prior root abandoned instead of always extending the file. Since **v25** the
//! free-list is **persisted** (meta offset 28 → a `page_type 7` chain) and reclamation is
//! **continuous within-session**: a file commit reclaims this commit's fresh orphans **in-commit**
//! (periodically — once the high-water passes ~2× the live count), so the high-water oscillates in
//! `[live, 2×live]` across a long churn rather than growing monotonically, and open reads the
//! persisted free-list directly (no reconstruction walk). These per-core tests cover what a static
//! golden cannot (the bytes depend on commit history): that within-session churn stays bounded, that
//! reopening reads the persisted free-list and a later churn stays bounded, that reuse round-trips,
//! and that a torn latest commit *after reuse* still falls back to the intact prior snapshot (a reused
//! page was dead, so overwriting it never damaged the fallback).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use jed::blockstore::{BlockStore, FileBlockStore};
use jed::pager::Pager;
use jed::value::Value;
use jed::{CreateOptions, Database, Engine, Outcome, Session, SessionOptions};

const PS: u64 = 256;

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The live meta slot's `free_list_head` (v25, meta offset 28) in a raw file image.
fn free_list_head(bytes: &[u8]) -> u32 {
    let ps = be32(bytes, 8) as usize;
    let live = if slot_txid(bytes, 0) >= slot_txid(bytes, 1) {
        0
    } else {
        1
    };
    be32(bytes, live * ps + 28)
}

/// The `page_type` (byte 0) of page `idx` in a raw file image.
fn page_type(bytes: &[u8], idx: u32) -> u8 {
    let ps = be32(bytes, 8) as usize;
    bytes[idx as usize * ps]
}

/// Count the `page_type 7` free-list pages in a raw file image (over all `page_count` pages).
fn count_freelist_pages(bytes: &[u8]) -> usize {
    let ps = be32(bytes, 8) as usize;
    let live = if slot_txid(bytes, 0) >= slot_txid(bytes, 1) {
        0
    } else {
        1
    };
    let page_count = be32(bytes, live * ps + 24) as usize;
    (0..page_count).filter(|&i| bytes[i * ps] == 7).count()
}

/// v25: after enough churn to build a multi-page free-list, the meta records a non-zero
/// `free_list_head` (offset 28) that heads a `page_type 7` chain, and reopening reads it back so the
/// file stays bounded — the persisted-free-list byte contract a static golden cannot pin (it depends
/// on commit history; format.md *Free-list page*).
#[test]
fn persisted_free_list_heads_a_page_type_7_chain() {
    let path = tmp("reclaim_persisted.jed");
    let mut db = setup(&path, 40);
    let big = "z".repeat(40);
    // Churn many rows so the free-list (and its page_type 7 chain) is comfortably non-empty and, at
    // page 256 (60 entries/free-list page), likely spans more than one chain page.
    for round in 0..40 {
        for id in 1..=40 {
            db.query_outcome(
                &format!("UPDATE t SET pad = 'r{round}-{id}-{big}' WHERE id = {id}"),
                &[],
            )
            .unwrap();
        }
    }
    drop(db);

    let bytes = std::fs::read(&path).unwrap();
    let head = free_list_head(&bytes);
    assert!(
        head >= 2,
        "the meta records a persisted free-list head (offset 28), got {head}"
    );
    assert_eq!(
        page_type(&bytes, head),
        7,
        "the free-list head page is page_type 7"
    );
    assert!(
        count_freelist_pages(&bytes) >= 1,
        "the file carries at least one persisted free-list page"
    );

    // Reopen (reads the persisted free-list, no reconstruction) and confirm the data round-trips and
    // the file is bounded (within-session reclamation, not monotonic churn growth).
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(ids(&mut db), (1..=40).collect::<Vec<_>>());
    assert!(
        db.page_count() < 200,
        "reopened file is bounded by within-session reclamation (got {})",
        db.page_count()
    );
}

/// `txid` of meta slot `slot` in a raw file image (spec/fileformat/format.md).
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

/// The `pad` text of the row with `id`, or `None` if absent.
fn pad_of(db: &mut Session, id: i64) -> Option<String> {
    match db
        .query_outcome(&format!("SELECT pad FROM t WHERE id = {id}"), &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => rows.first().map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            v => panic!("expected a text pad, got {v:?}"),
        }),
        _ => panic!("expected a query"),
    }
}

fn setup(path: &PathBuf, rows: i64) -> Session {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(path)),
        page_size: PS as u32,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
        .unwrap();
    let base = "x".repeat(40);
    for i in 1..=rows {
        db.query_outcome(
            &format!("INSERT INTO t VALUES ({i}, 'r{i:02}-{base}')"),
            &[],
        )
        .unwrap();
    }
    db
}

#[test]
fn within_session_churn_stays_bounded_and_reopens_from_the_persisted_free_list() {
    let path = tmp("reclaim_reuse.jed");
    let mut db = setup(&path, 30); // a multi-level tree at page 256
    let pad = "y".repeat(40);

    // Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the catalog
    // to fresh pages, and v25 reclaims the pages the prior root abandoned IN-COMMIT (periodically), so
    // the high-water oscillates in [live, 2×live] rather than growing monotonically with the 60
    // updates. (We track the committed `page_count`, not the file length — the file is preallocated in
    // chunks ahead of it, spec/design/pager.md §7.)
    for k in 0..60 {
        db.query_outcome(
            &format!("UPDATE t SET pad = 'a{k}-{pad}' WHERE id = 15"),
            &[],
        )
        .unwrap();
    }
    let pc_after_churn1 = db.page_count();
    assert!(
        pc_after_churn1 < 60,
        "within-session reclamation bounds the high-water (got {pc_after_churn1}); without it the 60 \
         updates would leak ~2 pages each",
    );
    drop(db);

    // Reopen: the free-list is read directly from the persisted chain (no reconstruction walk); the
    // high-water is whatever the last commit recorded.
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.page_count(),
        pc_after_churn1,
        "reopen reads the persisted high-water, unchanged"
    );

    // The first post-reopen commit reuses free pages from the persisted list rather than extending.
    db.query_outcome(&format!("UPDATE t SET pad = 'b0-{pad}' WHERE id = 15"), &[])
        .unwrap();
    assert!(
        db.page_count() <= pc_after_churn1 + 4,
        "the first commit after reopen reuses the persisted free-list (got {}, was {pc_after_churn1})",
        db.page_count(),
    );

    // A whole second churn stays bounded too — reusing reclaimed pages, the high-water does not grow
    // with the churn count.
    for k in 1..40 {
        db.query_outcome(
            &format!("UPDATE t SET pad = 'b{k}-{pad}' WHERE id = 15"),
            &[],
        )
        .unwrap();
    }
    assert!(
        db.page_count() <= 2 * pc_after_churn1,
        "the second churn stays bounded (got {}, ~2×{pc_after_churn1})",
        db.page_count(),
    );

    // And the data is exactly right (reuse never clobbered a live page).
    assert_eq!(
        pad_of(&mut db, 15).as_deref(),
        Some(&format!("b39-{pad}")[..])
    );
    assert_eq!(ids(&mut db), (1..=30).collect::<Vec<_>>());
    drop(db);
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        pad_of(&mut db, 15).as_deref(),
        Some(&format!("b39-{pad}")[..])
    );
    assert_eq!(ids(&mut db), (1..=30).collect::<Vec<_>>());
}

#[test]
fn heavy_insert_delete_churn_reopens_correctly_with_reuse() {
    // Insert/delete churn dirties a different node set than updates (split/merge rebalance) and,
    // across a reopen, exercises reuse over both. The live snapshot must reopen exactly.
    let path = tmp("reclaim_churn.jed");
    let mut db = setup(&path, 25);
    let pad = "z".repeat(40);
    // Repeatedly add then drop a high id, leaking pages each round.
    for k in 0..40 {
        db.query_outcome(&format!("INSERT INTO t VALUES (1000, 'k{k}-{pad}')"), &[])
            .unwrap();
        db.query_outcome("DELETE FROM t WHERE id = 1000", &[])
            .unwrap();
    }
    drop(db);

    // Reopen (free-list reconstructed) and churn again, now reusing reclaimed pages.
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    for k in 0..40 {
        db.query_outcome(&format!("INSERT INTO t VALUES (2000, 'm{k}-{pad}')"), &[])
            .unwrap();
        db.query_outcome("DELETE FROM t WHERE id = 2000", &[])
            .unwrap();
    }
    // Add a couple of permanent rows through the reused pages, then verify on a fresh open.
    db.query_outcome(&format!("INSERT INTO t VALUES (26, 'p-{pad}')"), &[])
        .unwrap();
    db.query_outcome(&format!("INSERT INTO t VALUES (27, 'q-{pad}')"), &[])
        .unwrap();
    drop(db);

    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(ids(&mut db), (1..=27).collect::<Vec<_>>());
}

#[test]
fn torn_commit_after_reuse_falls_back_to_the_intact_prior_snapshot() {
    let path = tmp("reclaim_torn.jed");
    let mut db = setup(&path, 20);
    let pad = "w".repeat(40);
    for k in 0..30 {
        db.query_outcome(
            &format!("UPDATE t SET pad = 'c{k}-{pad}' WHERE id = 10"),
            &[],
        )
        .unwrap();
    }
    drop(db);

    // Reopen so the free-list holds the churn's dead pages, then do two commits that *reuse* them.
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome(&format!("UPDATE t SET pad = 'A-{pad}' WHERE id = 10"), &[])
        .unwrap(); // prior snapshot
    let orig11 = pad_of(&mut db, 11).expect("row 11 exists");
    db.query_outcome(&format!("UPDATE t SET pad = 'B-{pad}' WHERE id = 11"), &[])
        .unwrap(); // newest commit
    drop(db);

    // Corrupt the newest meta slot's checksum (a torn write of the commit that reused free pages).
    let mut img = std::fs::read(&path).unwrap();
    let ps = PS as usize;
    let newest = if slot_txid(&img, 0) > slot_txid(&img, 1) {
        0
    } else {
        1
    };
    let prior_txid = slot_txid(&img, 1 - newest);
    img[newest * ps + 32] ^= 0xFF; // flip a CRC byte of the newest slot's meta header
    std::fs::write(&path, &img).unwrap();

    // The loader falls back to the prior snapshot — intact even though the torn commit reused
    // (overwrote) free pages, because those pages were dead and the prior snapshot never referenced
    // them. Row 11's update vanishes; row 10's prior-commit value and every row survive.
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.txid(),
        prior_txid,
        "fell back to the prior committed snapshot"
    );
    assert_eq!(
        pad_of(&mut db, 11).as_deref(),
        Some(&orig11[..]),
        "the torn commit's row-11 update vanished"
    );
    assert_eq!(
        pad_of(&mut db, 10).as_deref(),
        Some(&format!("A-{pad}")[..])
    );
    assert_eq!(ids(&mut db), (1..=20).collect::<Vec<_>>());
}

/// A `BlockStore` that counts `read_at` calls — the observable for the open-cost invariant below.
struct CountingStore {
    inner: FileBlockStore,
    reads: Arc<AtomicU32>,
}

impl BlockStore for CountingStore {
    fn read_at(&mut self, offset: u64, len: usize) -> jed::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> jed::Result<()> {
        self.inner.write_at(offset, bytes)
    }
    fn sync(&mut self) -> jed::Result<()> {
        self.inner.sync()
    }
    fn size(&mut self) -> jed::Result<u64> {
        self.inner.size()
    }
    fn set_size(&mut self, bytes: u64) -> jed::Result<()> {
        self.inner.set_size(bytes)
    }
}

/// Open reads the **interior spine, not every leaf** (spec/design/storage.md §6, "drop the eager
/// count"). Since v25 dropped the free-list reachability walk and this slice dropped the row-count
/// leaf sum, `open` faults only catalog + interior pages + ~one leaf per bottom-level interior (to
/// classify the level) + the meta/free-list pages — all O(interior spine). The block-read count must
/// therefore stay **well below the leaf count**, and above all must **not scale with it**. A counting
/// `BlockStore` is the only way to see this (it is not SQL-observable); the invariant is byte-identical
/// across cores, so the reference core pins it and the corpus disk mode covers the others' correctness.
#[test]
fn open_reads_the_interior_spine_not_every_leaf() {
    // A many-leaf table (page 256, ~4 rows/leaf) so "every leaf" is a large, distinctive number.
    let path = tmp("open_spine_only.jed");
    drop(setup(&path, 400));

    let bytes = std::fs::read(&path).unwrap();
    let ps = be32(&bytes, 8) as usize;
    let page_count = bytes.len() / ps;
    let leaves = (0..page_count).filter(|&i| bytes[i * ps] == 2).count(); // page_type 2 = leaf
    assert!(
        leaves >= 50,
        "the seed should span many leaves, got {leaves}"
    );

    // Open through the counting store and tally the block reads `open` performs.
    let reads = Arc::new(AtomicU32::new(0));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let store = CountingStore {
        inner: FileBlockStore::new(file, false),
        reads: Arc::clone(&reads),
    };
    let pager = Pager::from_store(Box::new(store)).unwrap();
    let db = Engine::open_paged(pager, 100_000).unwrap();
    let n = reads.load(Ordering::Relaxed) as usize;

    // The ceiling is deliberately loose (`< leaves`) — the point is that open does NOT read every
    // leaf, and in practice n ≪ leaves (only the spine + a peek per bottom-level interior).
    assert!(
        n < leaves,
        "open read {n} pages for a {leaves}-leaf table — it must read only the interior spine, not every leaf"
    );
    drop(db);
}
