//! Compression cost accrual (spec/design/cost.md §3 "the compression units";
//! spec/design/large-values.md §13). `value_decompress` joins a scan's up-front block —
//! `ceil(raw/C)` slabs per compressed stored value the bound admits — and `value_compress`
//! meters every disposition-plan compress ATTEMPT (adopted or rejected) at the INSERT/UPDATE
//! write site. The conformance corpus cannot exercise this (its 8 KiB pages never trigger the
//! plan), so these tests pin the accrual at page_size 256 (cap C = 244, RECORD_MAX = 116) with
//! spill-vs-control table deltas. Mirrored in Go (compressed_cost_test.go) and TS
//! (tests/compressed_cost.test.ts).

use jed::{Database, Outcome, execute};

const PAGE_SIZE: u32 = 256;
// A 600-byte payload = ceil(600/244) = 3 slabs (compress at write, decompress at scan); a
// 400-byte payload = 2 slabs.
const SLABS_600: i64 = 3;
const SLABS_400: i64 = 2;

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

fn cost(db: &mut Database, sql: &str) -> i64 {
    match execute(db, sql).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost } => cost,
    }
}

/// `comp` row 1 carries a 600-char "x" run → 0x03 inline-compressed (LZ4 shrinks it far under
/// RECORD_MAX, so no chain); `control` is the same shape fully inline-plain. Row 2 is inline in
/// both. Same tree shape (one leaf each), so cost deltas isolate the compression units.
fn two_tables() -> Database {
    let mut db = Database::with_page_size(PAGE_SIZE);
    let run600 = "x".repeat(600);
    execute(
        &mut db,
        "CREATE TABLE comp (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(
        &mut db,
        &format!("INSERT INTO comp VALUES (1, '{run600}'), (2, 'small')"),
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
fn scan_charges_decompress_slabs_for_an_inline_compressed_value() {
    let mut db = two_tables();
    let comp = cost(&mut db, "SELECT * FROM comp");
    let control = cost(&mut db, "SELECT * FROM control");
    // Identical plans, rows, and tree shape — the only difference is the ceil(600/244) = 3
    // value_decompress slabs (no chain: the compressed form fits inline, so page_read is equal).
    assert_eq!(comp, control + SLABS_600);
}

#[test]
fn external_compressed_charges_chain_pages_plus_decompress_slabs() {
    // A 400-char half-filler/half-run text compresses to ~212 B — smaller than plain but still
    // over RECORD_MAX → 0x04 external-compressed: ceil(212/244) = 1 chain page_read PLUS
    // ceil(400/244) = 2 value_decompress slabs.
    let mut db = Database::with_page_size(PAGE_SIZE);
    let mix = format!("{}{}", filler_text(200), "y".repeat(200));
    execute(
        &mut db,
        "CREATE TABLE comp (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(&mut db, &format!("INSERT INTO comp VALUES (1, '{mix}')")).unwrap();
    execute(
        &mut db,
        "CREATE TABLE control (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO control VALUES (1, 'tiny')").unwrap();
    let comp = cost(&mut db, "SELECT * FROM comp");
    let control = cost(&mut db, "SELECT * FROM control");
    assert_eq!(comp, control + 1 + SLABS_400);
}

#[test]
fn bounded_scan_charges_only_admitted_values_and_limit_does_not_lower() {
    let mut db = two_tables();
    // The point lookup that admits the compressed record pays its slabs ...
    let comp_hit = cost(&mut db, "SELECT * FROM comp WHERE id = 1");
    let control_hit = cost(&mut db, "SELECT * FROM control WHERE id = 1");
    assert_eq!(comp_hit, control_hit + SLABS_600);
    // ... the one that admits only the inline record pays nothing extra ...
    let comp_miss = cost(&mut db, "SELECT * FROM comp WHERE id = 2");
    let control_miss = cost(&mut db, "SELECT * FROM control WHERE id = 2");
    assert_eq!(comp_miss, control_miss);
    // ... and LIMIT does not lower the up-front block (cost.md §3 "LIMIT short-circuit"):
    // row 1 IS the compressed row, but even emitting only it pays the full bound's slabs.
    let comp_lim = cost(&mut db, "SELECT * FROM comp LIMIT 1");
    let control_lim = cost(&mut db, "SELECT * FROM control LIMIT 1");
    assert_eq!(comp_lim, control_lim + SLABS_600);
}

#[test]
fn insert_meters_compress_attempts_adopted_or_rejected() {
    let mut db = Database::with_page_size(PAGE_SIZE);
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)").unwrap();
    // A fully-inline row attempts nothing: INSERT stays zero-cost.
    assert_eq!(cost(&mut db, "INSERT INTO t VALUES (1, 'small')"), 0);
    // An adopted compression (the "x" run) costs its ceil(600/244) = 3 attempt slabs ...
    let run600 = "x".repeat(600);
    assert_eq!(
        cost(&mut db, &format!("INSERT INTO t VALUES (2, '{run600}')")),
        SLABS_600
    );
    // ... and a REJECTED attempt (incompressible filler → external-plain) costs the same
    // slabs — the encoder ran either way (cost.md §3).
    let fill600 = filler_text(600);
    assert_eq!(
        cost(&mut db, &format!("INSERT INTO t VALUES (3, '{fill600}')")),
        SLABS_600
    );
}

#[test]
fn update_meters_compress_attempts_per_rewritten_row() {
    let mut db = two_tables();
    // Rewriting the compressed row re-runs its disposition plan: one 600-slab attempt for the
    // new value. The delta against the same UPDATE on the control table isolates it from the
    // scan block (which itself includes the OLD row's 3 decompress slabs on both... only the
    // comp table — so compare against the comp table's own no-op-shape control: an UPDATE that
    // assigns the small row instead).
    let run600 = "x".repeat(600);
    let big_update = cost(
        &mut db,
        &format!("UPDATE comp SET body = '{run600}' WHERE id = 1"),
    );
    let small_update = cost(&mut db, "UPDATE comp SET body = 'small' WHERE id = 1");
    // Same bounded scan (id = 1 admits the same record both times: by then row 1 holds the
    // run600 value again after the first UPDATE — both scans pay its 3 decompress slabs), same
    // row reads and evals; the only delta is the new value's compress attempt: 3 slabs vs 0.
    assert_eq!(big_update, small_update + SLABS_600);
}

#[test]
fn decimal_payloads_compress_too() {
    // A long-coefficient decimal's body (flags|scale|ndigits|groups) is a spillable payload
    // like text/bytea (large-values.md §12/§13). 801 digits (an "12"-run plus ".5" so the
    // literal types as numeric) → 201 base-10⁴ groups → a 407-byte payload: over RECORD_MAX,
    // compressible (repeating groups), and ceil(407/244) = 2 slabs both ways.
    let mut db = Database::with_page_size(PAGE_SIZE);
    let digits = format!("{}.5", "12".repeat(400));
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, d numeric)").unwrap();
    let ins = cost(&mut db, &format!("INSERT INTO t VALUES (1, {digits})"));
    assert_eq!(ins, 2, "the compress attempt is metered");
    execute(
        &mut db,
        "CREATE TABLE control (id int32 PRIMARY KEY, d numeric)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO control VALUES (1, 7)").unwrap();
    let comp = cost(&mut db, "SELECT * FROM t");
    let control = cost(&mut db, "SELECT * FROM control");
    assert_eq!(comp, control + 2, "the decompress slabs are metered");
}

#[test]
fn untouched_compressed_columns_charge_no_slabs() {
    // The touched set (cost.md §3 "The touched set"): a query that never references the
    // compressed column pays no decompress slabs; an aggregate's ARGUMENT is a touch.
    let mut db = two_tables();
    let comp_id = cost(&mut db, "SELECT id FROM comp");
    let control_id = cost(&mut db, "SELECT id FROM control");
    assert_eq!(comp_id, control_id);
    let comp_cnt = cost(&mut db, "SELECT count(*) FROM comp");
    let control_cnt = cost(&mut db, "SELECT count(*) FROM control");
    assert_eq!(comp_cnt, control_cnt);
    let comp_min = cost(&mut db, "SELECT min(body) FROM comp");
    let control_min = cost(&mut db, "SELECT min(body) FROM control");
    assert_eq!(comp_min, control_min + SLABS_600);
}

#[test]
fn correlated_outer_reference_is_a_touch() {
    // A nested subquery's outer reference back into the scanned relation counts as a touch
    // (collected depth-aware — cost.md §3). `probe` holds the one value that matches both
    // tables' row 2, so the two queries emit identical row counts and differ only in the
    // outer table's storage — isolating the SLABS_600 the outer reference charges.
    let mut db = two_tables();
    execute(
        &mut db,
        "CREATE TABLE probe (id int32 PRIMARY KEY, body text)",
    )
    .unwrap();
    execute(&mut db, "INSERT INTO probe VALUES (1, 'small')").unwrap();
    let comp_q = cost(
        &mut db,
        "SELECT id FROM comp WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = comp.body)",
    );
    let control_q = cost(
        &mut db,
        "SELECT id FROM control WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = control.body)",
    );
    assert_eq!(comp_q, control_q + SLABS_600);
}
