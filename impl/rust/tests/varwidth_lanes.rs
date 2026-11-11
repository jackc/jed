//! A4 — variable-width columnar lanes + encoded-span predicates (spec/design/packed-leaf.md §11 Track
//! A4). On a paged database a single-table aggregate or bare-column projection reads its touched
//! variable-width columns in place: `IS [NOT] NULL` from the leaf's null directory, text/bytea `=`/`<>`
//! as a byte compare against the stored span, array `@>`/`<@`/`&&` against the stored element bodies,
//! and `COUNT(col)` from nullness alone; every other predicate (and every value a consumer needs) goes
//! through the ordinary evaluator over resolved values. The oracle is the row path: the same session
//! with a (never-reached) `max_cost`, which keeps the projection lane off, and with the aggregate lane's
//! attempt under a ceiling (cost.md §6.1) switched off. For every query shape
//! both must agree on rows AND cost, over NULLs, empty values, embedded 0x00 bytes, multibyte text,
//! NULL array elements, compressed and out-of-line values (which fall back to the evaluator), and a
//! writer's uncommitted (Decoded) leaves. Mirrors Go (varwidth_lanes_test.go) and TS
//! (tests/varwidth_lanes.test.ts).

use jed::{CreateOptions, Database, OpenOptions, Outcome, Session, SessionOptions, Value};

fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
}

fn exec(db: &mut Session, sql: &str) {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// Rows (rendered; sorted unless the query has an ORDER BY) and cost of one query.
fn run(db: &mut Session, sql: &str, params: &[Value]) -> (Vec<Vec<String>>, i64) {
    match db
        .query_outcome(sql, params)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        Outcome::Query { rows, cost, .. } => {
            let mut rs: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect();
            if !sql.contains("ORDER BY") {
                rs.sort();
            }
            (rs, cost)
        }
        other => panic!("expected a query result for `{sql}`, got {other:?}"),
    }
}

/// The lane (unmetered) and the row path (a ceiling it never reaches, the lane's attempt under a
/// ceiling switched off) agree on rows and cost.
fn check(db: &mut Session, sql: &str, params: &[Value]) {
    db.set_max_cost(0);
    let lane = run(db, sql, params);
    db.set_max_cost(i64::MAX / 4);
    jed::tooling::LANE_CEILING_OFF.set(true);
    let row = run(db, sql, params);
    jed::tooling::LANE_CEILING_OFF.set(false);
    db.set_max_cost(0);
    assert_eq!(lane, row, "lane vs row path: {sql}");
}

const SEED: &[&str] = &[
    "CREATE TABLE vt (id i32 PRIMARY KEY, n i32, s text, b bytea, tags text[], nums i32[], d decimal)",
    "INSERT INTO vt VALUES \
        (1, 1, 'a', '\\x00'::bytea, '{a,b}', '{1,2}', 1.5), \
        (2, 2, '', '\\x'::bytea, '{}', '{}', NULL), \
        (3, 3, NULL, NULL, NULL, NULL, 2.25), \
        (4, 4, 'é', '\\x6100'::bytea, '{é,NULL}', '{NULL,3}', 10), \
        (5, 5, 'ñandú', '\\x0000'::bytea, '{NULL}', '{-1}', -3.125), \
        (6, 6, 'a', '\\x61'::bytea, '{b,a,a}', '{3,3}', 0), \
        (7, 7, 'ab', '\\x6162'::bytea, '{{a,b},{c,NULL}}', '{{1,2},{3,4}}', 7)",
    // A compressed value set (large-values.md §13): a highly compressible text, bytea, and text array
    // — each falls back to the evaluator. Row 9 (incompressible, out of line) is built by `seed`.
    "INSERT INTO vt VALUES \
        (8, 8, repeat('x', 30000), decode(repeat('ab', 9000), 'hex'), ARRAY[repeat('a', 9000), 'a'], '{5}', 1)",
    "INSERT INTO vt SELECT g, g % 7, 'v' || (g % 13)::text, NULL, ARRAY['t' || (g % 5)::text], ARRAY[(g % 4)::i32], NULL \
        FROM generate_series(10, 400) g",
];

const QUERIES: &[&str] = &[
    // Nullness atoms, COUNT(col).
    "SELECT count(*) FROM vt WHERE s IS NULL",
    "SELECT count(*) FROM vt WHERE s IS NOT NULL",
    "SELECT count(*) FROM vt WHERE tags IS NULL",
    "SELECT count(*) FROM vt WHERE d IS NOT NULL",
    "SELECT count(s), count(b), count(tags), count(nums), count(d) FROM vt",
    // Text / bytea equality atoms (either operand order, empty, multibyte, embedded zero bytes).
    "SELECT count(*) FROM vt WHERE s = 'a'",
    "SELECT count(*) FROM vt WHERE 'a' = s",
    "SELECT count(*) FROM vt WHERE s <> 'a'",
    "SELECT count(*) FROM vt WHERE s = ''",
    "SELECT count(*) FROM vt WHERE s = 'é'",
    "SELECT count(*) FROM vt WHERE s = 'ñandú'",
    "SELECT count(*) FROM vt WHERE s = 'v3'",
    "SELECT count(*) FROM vt WHERE s <> 'v3'",
    "SELECT count(*) FROM vt WHERE b = '\\x00'::bytea",
    "SELECT count(*) FROM vt WHERE b = '\\x'::bytea",
    "SELECT count(*) FROM vt WHERE b = '\\x0000'::bytea",
    "SELECT count(*) FROM vt WHERE b <> '\\x6100'::bytea",
    // Array atoms (constant on either side; NULL elements; empty; multidimensional; ints).
    "SELECT count(*) FROM vt WHERE tags @> '{a}'::text[]",
    "SELECT count(*) FROM vt WHERE tags @> ARRAY['a']",
    "SELECT count(*) FROM vt WHERE tags @> ARRAY['a', 'b']",
    "SELECT count(*) FROM vt WHERE tags @> '{}'::text[]",
    "SELECT count(*) FROM vt WHERE tags @> ARRAY[NULL]::text[]",
    "SELECT count(*) FROM vt WHERE tags <@ '{a,b,é}'::text[]",
    "SELECT count(*) FROM vt WHERE tags && '{b,é,t1}'::text[]",
    "SELECT count(*) FROM vt WHERE '{a}'::text[] <@ tags",
    "SELECT count(*) FROM vt WHERE '{a,b,c}'::text[] @> tags",
    "SELECT count(*) FROM vt WHERE tags @> '{c}'::text[]",
    "SELECT count(*) FROM vt WHERE nums @> '{3}'::i32[]",
    "SELECT count(*) FROM vt WHERE nums && '{1,-1}'::i32[]",
    "SELECT count(*) FROM vt WHERE nums <@ '{1,2,3}'::i32[]",
    // Mixed: atoms beside generic predicates, OR / NOT (generic), short-circuit order.
    "SELECT count(*) FROM vt WHERE s = 'a' AND n > 1",
    "SELECT count(*) FROM vt WHERE n > 1 AND s = 'a' AND tags @> '{a}'::text[]",
    "SELECT count(*) FROM vt WHERE s IS NULL OR n = 2",
    "SELECT count(*) FROM vt WHERE NOT (s = 'a')",
    "SELECT count(*) FROM vt WHERE s LIKE 'v1%' AND b IS NULL",
    "SELECT count(*) FROM vt WHERE length(s) > 100",
    // Aggregates that resolve values.
    "SELECT max(s), min(s) FROM vt",
    "SELECT max(s) FROM vt WHERE tags @> '{t2}'::text[]",
    "SELECT min(b), max(d) FROM vt",
    "SELECT n, count(tags) FROM vt GROUP BY n",
    "SELECT n, max(s) FROM vt WHERE s <> 'v1' GROUP BY n",
    // Projection lane: survivors only, variable-width projected columns, filter-only columns.
    "SELECT id, s FROM vt",
    "SELECT id, s, tags FROM vt WHERE s = 'a'",
    "SELECT tags, b FROM vt WHERE n = 4",
    "SELECT id FROM vt WHERE tags && '{a,é}'::text[]",
    "SELECT s, s FROM vt WHERE s IS NOT NULL AND id < 12",
    "SELECT id, s FROM vt WHERE length(s) > 100",
    "SELECT id, d FROM vt WHERE d IS NOT NULL",
    "SELECT id FROM vt WHERE id >= 5 AND s = 'v5'",
];

fn check_all(db: &mut Session) {
    for q in QUERIES {
        check(db, q, &[]);
    }
    // A bind parameter is a row-independent constant too.
    check(
        db,
        "SELECT count(*) FROM vt WHERE s = $1",
        &[Value::Text("v4".into())],
    );
    check(
        db,
        "SELECT id FROM vt WHERE s <> $1 AND n = 3",
        &[Value::Text("v3".into())],
    );
}

/// One query's outcome: its rows and cost, or its error code.
fn outcome(db: &mut Session, sql: &str) -> std::result::Result<(Vec<Vec<String>>, i64), String> {
    match db.query_outcome(sql, &[]) {
        Ok(Outcome::Query { rows, cost, .. }) => {
            let mut rs: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect();
            if !sql.contains("ORDER BY") {
                rs.sort();
            }
            Ok((rs, cost))
        }
        Ok(other) => panic!("expected a query result for `{sql}`, got {other:?}"),
        Err(e) => Err(e.code().to_string()),
    }
}

/// Under a query-memory budget the lane and the row path hold different buffers, so they may trip
/// `54P05` at different budgets (memory.md §5.4; the thresholds themselves are pinned across cores by
/// `mise run conformance:query_memory`). What must hold on every path and at every budget: a
/// statement either fails `54P05` or returns exactly its unbudgeted rows and cost. Run unmetered (the
/// lane), under a ceiling it never reaches (the lane as an attempt, cost.md §6.1), and with the
/// attempt off (the row path).
fn check_under_budgets(db: &mut Session, sql: &str) {
    db.set_max_cost(0);
    let want = outcome(db, sql);
    assert!(want.is_ok(), "unbudgeted: {sql}: {want:?}");
    for budget in [64, 4096, 64 * 1024, 1024 * 1024, 64 * 1024 * 1024] {
        db.set_max_query_memory_bytes(budget);
        for (max_cost, off) in [(0, false), (i64::MAX / 4, false), (i64::MAX / 4, true)] {
            db.set_max_cost(max_cost);
            jed::tooling::LANE_CEILING_OFF.set(off);
            let got = outcome(db, sql);
            jed::tooling::LANE_CEILING_OFF.set(false);
            assert!(
                got == want || got == Err("54P05".to_string()),
                "budget {budget}, max_cost {max_cost}, attempt off {off}: {sql}: {got:?}"
            );
        }
        db.set_max_cost(0);
        db.set_max_query_memory_bytes(0);
    }
}

/// The decimal folds on the lane: `SUM(i64)`, `SUM(decimal)`, and `AVG` over an integer or a decimal
/// charge `decimal_work` against the running sum (cost.md §3), so the lane must meet the same running
/// sums in the same order as the row path to charge the same total. The values span one to many
/// base-10⁴ groups, mixed scales and signs, and sums that grow past 2^64 and 2^128, so the charge
/// changes as each sum grows.
const DECIMAL_FOLD_QUERIES: &[&str] = &[
    "SELECT sum(big) FROM dt",
    "SELECT sum(d) FROM dt",
    "SELECT sum(wide) FROM dt",
    "SELECT avg(big), avg(d), avg(wide), avg(n) FROM dt",
    "SELECT sum(big), sum(d), sum(wide), count(*), min(d), max(wide) FROM dt",
    "SELECT sum(d) FROM dt WHERE n > 2",
    "SELECT avg(d) FROM dt WHERE d > 0",
    "SELECT sum(big), avg(wide) FROM dt WHERE wide IS NOT NULL AND big < 0",
    "SELECT sum(d) FROM dt WHERE id < 0",
    "SELECT n, sum(big), sum(d), avg(d) FROM dt GROUP BY n",
    "SELECT n, sum(wide), avg(big), count(d) FROM dt WHERE d <> 0 GROUP BY n",
    "SELECT k, sum(d) FROM dt GROUP BY k",
];

#[test]
fn decimal_folds_match_row_path() {
    let seed: &[&str] = &[
        "CREATE TABLE dt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), wide numeric)",
        // Small money-shaped values, a NULL in every column, and a NULL group key.
        "INSERT INTO dt VALUES \
            (-3, 1, NULL, 9223372036854775807, 0.01, 1), \
            (-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001), \
            (-1, NULL, 2, NULL, NULL, NULL), \
            (0, 2, 2, 0, 0.00, 0.0)",
        "INSERT INTO dt SELECT g, g % 5, g % 3, \
            CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, \
            ((g * 7919) % 100000000)::numeric / 100 - 250000, \
            CASE WHEN g % 11 = 0 THEN NULL \
                 WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0 \
                 WHEN g % 4 = 0 THEN -(g::numeric / 1000000007) \
                 ELSE g::numeric * 1.000001 END \
            FROM generate_series(1, 3000) g",
    ];
    let path = std::env::temp_dir().join("jed_decimal_fold_lanes.jed");
    remove_db(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(path.clone()),
            skip_fsync: true,
            page_size: jed::DEFAULT_PAGE_SIZE,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
        for s in seed {
            exec(&mut db, s);
        }
    }
    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    // Clean (Packed) leaves faulted from the file, then a writer's uncommitted (Decoded) leaves.
    for q in DECIMAL_FOLD_QUERIES {
        check(&mut db, q, &[]);
        check_under_budgets(&mut db, q);
    }
    exec(&mut db, "BEGIN");
    exec(
        &mut db,
        "INSERT INTO dt VALUES (5000, 1, 1, 42, 123.45, 1e40)",
    );
    exec(
        &mut db,
        "UPDATE dt SET d = d + 1, big = big / 2 WHERE n = 3",
    );
    for q in DECIMAL_FOLD_QUERIES {
        check(&mut db, q, &[]);
    }
    exec(&mut db, "ROLLBACK");
    drop(db);
    remove_db(&path);

    // The same over an in-memory database.
    let mut mem = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in seed {
        exec(&mut mem, s);
    }
    for q in DECIMAL_FOLD_QUERIES {
        check(&mut mem, q, &[]);
        check_under_budgets(&mut mem, q);
    }
}

/// Grouped aggregates on a file-backed database take the columnar lane while the group table fits
/// `work_mem`, then hand it over to the spill structures and carry on (packed-leaf.md §11). Each
/// budget below places the hand-over somewhere else — after the first row, after a few groups,
/// part-way through a many-group scan, never — and at each the lane must agree with the row path
/// on rows and cost.
const GROUPED_HANDOVER_QUERIES: &[&str] = &[
    // A few groups, every accumulator kind (fixed-size and size-changing), NULL keys.
    "SELECT n, count(*), count(d), sum(big), sum(d), avg(d), min(d), max(wide), min(s), max(s) FROM gt GROUP BY n",
    "SELECT k, sum(wide), avg(big), min(wide), max(big), count(s) FROM gt WHERE d <> 0 GROUP BY k",
    "SELECT n, sum(f), avg(f), sum(m) FROM gt GROUP BY n",
    // About one group per row.
    "SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
    "SELECT big, count(*), min(s) FROM gt WHERE n > 1 AND s <> 'v3' GROUP BY big",
    // Grouped by the primary key, which the lane does not take: the same answer all the same.
    "SELECT id, sum(big) FROM gt GROUP BY id",
    // No survivors, and a primary-key bound.
    "SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n",
    "SELECT n, sum(d), max(s) FROM gt WHERE id BETWEEN 100 AND 900 GROUP BY n",
];

/// Grouped folds with an `ORDER BY` over their groups — by the key, by an aggregate, by an
/// expression of both — each a total order, so the row sequence is part of the comparison.
const ORDERED_GROUP_QUERIES: &[&str] = &[
    "SELECT n, count(*), sum(d) FROM gt GROUP BY n ORDER BY n",
    "SELECT k, avg(big), max(wide) FROM gt GROUP BY k ORDER BY k DESC NULLS FIRST",
    "SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
    "SELECT big, count(*), min(s) FROM gt GROUP BY big ORDER BY min(s) DESC, big LIMIT 9 OFFSET 3",
    "SELECT big, sum(d) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big",
    "SELECT big, sum(d) FROM gt GROUP BY big ORDER BY sum(d) + big % 3 DESC, big LIMIT 40",
    "SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n ORDER BY n",
    "SELECT n, max(s) FROM gt GROUP BY n ORDER BY n LIMIT 0",
];

/// The `gt` table — narrow and wide decimals, text, floats, NULLs in every column — in a fresh
/// database: file-backed at `file` in the temp directory and reopened (clean Packed leaves), or in
/// memory.
fn gt_session(file: Option<&str>) -> (Option<std::path::PathBuf>, Session) {
    let path = file.map(|f| std::env::temp_dir().join(f));
    if let Some(p) = &path {
        remove_db(p);
    }
    let mut db = Database::create(CreateOptions {
        path: path.clone(),
        skip_fsync: true,
        page_size: jed::DEFAULT_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    exec(
        &mut db,
        "CREATE TABLE gt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), \
         wide numeric, s text, f f64, m i16)",
    );
    exec(
        &mut db,
        "INSERT INTO gt VALUES \
            (-3, 1, NULL, 9223372036854775807, 0.01, 1, 'a', 1.5, 1), \
            (-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001, '', -2.25, -1), \
            (-1, NULL, 2, NULL, NULL, NULL, NULL, NULL, NULL), \
            (0, 2, 2, 0, 0.00, 0.0, 'é', 0, 0)",
    );
    exec(
        &mut db,
        "INSERT INTO gt SELECT g, g % 5, g % 3, \
            CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, \
            ((g * 7919) % 100000000)::numeric / 100 - 250000, \
            CASE WHEN g % 11 = 0 THEN NULL \
                 WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0 \
                 WHEN g % 4 = 0 THEN -(g::numeric / 1000000007) \
                 ELSE g::numeric * 1.000001 END, \
            'v' || repeat('x', (g % 40)::i32) || (g % 97)::text, \
            (g % 1000)::f64 / 8, (g % 300)::i16 \
            FROM generate_series(1, 3000) g",
    );
    let Some(p) = &path else {
        return (path, db);
    };
    drop(db);
    let db = Database::open_with_options(
        p,
        OpenOptions {
            skip_fsync: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    (path, db)
}

#[test]
fn grouped_lane_hands_over_to_spill() {
    let (path, mut db) = gt_session(Some("jed_grouped_handover_lanes.jed"));
    let path = path.unwrap();
    let handovers =
        || jed::tooling::LANE_GROUP_HANDOVERS.load(std::sync::atomic::Ordering::Relaxed);
    // `work_mem` of 1 hands over after the first survivor; the middle budgets after a handful of
    // groups and part-way through the many-group scans; the default never.
    for work_mem in [1usize, 400, 3_000, 40_000, 300_000, jed::DEFAULT_WORK_MEM] {
        db.set_work_mem(work_mem);
        for q in GROUPED_HANDOVER_QUERIES {
            let before = handovers();
            check(&mut db, q, &[]);
            // At the smallest budgets every query with a survivor hands over on its lane run.
            if work_mem <= 400 && !q.contains("id < -100") && !q.contains("GROUP BY id") {
                assert!(
                    handovers() > before,
                    "work_mem {work_mem}: no hand-over: {q}"
                );
            }
        }
    }
    // An ORDER BY over the groups: sorted by the bounded-spill lane's own stage, whether or not the
    // fold handed over.
    for work_mem in [1usize, 400, 3_000, 40_000, jed::DEFAULT_WORK_MEM] {
        db.set_work_mem(work_mem);
        for q in ORDERED_GROUP_QUERIES {
            let before = handovers();
            check(&mut db, q, &[]);
            if work_mem <= 400 && !q.contains("id < -100") {
                assert!(
                    handovers() > before,
                    "work_mem {work_mem}: no hand-over: {q}"
                );
            }
        }
    }
    // LIMIT/OFFSET over a handed-over result: the same window of the same emission order.
    db.set_work_mem(3_000);
    for q in [
        "SELECT big, sum(d) FROM gt GROUP BY big LIMIT 7",
        "SELECT big, count(*) FROM gt GROUP BY big LIMIT 5 OFFSET 2990",
        "SELECT big, count(*) FROM gt GROUP BY big OFFSET 5000",
        "SELECT n, max(s) FROM gt GROUP BY n LIMIT 0",
    ] {
        check(&mut db, q, &[]);
    }
    // A writer's uncommitted (Decoded) leaves, mid hand-over.
    exec(&mut db, "BEGIN");
    exec(
        &mut db,
        "INSERT INTO gt VALUES (5000, 9, 9, 42, 123.45, 1e40, 'zz', 3.5, 7)",
    );
    exec(&mut db, "UPDATE gt SET d = d + 1, s = s || '!' WHERE n = 3");
    for work_mem in [1usize, 3_000, jed::DEFAULT_WORK_MEM] {
        db.set_work_mem(work_mem);
        for q in GROUPED_HANDOVER_QUERIES {
            check(&mut db, q, &[]);
        }
    }
    exec(&mut db, "ROLLBACK");
    // Under a query-memory budget: the same rows and cost, or `54P05`.
    db.set_work_mem(3_000);
    for q in GROUPED_HANDOVER_QUERIES.iter().chain(ORDERED_GROUP_QUERIES) {
        check_under_budgets(&mut db, q);
    }
    drop(db);
    remove_db(&path);
}

/// Ordered rows of one query, or its error code.
fn ordered(db: &mut Session, sql: &str) -> std::result::Result<Vec<Vec<String>>, String> {
    assert!(sql.contains("ORDER BY"));
    outcome(db, sql).map(|(rows, _)| rows)
}

/// The single-table streaming sort (packed-leaf.md §11 Track A5) reads only the touched columns into a
/// reused row, and its top-k heap takes a row only when it keeps it. Every `LIMIT`/`OFFSET` window
/// must be that slice of the full sort, and the full sort must agree with the same query over a
/// derived table, which the eager executor sorts — across fixed-width, bounded-decimal, unbounded
/// and text keys (on a file the last two take the external sorter), with ties, NULLs, a filter, a
/// writer's uncommitted leaves, and query-memory budgets.
fn check_top_n(db: &mut Session, budgets: bool) {
    const KEYS: &[&str] = &[
        "d DESC, id",
        "d NULLS FIRST, id DESC",
        "n, d DESC NULLS LAST, id",
        "big DESC NULLS FIRST, id",
        "wide DESC NULLS LAST, id",
        "s, id",
        "f DESC, m, id",
        "m DESC NULLS LAST, id",
    ];
    const WINDOWS: &[(usize, usize)] = &[(10, 0), (7, 5), (1, 3003), (5000, 0), (0, 0), (3, 2999)];
    // Every key over a projection that also touches text and an unbounded decimal (on a file: the
    // external sorter), and the keys that need neither over a fixed-width and bounded-decimal
    // projection (on a file: the top-k heap).
    let cases = KEYS
        .iter()
        .map(|k| (*k, "id, d, s, big, n, wide, f, m"))
        .chain(
            KEYS.iter()
                .filter(|k| !k.contains("wide") && !k.starts_with("s,"))
                .map(|k| (*k, "id, d, big, n, f, m")),
        );
    for (key, cols) in cases {
        for filter in ["", " WHERE n > 1 AND d <> 0", " WHERE id < -100"] {
            let base = format!("SELECT {cols} FROM gt{filter}");
            let full = ordered(db, &format!("{base} ORDER BY {key}")).unwrap();
            let eager = ordered(db, &format!("SELECT * FROM ({base}) q ORDER BY {key}")).unwrap();
            assert_eq!(
                full, eager,
                "full sort vs eager sort: {base} ORDER BY {key}"
            );
            for &(limit, offset) in WINDOWS {
                let sql = format!("{base} ORDER BY {key} LIMIT {limit} OFFSET {offset}");
                let want: Vec<_> = full.iter().skip(offset).take(limit).cloned().collect();
                assert_eq!(ordered(db, &sql).unwrap(), want, "{sql}");
                // Under a budget a statement may fail `54P05`, but never returns other rows. (A
                // failure would abort an open transaction, so the writer's pass skips this.)
                let budgets: &[i64] = if budgets {
                    &[64, 4096, 64 * 1024, 1024 * 1024]
                } else {
                    &[]
                };
                for &budget in budgets {
                    db.set_max_query_memory_bytes(budget);
                    let got = ordered(db, &sql);
                    db.set_max_query_memory_bytes(0);
                    match got {
                        Ok(rows) => assert_eq!(rows, want, "budget {budget}: {sql}"),
                        Err(code) => assert_eq!(code, "54P05", "budget {budget}: {sql}"),
                    }
                }
            }
        }
    }
}

fn top_n_matches_full_sort(file: Option<&str>) {
    let (path, mut db) = gt_session(file);
    for work_mem in [3_000usize, jed::DEFAULT_WORK_MEM] {
        db.set_work_mem(work_mem);
        check_top_n(&mut db, true);
    }
    exec(&mut db, "BEGIN");
    exec(
        &mut db,
        "INSERT INTO gt VALUES (5000, 9, 9, 42, 249999.99, 1e40, 'zz', 3.5, 7), \
         (5001, 1, 1, 42, 249999.99, NULL, '', 3.5, NULL)",
    );
    exec(&mut db, "UPDATE gt SET d = d + 1, s = s || '!' WHERE n = 3");
    check_top_n(&mut db, false);
    exec(&mut db, "ROLLBACK");
    drop(db);
    if let Some(p) = path {
        remove_db(&p);
    }
}

#[test]
fn top_n_matches_full_sort_on_a_file() {
    top_n_matches_full_sort(Some("jed_top_n_lanes.jed"));
}

#[test]
fn top_n_matches_full_sort_in_memory() {
    top_n_matches_full_sort(None);
}

#[test]
fn varwidth_lanes_match_row_path() {
    let path = std::env::temp_dir().join("jed_varwidth_lanes.jed");
    remove_db(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(path.clone()),
            skip_fsync: true,
            page_size: jed::DEFAULT_PAGE_SIZE,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
        for s in SEED {
            exec(&mut db, s);
        }
        // Incompressible text and a large text array, stored out of line (large-values.md §12).
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut noise = |n: usize| -> String {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    char::from(b'a' + (x % 26) as u8)
                })
                .collect()
        };
        let long = noise(30000);
        let elems: Vec<String> = (0..700).map(|_| noise(40)).collect();
        exec(
            &mut db,
            &format!(
                "INSERT INTO vt VALUES (9, 9, '{long}', NULL, '{{{}}}', NULL, NULL)",
                elems.join(",")
            ),
        );
    }
    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            skip_fsync: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    // Clean (Packed) leaves faulted from the file.
    check_all(&mut db);
    // A writer's uncommitted changes live in Decoded leaves: the atoms fall back to the evaluator.
    exec(&mut db, "BEGIN");
    exec(
        &mut db,
        "INSERT INTO vt VALUES (1000, 1, 'a', '\\x00'::bytea, '{a}', '{3}', 1)",
    );
    exec(&mut db, "UPDATE vt SET s = 'é' WHERE id = 6");
    check_all(&mut db);
    exec(&mut db, "ROLLBACK");
    drop(db);
    remove_db(&path);
}

// ---- The aggregate lane under a cost ceiling (spec/design/cost.md §6.1) ----------------------

/// One statement's whole observable outcome under the session's current ceilings: its rows and cost,
/// or its error code and text, and what it added to the session's cumulative cost.
fn metered_outcome(db: &mut Session, sql: &str) -> (String, i64) {
    let before = db.lifetime_cost();
    let out = match db.query_outcome(sql, &[]) {
        Ok(Outcome::Query { rows, cost, .. }) => {
            let mut rs: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(|v| v.render()).collect())
                .collect();
            if !sql.contains("ORDER BY") {
                rs.sort();
            }
            format!("cost {cost}: {rs:?}")
        }
        Ok(other) => panic!("expected a query result for `{sql}`, got {other:?}"),
        Err(e) => format!("{}: {e}", e.code()),
    };
    (out, db.lifetime_cost() - before)
}

/// The same statement with the lane's attempt on and off: every observable must match.
fn lane_matches_row_path(db: &mut Session, sql: &str, what: &str) {
    jed::tooling::LANE_CEILING_OFF.set(false);
    let lane = metered_outcome(db, sql);
    jed::tooling::LANE_CEILING_OFF.set(true);
    let row = metered_outcome(db, sql);
    jed::tooling::LANE_CEILING_OFF.set(false);
    assert_eq!(lane, row, "{what}: {sql}");
}

/// What the row path alone spends on `sql` with a ceiling it never reaches (to an error, if it
/// raises one): the top of the range of ceilings worth sweeping.
fn row_path_spend(db: &mut Session, sql: &str) -> i64 {
    jed::tooling::LANE_CEILING_OFF.set(true);
    db.set_max_cost(i64::MAX / 4);
    let (_, spent) = metered_outcome(db, sql);
    db.set_max_cost(0);
    jed::tooling::LANE_CEILING_OFF.set(false);
    spent
}

/// `sql` at every `step`-th `max_cost` from 1 to past its cost, and at the boundary itself.
fn sweep_max_cost(db: &mut Session, sql: &str, step: usize) {
    let top = row_path_spend(db, sql);
    let limits = (1..=top + 2)
        .step_by(step)
        .chain(top - 1..=top + 2)
        .filter(|l| *l > 0);
    for limit in limits {
        db.set_max_cost(limit);
        lane_matches_row_path(db, sql, &format!("max_cost {limit}"));
    }
    db.set_max_cost(0);
}

/// `sql` under a lifetime budget with `left` units remaining, alone and beside a `max_cost` one
/// either side of it (which ceiling's error wins is part of the outcome).
fn sweep_lifetime(db: &mut Session, sql: &str, step: usize) {
    let top = row_path_spend(db, sql);
    for left in (1..=top + 2).step_by(step).chain(top - 1..=top + 2) {
        for max_cost in [0, left - 1, left, left + 1] {
            if max_cost < 0 {
                continue;
            }
            // Each run draws on the budget, so it is re-armed `left` above the running total.
            let mut run = |db: &mut Session, off: bool| {
                jed::tooling::LANE_CEILING_OFF.set(off);
                db.set_lifetime_max_cost(db.lifetime_cost() + left);
                db.set_max_cost(max_cost);
                let (mut out, spent) = metered_outcome(db, sql);
                // The lifetime error's text carries the session's absolute totals, which differ
                // between the two runs; its code and the cost it left accrued are what must match.
                if out.starts_with("54P02") {
                    out.truncate(5);
                }
                let out = (out, spent);
                db.set_lifetime_max_cost(0);
                db.set_max_cost(0);
                jed::tooling::LANE_CEILING_OFF.set(false);
                out
            };
            let lane = run(db, false);
            let row = run(db, true);
            assert_eq!(lane, row, "lifetime +{left}, max_cost {max_cost}: {sql}");
        }
    }
}

/// A 300-row table small enough to try every ceiling: a NULL row, decimals of both signs, text.
fn ct_session(file: Option<&str>) -> (Option<std::path::PathBuf>, Session) {
    let path = file.map(|f| std::env::temp_dir().join(f));
    if let Some(p) = &path {
        remove_db(p);
    }
    let mut db = Database::create(CreateOptions {
        path: path.clone(),
        skip_fsync: true,
        page_size: jed::DEFAULT_PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    exec(
        &mut db,
        "CREATE TABLE ct (id i32 PRIMARY KEY, n i32, big i64, d numeric(12,2), s text)",
    );
    exec(&mut db, "INSERT INTO ct VALUES (0, NULL, NULL, NULL, NULL)");
    exec(
        &mut db,
        "INSERT INTO ct SELECT g, g % 5, \
            CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, \
            ((g * 7919) % 100000000)::numeric / 100 - 250000, \
            'v' || repeat('x', (g % 40)::i32) || (g % 97)::text \
            FROM generate_series(1, 300) g",
    );
    let Some(p) = &path else {
        return (path, db);
    };
    drop(db);
    let db = Database::open_with_options(
        p,
        OpenOptions {
            skip_fsync: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    (path, db)
}

/// Lane-shaped statements whose filters are safe to discard: whole-table and grouped folds, ordered
/// groups, a window, no survivors, and filters that raise part-way through the scan.
const CEILING_QUERIES: &[&str] = &[
    "SELECT count(*), sum(d), min(s) FROM ct",
    "SELECT sum(big), avg(d) FROM ct WHERE n > 1 AND d <> 0",
    "SELECT n, count(*), sum(d) FROM ct GROUP BY n",
    "SELECT n, sum(d), max(s) FROM ct WHERE big % 3 <> 1 GROUP BY n ORDER BY sum(d) DESC, n",
    "SELECT n, count(*) FROM ct GROUP BY n ORDER BY n LIMIT 2 OFFSET 1",
    "SELECT big, count(*), max(d) FROM ct WHERE n IN (1, 3) GROUP BY big",
    "SELECT n, sum(d) FROM ct WHERE id < -5 GROUP BY n",
    "SELECT count(*), sum(d) FROM ct WHERE 1000 / (id - 200) > 0",
    "SELECT n, sum(d) FROM ct WHERE CASE WHEN id > 250 THEN 1000 / (id - 260) ELSE 1 END <> 7 GROUP BY n",
    "SELECT * FROM (SELECT n, sum(d) AS t FROM ct GROUP BY n) g WHERE t IS NOT NULL",
    // An uncorrelated scalar subquery is folded to a constant before the lane runs.
    "SELECT count(*), sum(d) FROM ct WHERE n > (SELECT 1)",
];

fn lane_under_a_ceiling_matches_row_path(file: Option<&str>) {
    let (path, mut db) = ct_session(file);
    let attempts = || jed::tooling::LANE_CEILING_ATTEMPTS.get();
    // Every ceiling on a file; every third in memory, where the lane and row path are the same pair.
    let step = if path.is_some() { 1 } else { 3 };
    for q in CEILING_QUERIES {
        let before = attempts();
        sweep_max_cost(&mut db, q, step);
        let (tried, fell_back) = attempts();
        // In memory an ordered grouping does not take the lane at all (packed-leaf.md §11 Track A5).
        if path.is_none() && q.contains("ORDER BY") {
            assert_eq!((tried, fell_back), before, "attempted in memory: {q}");
            continue;
        }
        assert!(tried > before.0, "the lane was never attempted: {q}");
        // A statement that completes was accepted at its high ceilings and rejected at its low ones.
        assert!(fell_back > before.1, "the lane never fell back: {q}");
        if !q.contains("1000 /") {
            assert!(
                tried - before.0 > fell_back - before.1,
                "the lane was never accepted: {q}"
            );
        }
        sweep_lifetime(&mut db, q, 41);
    }
    // The grouped fold handing over to the spill structures part-way (file only).
    if path.is_some() {
        for work_mem in [1usize, 400, 3_000] {
            db.set_work_mem(work_mem);
            for q in CEILING_QUERIES.iter().filter(|q| q.contains("GROUP BY")) {
                sweep_max_cost(&mut db, q, 7);
            }
        }
        db.set_work_mem(jed::DEFAULT_WORK_MEM);
    }
    // Not safe to discard — entropy, a function call, a correlated subquery, a regex — so never
    // attempted.
    for q in [
        "SELECT count(*), sum(d) FROM ct WHERE uuidv4() IS NOT NULL",
        "SELECT n, sum(d) FROM ct WHERE abs(n) > 1 GROUP BY n",
        "SELECT count(*) FROM ct WHERE n > (SELECT count(*) FROM ct c WHERE c.id = ct.id)",
        "SELECT count(*) FROM ct WHERE s ~ '^vx+1'",
    ] {
        let before = attempts();
        sweep_max_cost(&mut db, q, 97);
        assert_eq!(
            attempts(),
            before,
            "attempted though not safe to discard: {q}"
        );
    }
    // With a query-memory budget too (one nothing here reaches), the attempt is still made.
    db.set_max_query_memory_bytes(64 * 1024 * 1024);
    let before = attempts();
    for q in CEILING_QUERIES {
        sweep_max_cost(&mut db, q, 97);
    }
    assert!(attempts().0 > before.0);
    db.set_max_query_memory_bytes(0);
    // A writer's uncommitted leaves, at a ceiling the statement fits under (an abort would end the
    // transaction).
    exec(&mut db, "BEGIN");
    exec(&mut db, "INSERT INTO ct VALUES (900, 9, 42, 123.45, 'zz')");
    exec(&mut db, "UPDATE ct SET d = d + 1, s = s || '!' WHERE n = 3");
    for q in CEILING_QUERIES.iter().filter(|q| !q.contains("1000 /")) {
        let top = row_path_spend(&mut db, q);
        db.set_max_cost(top + 1);
        lane_matches_row_path(&mut db, q, "uncommitted writer");
        db.set_max_cost(0);
    }
    exec(&mut db, "ROLLBACK");
    drop(db);
    if let Some(p) = path {
        remove_db(&p);
    }
}

#[test]
fn lane_under_a_ceiling_matches_row_path_on_a_file() {
    lane_under_a_ceiling_matches_row_path(Some("jed_ceiling_lanes.jed"));
}

#[test]
fn lane_under_a_ceiling_matches_row_path_in_memory() {
    lane_under_a_ceiling_matches_row_path(None);
}

/// A sampled sweep over the 3,000-row table: many groups, a mid-scan hand-over, wide decimals.
#[test]
fn lane_under_a_ceiling_matches_row_path_on_a_larger_table() {
    let (path, mut db) = gt_session(Some("jed_ceiling_lanes_large.jed"));
    for work_mem in [3_000usize, jed::DEFAULT_WORK_MEM] {
        db.set_work_mem(work_mem);
        for q in [
            "SELECT count(*), sum(wide), avg(big), max(s) FROM gt WHERE d <> 0",
            "SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
            "SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
            "SELECT big, count(*) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big LIMIT 9",
        ] {
            sweep_max_cost(&mut db, q, 251);
        }
    }
    drop(db);
    remove_db(&path.unwrap());
}
