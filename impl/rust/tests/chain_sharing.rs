//! Chain sharing on rewrite (spec/design/large-values.md §15). A rewritten record keeps an
//! unchanged large value's stored form: an UPDATE that does not assign the column, and the other
//! rows of a leaf a commit rewrites, write the value from its stored bytes and keep its overflow
//! chain — no chain read, no decompression, no chain write. The plan, the bytes other than page
//! numbers, and the cost are those of a fresh plan, so cost is the same whether a row was read
//! from disk or is still resident. These tests pin the physical sharing (corrupt chain payloads
//! survive a rewrite untouched; no overflow page is written) and the cost identity. Mirrored in Go
//! (chain_sharing_test.go) and TS (tests/chain_sharing.test.ts).

use std::collections::BTreeMap;

use jed::{CreateOptions, Database, OpenOptions, Outcome, Session, SessionOptions};

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

fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
}

fn create(path: &std::path::Path) -> Session {
    remove_db(path);
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        skip_fsync: true,
        page_size: PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default())
}

fn open(path: &std::path::Path) -> Database {
    Database::open_with_options(
        path,
        OpenOptions {
            skip_fsync: true,
            ..Default::default()
        },
    )
    .unwrap()
}

fn cost(db: &mut Session, sql: &str) -> i64 {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

fn rendered(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        _ => panic!("expected a query result"),
    }
}

/// Row 1 external-plain (a 3-page chain of incompressible filler), row 2 external-compressed (a
/// 1-page chain), row 3 inline-compressed, rows 4..=12 small — at ps=256 (RECORD_MAX 114).
fn seed(db: &mut Session) {
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)", &[])
        .unwrap();
    let plain = filler_text(600);
    let extc = format!("{}{}", filler_text(200), "y".repeat(200));
    let inlc = "x".repeat(600);
    let small: Vec<String> = (4..=12)
        .map(|g| format!("({g}, 'small {g}', {g})"))
        .collect();
    // One statement, so every chain is written exactly once.
    db.query_outcome(
        &format!(
            "INSERT INTO t VALUES (1, '{plain}', 1), (2, '{extc}', 2), (3, '{inlc}', 3), {}",
            small.join(", ")
        ),
        &[],
    )
    .unwrap();
}

/// The CRC-32/IEEE page checksum over the page minus its own field at `[12, 16)` (format.md *Page
/// header*), so a corrupted payload stays checksum-valid.
fn page_crc(page: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in page[0..12].iter().chain(&page[16..]) {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Rewrite the file as its from-scratch image with every overflow page's payload set to 0xFF
/// (headers and checksums kept valid): any read of a chain's bytes now fails `XX001`.
fn corrupt_overflow_payloads(path: &std::path::Path) {
    let db = open(path).session(SessionOptions::default());
    let mut bytes = db.to_image(PAGE_SIZE, db.txid()).unwrap();
    drop(db);
    let ps = PAGE_SIZE as usize;
    let mut corrupted = 0;
    for i in 2..bytes.len() / ps {
        if bytes[i * ps] == 4 {
            for b in &mut bytes[i * ps + 16..(i + 1) * ps] {
                *b = 0xFF;
            }
            let crc = page_crc(&bytes[i * ps..(i + 1) * ps]);
            bytes[i * ps + 12..i * ps + 16].copy_from_slice(&crc.to_be_bytes());
            corrupted += 1;
        }
    }
    assert_eq!(corrupted, 4, "a 3-page and a 1-page chain");
    std::fs::write(path, &bytes).unwrap();
}

/// Every overflow page in the file, by page index.
fn overflow_pages(path: &std::path::Path) -> BTreeMap<usize, Vec<u8>> {
    let bytes = std::fs::read(path).unwrap();
    let ps = PAGE_SIZE as usize;
    (2..bytes.len() / ps)
        .filter(|&i| bytes[i * ps] == 4)
        .map(|i| (i, bytes[i * ps..(i + 1) * ps].to_vec()))
        .collect()
}

/// Neither an UPDATE that does not assign the spilled column nor a commit that rewrites the leaf
/// around it reads the chain: with every chain payload corrupt, both succeed, and the corruption
/// is still there afterwards (the chain was kept, not rewritten).
#[test]
fn rewrites_never_read_a_kept_chain() {
    jed::tooling::VERIFY_LIVE_PAGES.store(true, std::sync::atomic::Ordering::Relaxed);
    let path = tmp("jed_chain_share_corrupt.jed");
    {
        let mut db = create(&path);
        seed(&mut db);
    }
    corrupt_overflow_payloads(&path);
    {
        let mut db = open(&path).session(SessionOptions::default());
        db.query_outcome("UPDATE t SET n = n + 100", &[]).unwrap();
        db.query_outcome("INSERT INTO t VALUES (13, 'new', 13)", &[])
            .unwrap();
        db.query_outcome("UPDATE t SET id = 0 WHERE id = 2", &[])
            .unwrap();
        assert_eq!(
            rendered(&mut db, "SELECT sum(n) FROM t"),
            vec![vec!["1291".to_string()]]
        );
    }
    let mut db = open(&path).session(SessionOptions::default());
    for id in [0, 1] {
        let err = db
            .query_outcome(&format!("SELECT body FROM t WHERE id = {id}"), &[])
            .expect_err("the kept chain is still the corrupt one");
        assert_eq!(err.code(), "XX001", "id {id}");
    }
    assert_eq!(
        rendered(&mut db, "SELECT body FROM t WHERE id = 3"),
        vec![vec!["x".repeat(600)]]
    );
    drop(db);
    remove_db(&path);
}

/// A rewrite keeps a chain even when the statement reads the column (in its WHERE), as long as it
/// does not assign it: no overflow page is written or changed, and the values, a compaction of the
/// result, and the costs all come back exactly.
#[test]
fn read_but_unassigned_values_keep_their_chains() {
    jed::tooling::VERIFY_LIVE_PAGES.store(true, std::sync::atomic::Ordering::Relaxed);
    let path = tmp("jed_chain_share_read.jed");
    {
        let mut db = create(&path);
        seed(&mut db);
    }
    let before = overflow_pages(&path);
    assert_eq!(before.len(), 4);
    let want = {
        let mut db = open(&path).session(SessionOptions::default());
        db.query_outcome("UPDATE t SET n = -n WHERE length(body) > 3", &[])
            .unwrap();
        db.query_outcome("UPDATE t SET id = id + 100 WHERE id <= 2", &[])
            .unwrap();
        rendered(&mut db, "SELECT id, body, n FROM t ORDER BY id")
    };
    assert_eq!(overflow_pages(&path), before, "no chain page written");

    let db = open(&path);
    let mut s = db.session(SessionOptions::default());
    assert_eq!(
        rendered(&mut s, "SELECT id, body, n FROM t ORDER BY id"),
        want
    );
    let costs: Vec<i64> = ["SELECT * FROM t", "SELECT body FROM t WHERE id = 101"]
        .iter()
        .map(|q| cost(&mut s, q))
        .collect();
    drop(s);
    db.compact("main").unwrap();
    let mut s = db.session(SessionOptions::default());
    assert_eq!(
        rendered(&mut s, "SELECT id, body, n FROM t ORDER BY id"),
        want
    );
    let after: Vec<i64> = ["SELECT * FROM t", "SELECT body FROM t WHERE id = 101"]
        .iter()
        .map(|q| cost(&mut s, q))
        .collect();
    assert_eq!(costs, after, "compaction keeps every stored form");
    drop(s);
    drop(db);
    remove_db(&path);
}

/// Cost stays logical (cost.md §3): the same statements charge the same whether the rows were
/// read back from disk (deferred values, kept chains) or are still resident in memory (planned
/// from their values).
#[test]
fn rewrite_costs_do_not_depend_on_residency() {
    let path = tmp("jed_chain_share_cost.jed");
    let mut mem = Database::create(CreateOptions {
        page_size: PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    seed(&mut mem);
    {
        let mut db = create(&path);
        seed(&mut db);
    }
    let mut paged = open(&path).session(SessionOptions::default());
    for sql in [
        "UPDATE t SET n = n + 1",
        "UPDATE t SET n = 7 WHERE body > 'a'",
        "UPDATE t SET id = id + 50 WHERE id IN (1, 2, 3)",
        "UPDATE t SET body = body || 'z' WHERE id = 51",
        "UPDATE t SET body = 'short' WHERE id = 52",
        "INSERT INTO t VALUES (14, 'more', 14)",
        "SELECT * FROM t",
        "SELECT body FROM t WHERE id = 53",
    ] {
        assert_eq!(cost(&mut mem, sql), cost(&mut paged, sql), "{sql}");
    }
    assert_eq!(
        rendered(&mut mem, "SELECT * FROM t ORDER BY id"),
        rendered(&mut paged, "SELECT * FROM t ORDER BY id")
    );
    drop(paged);
    remove_db(&path);
}

/// The statements of [`kept_chains_stay_exclusive_across_savepoints_and_rekeys`], in three
/// sessions over the file (each phase starts from a fresh open, so its rows are read from disk).
const OWNERSHIP_PHASES: [&[&str]; 3] = [
    &[
        "BEGIN",
        "SAVEPOINT a",
        "UPDATE t SET n = n + 1000",
        "UPDATE t SET id = id + 100 WHERE id <= 3",
        "ROLLBACK TO SAVEPOINT a",
        "UPDATE t SET n = n + 1 WHERE id <= 3",
        "SAVEPOINT b",
        "UPDATE t SET id = id + 200 WHERE id = 1",
        "RELEASE SAVEPOINT b",
        "SAVEPOINT c",
        "UPDATE t SET id = id + 400 WHERE id = 2",
        "ROLLBACK TO SAVEPOINT c",
        "COMMIT",
    ],
    &[
        "INSERT INTO t VALUES (2, 'ignored', 0) ON CONFLICT (id) DO UPDATE SET id = 300",
        "INSERT INTO t SELECT id + 1000, body, n FROM t WHERE id IN (201, 300)",
    ],
    &[
        "DELETE FROM t WHERE id IN (201, 300)",
        "INSERT INTO t VALUES (13, 'FILLER', 13)",
        "UPDATE t SET n = n + 1",
    ],
];

fn ownership_sql(sql: &str) -> String {
    sql.replace("FILLER", &filler_text(900))
}

/// A chain belongs to exactly one record per snapshot (large-values.md §15.3), across savepoint
/// rollbacks, primary-key re-keys (UPDATE and upsert DO UPDATE), and row copies: rows rolled back
/// to a savepoint keep their original chains, a re-keyed row takes its chain along, and a copy gets
/// its own. The live-page self-check runs at every commit; deleting the original rows frees their
/// chains for reuse, so a shared chain would surface as a wrong or corrupt copy.
#[test]
fn kept_chains_stay_exclusive_across_savepoints_and_rekeys() {
    jed::tooling::VERIFY_LIVE_PAGES.store(true, std::sync::atomic::Ordering::Relaxed);
    let path = tmp("jed_chain_share_ownership.jed");
    let mut mem = Database::create(CreateOptions {
        page_size: PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    seed(&mut mem);
    {
        let mut db = create(&path);
        seed(&mut db);
    }
    let originals = rendered(
        &mut mem,
        "SELECT body FROM t WHERE id IN (1, 2) ORDER BY id",
    );
    for phase in OWNERSHIP_PHASES {
        let mut db = open(&path).session(SessionOptions::default());
        for sql in phase {
            let sql = ownership_sql(sql);
            mem.query_outcome(&sql, &[]).unwrap();
            db.query_outcome(&sql, &[]).unwrap();
        }
        let q = "SELECT id, body, n FROM t ORDER BY id";
        assert_eq!(rendered(&mut db, q), rendered(&mut mem, q));
    }
    let mut db = open(&path).session(SessionOptions::default());
    let q = "SELECT id, body, n FROM t ORDER BY id";
    assert_eq!(rendered(&mut db, q), rendered(&mut mem, q));
    assert_eq!(
        rendered(
            &mut db,
            "SELECT body FROM t WHERE id IN (1201, 1300) ORDER BY id"
        ),
        originals,
        "the copies kept their own chains"
    );
    drop(db);
    remove_db(&path);
}
