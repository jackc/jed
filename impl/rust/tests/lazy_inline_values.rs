//! L2/L3 — defer inline values at fault (spec/design/lazy-record.md §12). On the demand-paged path
//! every *variable-length / structured* present value (text/bytea/decimal/json/jsonb/composite/
//! array/range) is loaded as a deferred `Unfetched::Inline` (a zero-copy reference into the shared
//! page block — form (a), L3) instead of being eagerly decoded; the scan layer resolves exactly the
//! query's touched columns, an untouched one is dropped still deferred. The reshape is cost-,
//! result-, and byte-neutral (§8) regardless of representation (form (a)/(b)), so a paged file and a
//! fully-resident in-memory database must observe identical rows and identical cost for every
//! query shape — that mode-identity is the leak-catcher (an unresolved deferral escapes the scan
//! layer as a loud poison panic, never silent NULL). Mirrored in Go (lazy_inline_values_test.go)
//! and TS (tests/lazy_inline_values.test.ts).

use jed::{DatabaseOptions, Engine, Outcome, execute};

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// Schema + rows exercising every deferrable type alongside a join partner and a secondary index.
/// Default page size (8192) keeps every value inline-plain, so on a paged reopen each lands as
/// `Unfetched::Inline` — the L2 case (nothing spills; that is large-values.md §14's case).
fn seed(db: &mut Engine) {
    execute(db, "CREATE TYPE addr AS (street text, zip i32)").unwrap();
    execute(
        db,
        "CREATE TABLE t (\
            id i32 PRIMARY KEY, \
            name text, \
            data bytea, \
            amount decimal(12,2), \
            doc jsonb, \
            tags i32[], \
            home addr, \
            span i32range)",
    )
    .unwrap();
    execute(db, "CREATE INDEX t_name ON t (name)").unwrap();
    execute(
        db,
        "INSERT INTO t VALUES \
            (1, 'alice',  '\\xdeadbeef', 100.50, '{\"k\": 1, \"tag\": \"x\"}', ARRAY[10, 20, 30], ROW('Main St', 90210), '[1,5)'), \
            (2, 'bob',    '\\xcafe',     2.25,   '{\"k\": 2}',                ARRAY[1, NULL, 3], ROW('Oak Ave', 12345), '[10,20]'), \
            (3, 'carol',  NULL,          NULL,   NULL,                        NULL,              ROW('Elm', NULL),      'empty'), \
            (4, 'dave',   '\\x00ff',     9999.99,'{\"k\": 4, \"nested\": {\"a\": [1,2,3]}}', '{}',  ROW(NULL, 7),         '(,9)')",
    )
    .unwrap();

    execute(
        db,
        "CREATE TABLE u (id i32 PRIMARY KEY, t_id i32, note text)",
    )
    .unwrap();
    execute(
        db,
        "INSERT INTO u VALUES (1, 1, 'first'), (2, 1, 'again'), (3, 3, 'lonely'), (4, 99, 'orphan')",
    )
    .unwrap();
}

/// Rows rendered to strings and sorted — an order-insensitive multiset compare (a query without
/// `ORDER BY` has unspecified order, CLAUDE.md §8; sorting both sides is sound for equality either
/// way).
fn rows_sorted(db: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let mut rs: Vec<Vec<String>> = match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("expected a query result for `{sql}`, got {other:?}"),
    };
    rs.sort();
    rs
}

fn cost(db: &mut Engine, sql: &str) -> i64 {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

/// The broad leak-catcher: a battery of query shapes touching the deferred columns through every
/// read path (projection, filter, sort, DISTINCT, aggregate, join, subquery, correlated, index,
/// window, CTE, container element/field access). For each, a paged reopen and an in-memory seed
/// must agree on both rows and cost.
#[test]
fn paged_inline_values_match_resident_across_query_shapes() {
    let path = tmp("jed_l2_shapes.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Engine::create(
            &path,
            DatabaseOptions {
                page_size: jed::DEFAULT_PAGE_SIZE,
            },
        )
        .unwrap();
        seed(&mut db);
        db.close().unwrap();
    }
    let mut mem = Engine::with_page_size(jed::DEFAULT_PAGE_SIZE);
    seed(&mut mem);
    let mut paged = Engine::open(&path).unwrap();

    let queries = [
        // Whole-row and per-column projection (every deferred type resolves).
        "SELECT * FROM t",
        "SELECT id FROM t",
        "SELECT name FROM t",
        "SELECT data FROM t",
        "SELECT amount FROM t",
        "SELECT doc FROM t",
        "SELECT tags FROM t",
        "SELECT home FROM t",
        "SELECT span FROM t",
        // Filters over deferred columns (the WHERE touched set).
        "SELECT id FROM t WHERE name = 'bob'",
        "SELECT id FROM t WHERE amount > 100",
        "SELECT id FROM t WHERE data = '\\xcafe'",
        "SELECT id FROM t WHERE name IS NULL",
        "SELECT id FROM t WHERE data IS NULL",
        // Container element / field / document access.
        "SELECT tags[1] FROM t",
        "SELECT (home).zip FROM t",
        "SELECT (home).street FROM t",
        "SELECT doc->>'k' FROM t",
        "SELECT id FROM t WHERE (doc->>'k') = '2'",
        "SELECT id FROM t WHERE lower(span) = 1",
        // Sort (the streaming-sort feed resolves the sort key; carried columns ride along).
        "SELECT name FROM t ORDER BY name",
        "SELECT id, name FROM t ORDER BY name DESC",
        "SELECT name, amount FROM t ORDER BY id",
        // DISTINCT and GROUP BY / aggregates over deferred columns.
        "SELECT DISTINCT name FROM t",
        "SELECT count(*), max(name), min(amount) FROM t",
        "SELECT amount, count(*) FROM t GROUP BY amount",
        "SELECT name FROM t GROUP BY name HAVING count(*) = 1",
        // Secondary-index scan that also projects the deferred indexed column.
        "SELECT name FROM t WHERE name = 'carol'",
        "SELECT id, name FROM t WHERE name > 'bob' ORDER BY name",
        // Joins across two paged tables (both sides' deferred columns).
        "SELECT t.name, u.note FROM t JOIN u ON u.t_id = t.id",
        "SELECT t.name FROM t JOIN u ON u.t_id = t.id WHERE u.note = 'first'",
        // Subquery + correlated subquery.
        "SELECT name FROM t WHERE id IN (SELECT t_id FROM u WHERE note = 'lonely')",
        "SELECT name FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
        "SELECT name FROM t WHERE id = (SELECT min(t_id) FROM u)",
        // Window function carrying a deferred column through the buffered operator.
        "SELECT name, row_number() OVER (ORDER BY id) FROM t",
        "SELECT name, count(*) OVER () FROM t",
        // CTE (read) carrying deferred columns.
        "WITH c AS (SELECT id, name FROM t) SELECT name FROM c WHERE id = 1",
        "WITH c AS (SELECT name, amount FROM t WHERE amount IS NOT NULL) \
         SELECT name FROM c ORDER BY amount",
    ];

    for sql in queries {
        assert_eq!(
            rows_sorted(&mut mem, sql),
            rows_sorted(&mut paged, sql),
            "rows differ (paged vs resident): {sql}"
        );
        assert_eq!(
            cost(&mut mem, sql),
            cost(&mut paged, sql),
            "cost differs (paged vs resident): {sql}"
        );
    }

    paged.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// Mutations on a paged store with deferred inline values: an UPDATE that touches only some columns
/// must re-store the *untouched* deferred ones losslessly (the dirty leaf's other rows resolve at
/// commit; the rewritten row's remaining references resolve as part of the rewrite — large-values.md
/// §14, generalized to inline values). Applying the identical sequence to a resident database must
/// reach the identical final state.
#[test]
fn mutations_preserve_untouched_inline_values() {
    let path = tmp("jed_l2_mutations.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Engine::create(
            &path,
            DatabaseOptions {
                page_size: jed::DEFAULT_PAGE_SIZE,
            },
        )
        .unwrap();
        seed(&mut db);
        db.close().unwrap();
    }
    let mut mem = Engine::with_page_size(jed::DEFAULT_PAGE_SIZE);
    seed(&mut mem);

    let mutations = [
        // Touches only `amount`: every other deferred column on these rows must round-trip
        // untouched through the rewrite + the dirty leaf's resolve-at-commit.
        "UPDATE t SET amount = amount + 1 WHERE id = 1",
        // Touches only a fixed-width-free path on a single row, dirtying the leaf that also holds
        // rows 1/3/4 whose deferred values must resolve at commit.
        "UPDATE t SET name = 'robert' WHERE id = 2",
        // Replaces a container (array) deferred column; the row's other deferred columns (doc,
        // home, span) stay untouched and must round-trip through the rewrite.
        "UPDATE t SET tags = ARRAY[7, 8] WHERE id = 4",
        // Delete a row (filter touches only the key; no deferred read).
        "DELETE FROM t WHERE id = 3",
        // Insert a fresh row, then update an unrelated table.
        "INSERT INTO t VALUES (5, 'erin', '\\xab', 1.00, '{\"k\":5}', ARRAY[9], ROW('New', 1), '[2,3)')",
        "UPDATE u SET note = 'edited' WHERE t_id = 1",
    ];

    // Apply to the resident baseline directly.
    for m in mutations {
        execute(&mut mem, m).unwrap();
    }
    // Apply to the paged store across a reopen so each mutation runs against lazily-faulted rows.
    {
        let mut paged = Engine::open(&path).unwrap();
        for m in mutations {
            execute(&mut paged, m).unwrap();
        }
        paged.close().unwrap();
    }

    // A fresh paged reopen must read back exactly the resident final state.
    let mut paged = Engine::open(&path).unwrap();
    for sql in [
        "SELECT * FROM t",
        "SELECT id, name, amount, doc, tags, home, span, data FROM t ORDER BY id",
        "SELECT * FROM u",
    ] {
        assert_eq!(
            rows_sorted(&mut mem, sql),
            rows_sorted(&mut paged, sql),
            "final state differs: {sql}"
        );
    }
    paged.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// The v7 per-page CRC (replicated, like lazy_large_values.rs) so a corrupted page stays
/// checksum-valid and the failure isolates to decode time, not the open-time checksum gate.
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

/// Read-on-touch for *inline* values, proved physically (lazy-record.md §8): with an inline text
/// body corrupted to non-UTF-8 on disk (but its length prefix + the page checksum kept valid), the
/// skip-walk that finds the body's span still advances correctly — so open and an untouching query
/// succeed — while touching the column runs the real decode and surfaces `XX001`. This is the
/// inline analogue of lazy_large_values.rs `chains_are_read_only_when_touched`.
#[test]
fn untouched_corrupt_inline_body_defers_its_error() {
    let path = tmp("jed_l2_corrupt.jed");
    let _ = std::fs::remove_file(&path);
    // A distinctive 32-byte marker that appears only in the corruptible text body.
    let marker = "Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq"; // 32 chars, no overlap with catalog text
    {
        let mut db = Engine::create(
            &path,
            DatabaseOptions {
                page_size: jed::DEFAULT_PAGE_SIZE,
            },
        )
        .unwrap();
        execute(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)",
        )
        .unwrap();
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES (1, '{marker}', 42), (2, 'clean', 7)"),
        )
        .unwrap();
        db.close().unwrap();
    }

    // Corrupt the first content byte of the marker text body to 0xFF (an invalid UTF-8 lead byte),
    // leaving the u16 length prefix intact so the skip-walk advances identically, then repair the
    // page CRC so the corruption is checksum-valid (isolating the failure to decode time).
    {
        let ps = jed::DEFAULT_PAGE_SIZE as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        let needle = marker.as_bytes();
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("marker text body present in the file");
        // The leaf page holding it (page 0/1 are meta; a body page is >= 2).
        let page_idx = at / ps;
        assert_eq!(bytes[page_idx * ps], 2, "marker lives in a leaf page");
        bytes[at] = 0xFF; // content byte → non-UTF-8; length prefix (just before `at`) untouched
        let crc = page_crc(&bytes[page_idx * ps..(page_idx + 1) * ps]);
        bytes[page_idx * ps + 12..page_idx * ps + 16].copy_from_slice(&crc.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
    }

    let mut db = Engine::open(&path).unwrap();
    // Open faulted the leaf (skip-walk only); untouching queries never construct the body.
    assert_eq!(rows_sorted(&mut db, "SELECT id FROM t").len(), 2);
    assert_eq!(
        rows_sorted(&mut db, "SELECT id, n FROM t WHERE n = 42"),
        vec![vec!["1".to_string(), "42".to_string()]]
    );
    // The clean row's body resolves fine.
    assert_eq!(
        rows_sorted(&mut db, "SELECT body FROM t WHERE id = 2"),
        vec![vec!["clean".to_string()]]
    );
    // Touching the corrupted body runs the real decode: XX001.
    let err = execute(&mut db, "SELECT body FROM t WHERE id = 1")
        .expect_err("a corrupted inline body must fail when touched");
    assert_eq!(err.code(), "XX001");
    // It also surfaces through a whole-row projection that includes the body.
    let err = execute(&mut db, "SELECT * FROM t ORDER BY id")
        .expect_err("touching the body through SELECT * must fail");
    assert_eq!(err.code(), "XX001");

    db.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// An untouched deferred column riding a *spilling* sort (spill.md §4): the streaming sort feed
/// pushes the full base row into the sorter with only the touched columns resolved, so an
/// unreferenced deferred column stays `Unfetched::Inline` and must round-trip opaquely through the
/// spill run file (the spill codec's inline pass-through). Under a tiny `work_mem` the sort spills
/// many runs; the result must still equal the in-memory sort, which proves the deferred value
/// survives a spill+merge cycle untouched (and never resolves — it is dropped at projection).
#[test]
fn untouched_deferred_column_rides_a_spilling_sort() {
    let path = tmp("jed_l2_spill.jed");
    let _ = std::fs::remove_file(&path);
    let mut mem = Engine::new();
    {
        let mut db = Engine::create(&path, DatabaseOptions::default()).unwrap();
        for db in [&mut mem as &mut Engine, &mut db] {
            execute(
                db,
                "CREATE TABLE t (id i32 PRIMARY KEY, k i32, label text, doc jsonb)",
            )
            .unwrap();
        }
        // 200 rows: a scrambled sort key `k`, plus deferred `label`/`doc` columns the queries below
        // never reference (so they ride the sorter unresolved on the paged path).
        for id in 0..200i64 {
            let k = (id * 48271) % 100;
            let row = format!(
                "INSERT INTO t VALUES ({id}, {k}, 'label-{id}-xxxxxxxxxx', '{{\"id\": {id}}}')"
            );
            execute(&mut mem, &row).unwrap();
            execute(&mut db, &row).unwrap();
        }
        db.close().unwrap();
    }

    let mut paged = Engine::open(&path).unwrap();
    paged.set_work_mem(128); // ~2-3 rows per run → dozens of spilled runs + a deep k-way merge

    for sql in [
        // The deferred `label`/`doc` are never referenced — they ride the sorter unresolved.
        "SELECT id FROM t ORDER BY k, id",
        "SELECT id, k FROM t ORDER BY k DESC, id DESC",
        "SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
        // A referenced deferred column (label) IS resolved before the push; both paths agree.
        "SELECT label FROM t ORDER BY k, id LIMIT 5",
    ] {
        assert_eq!(
            rows_sorted(&mut mem, sql),
            rows_sorted(&mut paged, sql),
            "spilling sort with deferred carried columns differs: {sql}"
        );
        assert_eq!(
            cost(&mut mem, sql),
            cost(&mut paged, sql),
            "cost differs: {sql}"
        );
    }

    paged.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
