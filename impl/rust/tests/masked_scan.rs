//! A1 — touched-column scan wiring (packed-leaf.md §4/§11; the PAX read-path dividend). A file-backed
//! SELECT feed reconstructs only the query's touched columns (`rel_masks`), leaving untouched columns
//! `Null` on the Packed leaf, instead of decoding the whole row. This is byte/result/cost-neutral IFF
//! the mask is a complete superset of every column any consumer reads — an invariant already
//! load-bearing for deferred VARIABLE-LENGTH values (an untouched unfetched value poisons if read,
//! tests/lazy_inline_values.rs) but NEWLY load-bearing for FIXED-WIDTH columns (previously always
//! decoded, so a mask gap was harmless). This battery actively exercises that: a WIDE ALL-FIXED-WIDTH
//! table and a spread of query shapes each touching a different column subset, where a paged reopen
//! (masked reconstruction) and a fully-resident in-memory database (whole rows) must agree on both rows
//! and cost. A mask gap surfaces as a divergence here, never a silent wrong answer. Mirrors Go
//! (masked_scan_test.go) and TS (tests/masked_scan.test.ts).
//!
//! A bare-column PROJECTION additionally takes the A2/A3 **columnar** gather on the paged (file-backed)
//! database (`project_columnar` → `Emitter::Columnar`): only the touched columns are gathered into dense
//! lanes — never a full-width row — and a `WHERE` predicate (A3) is applied over the lanes into a
//! selection vector. So the projection cases below compare a paged-columnar path against the resident
//! row path (rows AND cost). A single-base-table SUM/COUNT/MIN/MAX/AVG (whole-table or grouped by a
//! single integer column) likewise takes the vectorized aggregate executor: on the paged database it
//! gathers its touched columns columnar (`agg_columnar` → `fold_agg_whole`/`group_by_int_key`), on the
//! in-memory database it folds int64-bucketed over the materialized rows — both byte-identical to the
//! scalar group machinery on rows AND cost (the conformance corpus proves the in-memory row path; this
//! battery proves the paged columnar path against the resident oracle).

use jed::{Database, DatabaseOptions, Outcome, Session, SessionOptions};

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// A wide all-fixed-width table (i16/i32/i64, several nullable) plus a secondary index and a join
/// partner. Every column is fixed-width, so on a paged reopen the leaf is Packed with no deferred
/// values — the case `row_at_masked` skips whole-column decodes that `row_at` would have done.
fn seed(db: &mut Session) {
    db.execute(
        "CREATE TABLE w (\
            id i32 PRIMARY KEY, c0 i16, c1 i32, c2 i64, c3 i32, c4 i16, c5 i64, c6 i32, c7 i32)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO w VALUES \
            (1, 10, 100, 1000, 7, 3, 500, 42, 9), \
            (2, 20, 100, 2000, 7, NULL, 600, 43, 8), \
            (3, 10, 300, 3000, 8, 5, NULL, 44, 7), \
            (4, 20, 100, 4000, 8, 6, 800, NULL, 6), \
            (5, 10, 500, 5000, 9, NULL, 900, 46, 5)",
        &[],
    )
    .unwrap();
    db.execute("CREATE INDEX w_c3 ON w (c3)", &[]).unwrap();
    db.execute("CREATE TABLE w2 (id i32 PRIMARY KEY, k i32, note i32)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO w2 VALUES (1, 7, 71), (2, 8, 82), (3, 7, 73), (5, 9, 95)",
        &[],
    )
    .unwrap();
}

/// Rows rendered to strings and sorted — an order-insensitive multiset compare (a query without
/// `ORDER BY` has unspecified order, CLAUDE.md §8; sorting both sides is sound for equality either way).
fn rows_sorted(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let mut rs: Vec<Vec<String>> = match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
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

fn cost(db: &mut Session, sql: &str) -> i64 {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

/// The streaming (`query()`) result, fully drained + rendered + sorted, plus the final cost — exercises
/// the LAZY drive (`BufferedScan` → `BufState::Columnar`) of the columnar projection path, which the
/// eager `execute()` helpers above do not reach. Fully drained, it must observe the same rows AND total
/// cost as the eager path (streaming.md §6).
fn streamed_sorted(db: &mut Session, sql: &str) -> (Vec<Vec<String>>, i64) {
    let mut rows = db
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let mut out: Vec<Vec<String>> = Vec::new();
    for r in &mut rows {
        out.push(r.iter().map(|v| v.render()).collect());
    }
    rows.error().unwrap();
    let c = rows.cost();
    out.sort();
    (out, c)
}

/// A paged reopen (masked reconstruction) and an in-memory seed (whole rows) must agree on rows AND
/// cost for every query shape — each touching a different column subset. If masked reconstruction
/// wrongly NULLed a needed column, the paged rows/cost would diverge from the resident whole-row path.
#[test]
fn paged_masked_scan_matches_resident_across_query_shapes() {
    let path = tmp("jed_masked_wide_fixed.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: jed::DEFAULT_PAGE_SIZE,
            },
        )
        .unwrap()
        .session(SessionOptions::default());
        seed(&mut db);
        drop(db);
    }
    let mut mem = Database::new_in_memory_with_page_size(jed::DEFAULT_PAGE_SIZE)
        .session(SessionOptions::default());
    seed(&mut mem);
    let mut paged = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());

    let queries = [
        // Whole-row and single/multi-column projections.
        "SELECT * FROM w",
        "SELECT c0 FROM w",
        "SELECT c3, c7 FROM w",
        "SELECT id, c5 FROM w",
        // WHERE on one column, project another (touched set spans filter + projection).
        "SELECT c1 FROM w WHERE c0 > 15",
        "SELECT id FROM w WHERE c7 < 8",
        "SELECT c6 FROM w WHERE c4 IS NULL",
        "SELECT c2 FROM w WHERE c5 IS NOT NULL",
        "SELECT c1 FROM w WHERE c0 > 5 AND c7 < 9", // AND predicate
        "SELECT c0, c6 FROM w WHERE c7 = 5 OR c7 = 8", // OR predicate, multi-column projection
        "SELECT c0 FROM w WHERE c0 > 1000",         // zero survivors
        "SELECT id FROM w WHERE id > 0",            // every row survives
        // Aggregates touching one operand column — the vectorized aggregate executor: columnar gather on
        // the paged database, int64-bucketed row fold on the resident one; both must agree on rows + cost.
        "SELECT count(*) FROM w",
        "SELECT sum(c2) FROM w",
        "SELECT sum(c1) FROM w",
        "SELECT sum(c3), count(c6) FROM w",
        "SELECT count(c4) FROM w", // COUNT over a nullable operand
        "SELECT min(c5), max(c6) FROM w",
        "SELECT sum(c0) FROM w WHERE c1 = 100", // filtered agg
        "SELECT count(*) FROM w WHERE c7 < 8",  // filtered COUNT(*)
        "SELECT min(c5), max(c6) FROM w WHERE c4 IS NOT NULL", // filtered MIN/MAX over a nullable operand
        "SELECT sum(c1) FROM w WHERE c0 > 1000",               // filter admits no rows
        // Single-integer-key GROUP BY (touched: the key + the operand).
        "SELECT c0, sum(c2) FROM w GROUP BY c0",
        "SELECT c0, sum(c1), count(c4) FROM w GROUP BY c0", // grouped multi-spec, nullable operand
        "SELECT c3, count(*) FROM w GROUP BY c3",
        "SELECT c0, sum(c1) FROM w WHERE c7 > 5 GROUP BY c0", // filtered grouped
        // ORDER BY satisfied by the PK scan (top-N streaming) and by a sort (non-PK).
        "SELECT c1 FROM w ORDER BY id",
        "SELECT c1 FROM w ORDER BY id LIMIT 3",
        "SELECT c6 FROM w ORDER BY c6 DESC",
        "SELECT id, c0 FROM w ORDER BY c0, id",
        // DISTINCT.
        "SELECT DISTINCT c0 FROM w",
        "SELECT DISTINCT c3, c0 FROM w",
        // PK point + range bounds (the range_scan_with_units_masked feed).
        "SELECT c4 FROM w WHERE id = 2",
        "SELECT c2, c6 FROM w WHERE id >= 3",
        // Secondary-index bound (index_bound_rows — whole-row, must still agree).
        "SELECT c0 FROM w WHERE c3 = 7",
        // Join (each rel materialized under its own mask).
        "SELECT w.c0, w2.note FROM w JOIN w2 ON w2.id = w.id",
        "SELECT w.c1 FROM w JOIN w2 ON w2.k = w.c3 WHERE w2.note > 72",
        // Subquery / IN (the inner and outer each touch distinct columns).
        "SELECT c0 FROM w WHERE id IN (SELECT id FROM w2 WHERE k = 7)",
        "SELECT c7 FROM w WHERE EXISTS (SELECT 1 FROM w2 WHERE w2.id = w.id AND w2.note > 80)",
    ];
    for sql in queries {
        assert_eq!(
            rows_sorted(&mut mem, sql),
            rows_sorted(&mut paged, sql),
            "rows differ (paged-masked vs resident) for `{sql}`"
        );
        assert_eq!(
            cost(&mut mem, sql),
            cost(&mut paged, sql),
            "cost differs (paged-masked vs resident) for `{sql}`"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// Seed a MULTI-LEVEL B-tree (enough rows that the tree splits past a single leaf into a root interior
/// node carrying separator entries), so the A2/A3 columnar projection gather's interior-separator path —
/// a B-tree stores records in interior nodes too, gathered alongside the leaves — is exercised against
/// the in-memory row oracle. The single-leaf `w` table above never builds an interior node, so its
/// columnar walk only visits leaves. Both databases use the DEFAULT page size so their tree shapes (hence
/// the page_read node counts) are identical; the depth comes from the row count, not a shrunk page.
fn seed_multilevel(db: &mut Session) {
    db.execute(
        "CREATE TABLE m (id i32 PRIMARY KEY, k i32, a i32, b i16, f f64)",
        &[],
    )
    .unwrap();
    const ROWS: i32 = 5000;
    const CHUNK: i32 = 1000;
    let mut start = 0;
    while start < ROWS {
        let mut sql = String::from("INSERT INTO m VALUES ");
        let mut i = start;
        while i < start + CHUNK && i < ROWS {
            if i > start {
                sql.push(',');
            }
            // k has 8 recurring buckets that span leaves; a stays small; b is NULL on every 7th row; f is
            // an exactly-representable f64 (`.5` fraction) so the float SUM/AVG columnar fold matches the
            // resident fold on the last ULP (both run the shared canonical-order FloatFold — float.md §7).
            let b = if i % 7 == 0 {
                "NULL".to_string()
            } else {
                format!("{}", i % 100)
            };
            sql.push_str(&format!(
                "({},{},{},{},{}.5)",
                i,
                i % 8,
                i % 1000,
                b,
                i % 50
            ));
            i += 1;
        }
        db.execute(&sql, &[]).unwrap();
        start += CHUNK;
    }
}

/// Bare-column PROJECTIONS over a multi-level tree take the A2 columnar projection path (Emitter::Columnar)
/// on the paged (file-backed) database and the row path on the resident one — the interior separators are
/// gathered into the lanes alongside the leaf records, so a mis-indexed gather diverges from the resident
/// row path on rows or cost. FILTERED projections take the A3 columnar path: the predicate is applied over
/// the gathered lanes into a selection vector, and the emit visits only the selected lane positions — so a
/// mis-indexed selection vector (an off-by-one against the interior-node gather) diverges loudly here.
#[test]
fn paged_columnar_multilevel_matches_resident() {
    let path = tmp("jed_masked_multilevel.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(
            &path,
            DatabaseOptions {
                page_size: jed::DEFAULT_PAGE_SIZE,
            },
        )
        .unwrap()
        .session(SessionOptions::default());
        seed_multilevel(&mut db);
        drop(db);
    }
    let mut mem = Database::new_in_memory_with_page_size(jed::DEFAULT_PAGE_SIZE)
        .session(SessionOptions::default());
    seed_multilevel(&mut mem);
    let mut paged = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());

    let queries = [
        // Bare-column projections — the columnar projection path (interior + leaf a/k/b values gathered).
        "SELECT a FROM m",
        "SELECT k, a, b FROM m",
        "SELECT id FROM m",                // the PK column projected
        "SELECT a FROM m WHERE id = 2500", // PK point → columnar with a point bound
        "SELECT k, b FROM m WHERE id >= 100 AND id < 400", // PK range → columnar with a range bound
        // Filtered projections — the A3 columnar path over the multi-level tree (selection vector over the
        // interior-separator + leaf gather).
        "SELECT a FROM m WHERE k = 3", // one of 8 recurring buckets, spanning leaves
        "SELECT id, a FROM m WHERE a >= 200 AND a < 800", // proper subset
        "SELECT a FROM m WHERE a < 0", // empty selection vector
        "SELECT k, a FROM m WHERE b IS NULL", // filter on the nullable column
        "SELECT id FROM m WHERE a > 5 AND k < 4", // AND predicate over two columns
        // Whole-table aggregates — the columnar aggregate gather (fold_agg_whole over the interior +
        // leaf lanes), a WHERE (A3) applied over the lanes into a selection vector before the fold.
        "SELECT count(*) FROM m",
        "SELECT sum(a) FROM m",
        "SELECT sum(a), count(b), count(*) FROM m", // multi-spec, nullable operand
        "SELECT min(a), max(a) FROM m",
        "SELECT sum(f), avg(f) FROM m", // float SUM/AVG lane gather + canonical-order FloatFold
        "SELECT sum(a) FROM m WHERE k = 3", // filtered whole-table agg
        "SELECT count(*) FROM m WHERE a >= 500", // filtered COUNT(*)
        "SELECT sum(a) FROM m WHERE a < 0", // empty selection vector → NULL sum
        // Single-integer-key GROUP BY — the columnar grouped fold (group_by_int_key over the lanes), the
        // key + operand columns gathered, buckets in scan-order-of-first-appearance.
        "SELECT k, count(*) FROM m GROUP BY k",
        "SELECT k, sum(a) FROM m GROUP BY k",
        "SELECT k, sum(a), count(b) FROM m GROUP BY k", // grouped multi-spec, nullable operand
        "SELECT k, avg(f) FROM m GROUP BY k",           // grouped float AVG
        "SELECT k, sum(a) FROM m WHERE a >= 200 GROUP BY k", // filtered grouped
        "SELECT k, count(*) FROM m GROUP BY k LIMIT 3", // grouped + LIMIT window over synthetic rows
    ];
    for sql in queries {
        // Eager drive (execute → Emitter::Columnar): paged-columnar vs resident-row.
        assert_eq!(
            rows_sorted(&mut mem, sql),
            rows_sorted(&mut paged, sql),
            "rows differ (paged-columnar vs resident) for `{sql}`"
        );
        assert_eq!(
            cost(&mut mem, sql),
            cost(&mut paged, sql),
            "cost differs (paged-columnar vs resident) for `{sql}`"
        );
        // Lazy drive (query → BufferedScan → BufState::Columnar), fully drained: must observe the same
        // rows AND total cost as the resident eager path (streaming.md §6).
        let (lazy_rows, lazy_cost) = streamed_sorted(&mut paged, sql);
        assert_eq!(
            rows_sorted(&mut mem, sql),
            lazy_rows,
            "lazy-columnar rows differ (paged query() vs resident) for `{sql}`"
        );
        assert_eq!(
            cost(&mut mem, sql),
            lazy_cost,
            "lazy-columnar cost differs (paged query() vs resident) for `{sql}`"
        );
    }
    let _ = std::fs::remove_file(&path);
}
