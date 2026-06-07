//! P6.2 — free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit
//! allocator reuses pages a prior root abandoned instead of always extending the file: on open the
//! free-list is reconstructed as `[2, page_count)` minus the committed root's reachable pages, and a
//! commit draws dirty/catalog pages from it (lowest-first) before extending. These per-core tests
//! cover what a static golden cannot (the bytes depend on commit history): that reopening reclaims
//! the dead pages a churn left so a later churn reuses them (the file stops growing), that reuse
//! round-trips, and that a torn latest commit *after reuse* still falls back to the intact prior
//! snapshot (a reused page was dead, so overwriting it never damaged the fallback).

use std::path::PathBuf;

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};

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

/// `txid` of meta slot `slot` in a raw file image (spec/fileformat/format.md).
fn slot_txid(bytes: &[u8], slot: usize) -> u64 {
    let ps = be32(bytes, 8) as usize;
    be64(bytes, slot * ps + 12)
}

/// `page_count` equals the file length in pages (format invariant `page_count = file_size /
/// page_size`), so the file size directly reports whether a commit extended the file or reused a
/// free page.
fn page_count(path: &PathBuf) -> u64 {
    std::fs::metadata(path).unwrap().len() / PS
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

/// The `pad` text of the row with `id`, or `None` if absent.
fn pad_of(db: &mut Database, id: i64) -> Option<String> {
    match execute(db, &format!("SELECT pad FROM t WHERE id = {id}")).unwrap() {
        Outcome::Query { rows, .. } => rows.first().map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            v => panic!("expected a text pad, got {v:?}"),
        }),
        _ => panic!("expected a query"),
    }
}

fn setup(path: &PathBuf, rows: i64) -> Database {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(
        path,
        DatabaseOptions {
            page_size: PS as u32,
        },
    )
    .unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)").unwrap();
    let base = "x".repeat(40);
    for i in 1..=rows {
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, 'r{i:02}-{base}')"),
        )
        .unwrap();
    }
    db
}

#[test]
fn reopen_reclaims_dead_pages_so_a_later_churn_reuses_them() {
    let path = tmp("reclaim_reuse.jed");
    let mut db = setup(&path, 30); // a multi-level tree at page 256
    let pad = "y".repeat(40);

    // Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the
    // catalog to fresh pages and *leaks* the old ones (P6.2 does not reclaim mid-session), so the
    // file grows monotonically across the 60 updates.
    for k in 0..60 {
        execute(
            &mut db,
            &format!("UPDATE t SET pad = 'a{k}-{pad}' WHERE id = 15"),
        )
        .unwrap();
    }
    let size_after_churn1 = page_count(&path);
    db.close().unwrap();

    // Reopen: the free-list is reconstructed from the ~60 churn iterations' dead pages.
    let mut db = Database::open(&path).unwrap();
    let pc_reopen = page_count(&path);
    assert_eq!(
        pc_reopen, size_after_churn1,
        "reopen does not change the file"
    );

    // The very first post-reopen commit reuses a free page rather than extending the file.
    execute(
        &mut db,
        &format!("UPDATE t SET pad = 'b0-{pad}' WHERE id = 15"),
    )
    .unwrap();
    assert_eq!(
        page_count(&path),
        pc_reopen,
        "the first commit after reopen reuses a dead page (no growth)"
    );

    // A whole second churn — shorter than the first, so the reclaimed pool covers it — extends the
    // file not at all: the page count after equals the count after the first churn.
    for k in 1..40 {
        execute(
            &mut db,
            &format!("UPDATE t SET pad = 'b{k}-{pad}' WHERE id = 15"),
        )
        .unwrap();
    }
    assert_eq!(
        page_count(&path),
        size_after_churn1,
        "reusing reclaimed pages, the second churn does not grow the file"
    );

    // And the data is exactly right (reuse never clobbered a live page).
    assert_eq!(
        pad_of(&mut db, 15).as_deref(),
        Some(&format!("b39-{pad}")[..])
    );
    assert_eq!(ids(&mut db), (1..=30).collect::<Vec<_>>());
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
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
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES (1000, 'k{k}-{pad}')"),
        )
        .unwrap();
        execute(&mut db, "DELETE FROM t WHERE id = 1000").unwrap();
    }
    db.close().unwrap();

    // Reopen (free-list reconstructed) and churn again, now reusing reclaimed pages.
    let mut db = Database::open(&path).unwrap();
    for k in 0..40 {
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES (2000, 'm{k}-{pad}')"),
        )
        .unwrap();
        execute(&mut db, "DELETE FROM t WHERE id = 2000").unwrap();
    }
    // Add a couple of permanent rows through the reused pages, then verify on a fresh open.
    execute(&mut db, &format!("INSERT INTO t VALUES (26, 'p-{pad}')")).unwrap();
    execute(&mut db, &format!("INSERT INTO t VALUES (27, 'q-{pad}')")).unwrap();
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(ids(&mut db), (1..=27).collect::<Vec<_>>());
}

#[test]
fn torn_commit_after_reuse_falls_back_to_the_intact_prior_snapshot() {
    let path = tmp("reclaim_torn.jed");
    let mut db = setup(&path, 20);
    let pad = "w".repeat(40);
    for k in 0..30 {
        execute(
            &mut db,
            &format!("UPDATE t SET pad = 'c{k}-{pad}' WHERE id = 10"),
        )
        .unwrap();
    }
    db.close().unwrap();

    // Reopen so the free-list holds the churn's dead pages, then do two commits that *reuse* them.
    let mut db = Database::open(&path).unwrap();
    execute(
        &mut db,
        &format!("UPDATE t SET pad = 'A-{pad}' WHERE id = 10"),
    )
    .unwrap(); // prior snapshot
    let orig11 = pad_of(&mut db, 11).expect("row 11 exists");
    execute(
        &mut db,
        &format!("UPDATE t SET pad = 'B-{pad}' WHERE id = 11"),
    )
    .unwrap(); // newest commit
    db.close().unwrap();

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
    let mut db = Database::open(&path).unwrap();
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
