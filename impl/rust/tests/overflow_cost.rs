//! Overflow-chain `page_read` accrual (spec/design/large-values.md §8.1/§12; cost.md §3
//! "page_read"). A scan's up-front page_read block counts the B-tree nodes the bound intersects
//! PLUS one per overflow chain page of every record the bound admits. The conformance corpus
//! cannot exercise this (its tables use the 8 KiB default page, where nothing spills), so these
//! tests pin the accrual at page_size 256 by comparing a spilling table against a control table
//! of identical shape (same schema, same keys, same row count, one leaf each) whose values stay
//! inline — the cost delta is exactly the chain pages. Mirrored in Go
//! (overflow_cost_test.go) and TS (tests/overflow_cost.test.ts).

use jed::{Database, Outcome, execute};

// page_size 256 ⇒ cap = 244, RECORD_MAX = 116. A 600-byte text payload spills into
// ceil(600/244) = 3 overflow pages; a 300-byte bytea into ceil(300/244) = 2. Payloads are
// incompressible filler so Slice B's compress pass rejects them (store-smaller) and they
// genuinely spill plain — compression's own costs are pinned in compressed_cost.rs.
const PAGE_SIZE: u32 = 256;
const TEXT_CHAIN_PAGES: i64 = 3;
const BYTEA_CHAIN_PAGES: i64 = 2;

/// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32("JEDB") mapped to a
/// 64-char alphabet (text) or hex bytes (bytea literals). High-entropy, so Slice B's compress
/// pass never wins store-smaller and the value genuinely spills PLAIN — keeping these tests
/// about overflow chains. Mirrors verify.rb's filler_text/filler_bytes.
const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn filler_step(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^ (x << 5)
}

fn filler_text(n: usize) -> String {
    let mut x: u32 = 0x4A45_4442;
    (0..n)
        .map(|_| {
            x = filler_step(x);
            ALPHA64[(x % 64) as usize] as char
        })
        .collect()
}

fn filler_bytes_hex(n: usize) -> String {
    let mut x: u32 = 0x4A45_4442;
    let mut out = String::with_capacity(n * 2);
    for _ in 0..n {
        x = filler_step(x);
        out.push_str(&format!("{:02x}", x % 256));
    }
    out
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    match execute(db, sql).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost } => cost,
    }
}

/// Two tables of identical shape: `spill` row 1 carries a 600-char text (3-page chain),
/// `control` keeps every value inline. Row 2 is inline in both.
fn two_tables() -> Database {
    let mut db = Database::with_page_size(PAGE_SIZE);
    let big = filler_text(600);
    execute(
        &mut db,
        "CREATE TABLE spill (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(
        &mut db,
        &format!("INSERT INTO spill VALUES (1, '{big}'), (2, 'small')"),
    )
    .unwrap();
    execute(
        &mut db,
        "CREATE TABLE control (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO control VALUES (1, 'tiny'), (2, 'small')",
    )
    .unwrap();
    db
}

#[test]
fn full_scan_charges_chain_pages() {
    let mut db = two_tables();
    let spill = cost(&mut db, "SELECT * FROM spill");
    let control = cost(&mut db, "SELECT * FROM control");
    // Identical plans, rows, and tree shape — the only difference is the 3-page chain.
    assert_eq!(spill, control + TEXT_CHAIN_PAGES);
}

#[test]
fn bounded_scan_charges_only_admitted_chains() {
    let mut db = two_tables();
    // The point lookup that admits the spilled record pays its chain ...
    let spill_hit = cost(&mut db, "SELECT * FROM spill WHERE id = 1");
    let control_hit = cost(&mut db, "SELECT * FROM control WHERE id = 1");
    assert_eq!(spill_hit, control_hit + TEXT_CHAIN_PAGES);
    // ... the one that admits only the inline record pays nothing extra.
    let spill_inline = cost(&mut db, "SELECT * FROM spill WHERE id = 2");
    let control_inline = cost(&mut db, "SELECT * FROM control WHERE id = 2");
    assert_eq!(spill_inline, control_inline);
}

#[test]
fn limit_does_not_lower_the_block() {
    // The spilled record is row 2, so LIMIT 1 emits only the inline row 1 — yet the page_read
    // block (which never short-circuits — cost.md §3 "LIMIT short-circuit") still counts the
    // bound's chain pages.
    let mut db = Database::with_page_size(PAGE_SIZE);
    let big = filler_text(600);
    execute(
        &mut db,
        "CREATE TABLE spill (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(
        &mut db,
        &format!("INSERT INTO spill VALUES (1, 'small'), (2, '{big}')"),
    )
    .unwrap();
    execute(
        &mut db,
        "CREATE TABLE control (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO control VALUES (1, 'small'), (2, 'tiny')",
    )
    .unwrap();
    let spill = cost(&mut db, "SELECT * FROM spill LIMIT 1");
    let control = cost(&mut db, "SELECT * FROM control LIMIT 1");
    assert_eq!(spill, control + TEXT_CHAIN_PAGES);
}

#[test]
fn mutation_scans_charge_chain_pages() {
    let mut db = two_tables();
    let spill = cost(&mut db, "DELETE FROM spill");
    let control = cost(&mut db, "DELETE FROM control");
    assert_eq!(spill, control + TEXT_CHAIN_PAGES);
}

#[test]
fn multiple_chains_sum() {
    // One record with two externalized values charges the sum of both chains: 3 + 2 = 5.
    let mut db = Database::with_page_size(PAGE_SIZE);
    let big_text = filler_text(600);
    let big_hex = filler_bytes_hex(300);
    execute(
        &mut db,
        "CREATE TABLE spill (id int32 PRIMARY KEY, body text, blob bytea)",
    )
    .unwrap();
    execute(
        &mut db,
        &format!("INSERT INTO spill VALUES (1, '{big_text}', '\\x{big_hex}')"),
    )
    .unwrap();
    execute(
        &mut db,
        "CREATE TABLE control (id int32 PRIMARY KEY, body text, blob bytea)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO control VALUES (1, 'tiny', '\\xcafe')").unwrap();
    let spill = cost(&mut db, "SELECT * FROM spill");
    let control = cost(&mut db, "SELECT * FROM control");
    assert_eq!(spill, control + TEXT_CHAIN_PAGES + BYTEA_CHAIN_PAGES);
}
