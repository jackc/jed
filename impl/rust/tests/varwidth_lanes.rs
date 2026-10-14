//! A4 — variable-width columnar lanes + encoded-span predicates (spec/design/packed-leaf.md §11 Track
//! A4). On a paged database a single-table aggregate or bare-column projection reads its touched
//! variable-width columns in place: `IS [NOT] NULL` from the leaf's null directory, text/bytea `=`/`<>`
//! as a byte compare against the stored span, array `@>`/`<@`/`&&` against the stored element bodies,
//! and `COUNT(col)` from nullness alone; every other predicate (and every value a consumer needs) goes
//! through the ordinary evaluator over resolved values. The lanes run only on the unmetered lane, so the
//! same session with a (never-reached) `max_cost` runs the row path — the oracle. For every query shape
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

/// Rows (rendered, sorted — no query here has an ORDER BY) and cost of one query.
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
            rs.sort();
            (rs, cost)
        }
        other => panic!("expected a query result for `{sql}`, got {other:?}"),
    }
}

/// The lane (unmetered) and the row path (a ceiling it never reaches) agree on rows and cost.
fn check(db: &mut Session, sql: &str, params: &[Value]) {
    db.set_max_cost(0);
    let lane = run(db, sql, params);
    db.set_max_cost(i64::MAX / 4);
    let row = run(db, sql, params);
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
