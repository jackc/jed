//! Per-page checksum (`format_version` 7): every body page — catalog, B-tree leaf, B-tree
//! interior, and overflow — carries a CRC-32/IEEE over its own bytes (spec/fileformat/format.md
//! *Page header*; spec/design/storage.md §6). This pins the durability guarantee that distinguishes
//! reliability item #3 from the meta-only checksum: a silently corrupted **live** page is detected
//! as `XX001` the instant it is read — at open for a catalog/interior/overflow page (the loader and
//! the free-list reachability walk), at fault for a leaf — and is **never** served as wrong rows. A
//! corrupted **dead** page (free space an earlier incremental commit abandoned, P6.2) is harmless:
//! it is not reachable from the committed snapshot, so the file still reads back exactly. The
//! invariant the loop asserts is therefore the strong one: corrupting *any* body page yields either
//! `XX001` or the byte-identical correct result — corruption is **caught or inert, never silent**.
//! Mirrored in Go (checksum_test.go) and TS (tests/checksum.test.ts).

use jed::{Database, DatabaseOptions, Outcome, Result, execute};

const PAGE_SIZE: u32 = 256;

/// Incompressible filler (spec/fileformat/format.md "Fixtures") so row 1's body spills to an
/// **overflow** chain (`page_type 4`) rather than compressing back inline.
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

/// The full scan result as rendered strings, or the error if any page read failed.
fn scan(path: &std::path::Path) -> Result<Vec<Vec<String>>> {
    let mut db = Database::open(path)?;
    let out = match execute(&mut db, "SELECT id, body FROM t ORDER BY id")? {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        _ => panic!("expected a query result"),
    };
    db.close()?;
    Ok(out)
}

/// Seed a file whose tree spans every body-page kind at `page_size = 256`: a multi-leaf B-tree
/// (interior root) of ~30 rows, with row 1 a 600-char incompressible body that spills out-of-line.
fn seed(path: &std::path::Path) {
    let mut db = Database::create(
        path,
        DatabaseOptions {
            page_size: PAGE_SIZE,
        },
    )
    .unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)").unwrap();
    let big = filler_text(600);
    let mut sql = format!("INSERT INTO t VALUES (1, '{big}')");
    for id in 2..=30 {
        sql.push_str(&format!(", ({id}, 'row{id}')"));
    }
    execute(&mut db, &sql).unwrap();
    db.close().unwrap();
}

#[test]
fn corrupting_any_body_page_is_caught_or_inert_never_silent() {
    let path = tmp("jed_checksum_seed.jed");
    let cpath = tmp("jed_checksum_corrupt.jed");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&cpath);
    seed(&path);

    // The correct result over the intact file (proves the seed is readable, and is the oracle the
    // dead-page case must reproduce).
    let want = scan(&path).expect("the intact file scans cleanly");
    assert_eq!(want.len(), 30, "30 rows seeded");

    let clean = std::fs::read(&path).unwrap();
    let ps = PAGE_SIZE as usize;
    let pages = clean.len() / ps;
    assert!(
        pages >= 6,
        "the seed should span several pages, got {pages}"
    );

    // Corrupt one payload byte of each body page in turn (pages 0/1 are the meta slots, checksummed
    // separately — incremental.rs / reclamation.rs). The flip is NOT CRC-repaired, so a live page
    // fails its per-page checksum; a dead page is never read and the snapshot is unaffected.
    let mut detected = 0;
    for i in 2..pages {
        let mut bytes = clean.clone();
        bytes[i * ps + 16] ^= 0xFF; // first payload byte (offset PAGE_HEADER = 16)
        std::fs::write(&cpath, &bytes).unwrap();
        match scan(&cpath) {
            Err(e) => {
                assert_eq!(
                    e.code(),
                    "XX001",
                    "corrupting live page {i} must be data_corrupted"
                );
                detected += 1;
            }
            Ok(rows) => assert_eq!(
                rows, want,
                "corrupting dead page {i} must not change results"
            ),
        }
    }

    // The live pages — catalog, the interior root, several leaves, and the overflow chain — are all
    // protected; only a handful of pages (if any) are dead space. A floor of 4 guarantees detection
    // fired across page kinds, not just one.
    assert!(
        detected >= 4,
        "expected live pages across kinds to be detected, got {detected}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&cpath);
}
