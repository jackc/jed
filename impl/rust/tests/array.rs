//! Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural `i32[]` column,
//! the `ARRAY[…]` constructor + the `'{…}'` literal, the compact value codec (S2), btree-NULL
//! element comparison / ORDER BY / DISTINCT (S4), and `array_out` rendering. v1 is 1-D values of
//! scalar elements; multidim values, arrays-in-keys, slices, and the array function surface are
//! deferred (§12).

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

fn query(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

/// S2 codec: an array column survives a whole-image serialize + reload (`to_image` → `from_image`),
/// exercising `encode_array_body` / `read_array_body` (the null bitmap, the empty array, a NULL
/// array). The on-disk array body is version-independent (spec/design/array.md §4).
#[test]
fn array_image_roundtrip() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])",
    );
    run(&mut db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3], '{}')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL, NULL)");
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image)
        .expect("load image")
        .session(SessionOptions::default());
    assert_eq!(
        query(&mut loaded, "SELECT id, xs, tags FROM t ORDER BY id"),
        vec![
            vec![
                "1".to_string(),
                "{10,20,30}".to_string(),
                "{a,b}".to_string()
            ],
            vec!["2".to_string(), "{1,NULL,3}".to_string(), "{}".to_string()],
            vec!["3".to_string(), "NULL".to_string(), "NULL".to_string()],
        ]
    );
}

// --- AC1: array-of-composite element types (spec/design/array.md §12) -----------------------------

/// AC1: a composite type is a first-class array element type. Construct via the `'{…}'::addr[]`
/// literal (array_in → record_in per element) AND via the `ARRAY[ROW(…)]` constructor with the
/// column's composite element context (the jed extension PG needs `::addr` casts for — covered here,
/// not in the PG-oracle corpus). `array_out` nests the two quoting layers; subscript yields the
/// composite, field access reads into it, a slice yields `addr[]`.
#[test]
fn array_of_composite_roundtrip_and_access() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
    // The text-literal construction path.
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"(Main,90210)\",\"(Side,5)\"}')",
    );
    // The ARRAY[ROW(…)] constructor with composite element context (no `::addr` cast needed).
    run(
        &mut db,
        "INSERT INTO t VALUES (2, ARRAY[ROW('Other, Ln', 12)])",
    );
    run(&mut db, "INSERT INTO t VALUES (3, '{\"(Main,)\",NULL}')");
    assert_eq!(
        query(&mut db, "SELECT id, items FROM t ORDER BY id"),
        vec![
            vec!["1", "{\"(Main,90210)\",\"(Side,5)\"}"],
            vec!["2", "{\"(\\\"Other, Ln\\\",12)\"}"],
            vec!["3", "{\"(Main,)\",NULL}"],
        ]
    );
    // Subscript → the composite element (record_out, no braces); field access reads a field; a slice
    // stays addr[].
    assert_eq!(
        query(&mut db, "SELECT items[1] FROM t WHERE id = 1"),
        vec![vec!["(Main,90210)"]]
    );
    assert_eq!(
        query(&mut db, "SELECT (items[2]).street FROM t WHERE id = 1"),
        vec![vec!["Side"]]
    );
    assert_eq!(
        query(&mut db, "SELECT items[1:1] FROM t WHERE id = 1"),
        vec![vec!["{\"(Main,90210)\"}"]]
    );
}

/// AC1: an `addr[]` column survives the on-disk image round-trip byte-for-byte (the recursive value
/// codec — composite element bodies inside the array body; complements the cross-core golden).
#[test]
fn array_of_composite_image_roundtrip() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"(Main,90210)\",\"(Side,5)\"}')",
    );
    run(&mut db, "INSERT INTO t VALUES (2, '{\"(Main,)\",NULL}')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL)");
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image)
        .expect("load image")
        .session(SessionOptions::default());
    assert_eq!(
        query(&mut loaded, "SELECT id, items FROM t ORDER BY id"),
        vec![
            vec!["1", "{\"(Main,90210)\",\"(Side,5)\"}"],
            vec!["2", "{\"(Main,)\",NULL}"],
            vec!["3", "NULL"],
        ]
    );
}

/// AC1, the load-bearing comparison fix: a composite element's per-element compare routes through
/// the composite TOTAL ORDER (NULLs-last, definite), NOT the 3VL — so the ordering operators
/// `< <= > >=` are consistent for arrays whose composite elements have NULL fields (the bug the
/// scalar element path never reached). Equal-with-NULL-field arrays compare `<=` AND `>=` TRUE and
/// `<` FALSE; a NULL field sorts AFTER a present field (spec/design/array.md §5, oracle-pinned).
#[test]
fn array_of_composite_null_field_ordering_operators() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    // Equal arrays with a NULL composite field: definite, never UNKNOWN.
    assert_eq!(
        query(
            &mut db,
            "SELECT '{\"(1,)\"}'::addr[] <= '{\"(1,)\"}'::addr[], \
                    '{\"(1,)\"}'::addr[] >= '{\"(1,)\"}'::addr[], \
                    '{\"(1,)\"}'::addr[] <  '{\"(1,)\"}'::addr[]"
        ),
        vec![vec!["true", "true", "false"]]
    );
    // A NULL field sorts after a present field: {(a,)} > {(a,1)} and {(a,1)} < {(a,)}.
    assert_eq!(
        query(
            &mut db,
            "SELECT '{\"(a,)\"}'::addr[] > '{\"(a,1)\"}'::addr[], \
                    '{\"(a,1)\"}'::addr[] < '{\"(a,)\"}'::addr[]"
        ),
        vec![vec!["true", "true"]]
    );
}

/// AC1: a composite `PRIMARY KEY` element array stays `0A000` (arrays are never keyable this slice,
/// §8) — the new element type does not relax the key gate.
#[test]
fn array_of_composite_primary_key_is_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    assert_eq!(
        err(&mut db, "CREATE TABLE t (items addr[] PRIMARY KEY)"),
        "0A000"
    );
}
