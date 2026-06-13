//! Physical lazy (read-on-touch) materialization of large values
//! (spec/design/large-values.md §14, phase 2). A lazily-loaded record holds unfetched
//! references for its external/compressed values; the scan layer resolves exactly the
//! query's touched columns through the pager, the open-time reachability walk follows
//! chains by headers only, and a dirty leaf's re-encode resolves what it must at commit.
//! These tests pin all three physically: corrupting every overflow-chain *payload* on disk
//! is invisible to open and to untouching queries, and surfaces as `XX001` only when the
//! spilled column is touched. Mirrored in Go (lazy_large_values_test.go) and TS
//! (tests/lazy_large_values.test.ts).

use jed::{Database, DatabaseOptions, Outcome, execute};

const PAGE_SIZE: u32 = 256;

/// Incompressible filler (spec/fileformat/format.md "Fixtures") — see overflow_cost.rs.
const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn filler_text(n: usize) -> String {
    let mut x: u32 = 0x4A45_4442;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            ALPHA64[(x % 64) as usize] as char
        })
        .collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    match execute(db, sql).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

fn query_rows(db: &mut Database, sql: &str) -> Vec<Vec<jed::Value>> {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected a query result"),
    }
}

/// One row per stored form at ps=256 (RECORD_MAX 116, cap 244): id 1 external-plain
/// (incompressible 600-char filler → a 3-page chain), id 2 external-compressed (half
/// filler / half run → the ~212-byte block spills to a 1-page chain), id 3
/// inline-compressed (a 600-char run), id 4 plain inline.
fn seed(db: &mut Database) {
    execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)").unwrap();
    let plain = filler_text(600);
    let extc = format!("{}{}", filler_text(200), "y".repeat(200));
    let inlc = "x".repeat(600);
    execute(
        db,
        &format!("INSERT INTO t VALUES (1, '{plain}'), (2, '{extc}'), (3, '{inlc}'), (4, 'tiny')"),
    )
    .unwrap();
}

/// Overwrite every overflow page's **payload** with 0xFF, keeping the 12-byte header
/// (page_type / item_count / next_page) intact — so the header-only chain walk still works
/// but any read of the chain's bytes yields garbage (non-UTF-8 for a plain text payload, a
/// malformed block for a compressed one).
fn corrupt_overflow_payloads(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let ps = PAGE_SIZE as usize;
    let pages = bytes.len() / ps;
    let mut corrupted = 0;
    for i in 2..pages {
        if bytes[i * ps] == 4 {
            for b in &mut bytes[i * ps + 12..(i + 1) * ps] {
                *b = 0xFF;
            }
            corrupted += 1;
        }
    }
    assert!(corrupted >= 4, "expected several overflow pages to corrupt");
    std::fs::write(path, &bytes).unwrap();
}

/// The core phase-2 pin: with every chain payload corrupted, open succeeds (the
/// reachability walk reads headers only), untouching queries succeed (no chain read, no
/// decompression), and touching the spilled column fails `XX001` — read-on-touch, physically.
#[test]
fn chains_are_read_only_when_touched() {
    let path = tmp("jed_lazy_touch.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: PAGE_SIZE,
            },
        )
        .unwrap();
        seed(&mut db);
        db.close().unwrap();
    }
    corrupt_overflow_payloads(&path);

    // Open walks live chains by headers only — corrupt payloads are invisible.
    let mut db = Database::open(&path).unwrap();

    // Untouching queries never read a chain or decompress a block.
    let ids = query_rows(&mut db, "SELECT id FROM t");
    assert_eq!(ids.len(), 4);
    let count = query_rows(&mut db, "SELECT count(*) FROM t");
    assert_eq!(count[0][0].render(), "4");

    // Touching the spilled column reads the chain: the corruption surfaces as XX001 —
    // non-UTF-8 for the external-plain text, a malformed LZ4 block for external-compressed.
    for id in [1, 2] {
        let err = execute(&mut db, &format!("SELECT body FROM t WHERE id = {id}"))
            .expect_err("a corrupted chain must fail when touched");
        assert_eq!(err.code(), "XX001", "id {id}");
    }

    // The inline-compressed and plain rows live in the (uncorrupted) leaf: still exact.
    let r3 = query_rows(&mut db, "SELECT body FROM t WHERE id = 3");
    assert_eq!(r3[0][0].render(), "x".repeat(600));
    let r4 = query_rows(&mut db, "SELECT body FROM t WHERE id = 4");
    assert_eq!(r4[0][0].render(), "tiny");

    db.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// All three lazy forms materialize exactly through the paged path (resolution correctness).
#[test]
fn lazy_values_round_trip_exactly() {
    let path = tmp("jed_lazy_roundtrip.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: PAGE_SIZE,
            },
        )
        .unwrap();
        seed(&mut db);
        db.close().unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let rows = query_rows(&mut db, "SELECT body FROM t");
    let got: Vec<String> = rows.iter().map(|r| r[0].render()).collect();
    assert_eq!(
        got,
        vec![
            filler_text(600),
            format!("{}{}", filler_text(200), "y".repeat(200)),
            "x".repeat(600),
            "tiny".to_string(),
        ]
    );
    db.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// An UPDATE that never touches the spilled column re-stores it without losing it: the
/// rewritten row resolves its unfetched values as part of the rewrite, the dirty leaf's
/// other rows resolve at commit, and a reopen reads everything back exactly
/// (large-values.md §14 — resolve-at-commit; chain sharing stays the deferred follow-on).
#[test]
fn update_of_other_columns_preserves_spilled_values() {
    let path = tmp("jed_lazy_update.jed");
    let _ = std::fs::remove_file(&path);
    let big = filler_text(600);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: PAGE_SIZE,
            },
        )
        .unwrap();
        execute(
            &mut db,
            "CREATE TABLE t (id int32 PRIMARY KEY, body text, n int32)",
        )
        .unwrap();
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES (1, '{big}', 10), (2, 'small', 20)"),
        )
        .unwrap();
        db.close().unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        // Dirties the leaf carrying row 1's unfetched body without touching it: row 2's
        // rewrite resolves nothing, row 1 resolves at commit.
        execute(&mut db, "UPDATE t SET n = 99 WHERE id = 2").unwrap();
        // Rewrites row 1 itself: the rewrite materializes its body (part of the write work).
        execute(&mut db, "UPDATE t SET n = 11 WHERE id = 1").unwrap();
        db.close().unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let rows = query_rows(&mut db, "SELECT body, n FROM t");
    assert_eq!(rows[0][0].render(), big);
    assert_eq!(rows[0][1].render(), "11");
    assert_eq!(rows[1][0].render(), "small");
    assert_eq!(rows[1][1].render(), "99");
    db.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// Logical cost is mode-independent (cost.md §3): a demand-paged file and a fully-resident
/// in-memory database charge identical costs for the same queries — the unfetched-reference
/// units equal the resident disposition plan's by construction.
#[test]
fn paged_and_resident_costs_match() {
    let path = tmp("jed_lazy_cost.jed");
    let _ = std::fs::remove_file(&path);
    let mut mem = Database::with_page_size(PAGE_SIZE);
    seed(&mut mem);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: PAGE_SIZE,
            },
        )
        .unwrap();
        seed(&mut db);
        db.close().unwrap();
    }
    let mut paged = Database::open(&path).unwrap();
    for sql in [
        "SELECT * FROM t",
        "SELECT id FROM t",
        "SELECT count(*) FROM t",
        "SELECT min(body) FROM t",
        "SELECT body FROM t WHERE id = 1",
        "SELECT body FROM t WHERE id = 4",
        "SELECT id FROM t WHERE body = 'tiny'",
    ] {
        assert_eq!(cost(&mut mem, sql), cost(&mut paged, sql), "{sql}");
    }
    paged.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
