//! Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural `int32[]` column,
//! the `ARRAY[…]` constructor + the `'{…}'` literal, the compact value codec (S2), btree-NULL
//! element comparison / ORDER BY / DISTINCT (S4), and `array_out` rendering. v1 is 1-D values of
//! scalar elements; multidim values, arrays-in-keys, slices, and the array function surface are
//! deferred (§12).

use jed::{Database, Outcome, execute};

fn run(db: &mut Database, sql: &str) {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

/// S2: an `int32[]` column round-trips an `ARRAY[…]` constructor and a `'{…}'` literal, rendered by
/// `array_out` as `{…}`. The empty array renders `{}`.
#[test]
fn array_column_roundtrip() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])",
    );
    run(&mut db, "INSERT INTO t VALUES (2, '{40,50}', '{}')");
    assert_eq!(
        query(&mut db, "SELECT id, xs, tags FROM t ORDER BY id"),
        vec![
            vec![
                "1".to_string(),
                "{10,20,30}".to_string(),
                "{a,b}".to_string()
            ],
            vec!["2".to_string(), "{40,50}".to_string(), "{}".to_string()],
        ]
    );
}

/// S2 codec: an array column survives a whole-image serialize + reload (`to_image` → `from_image`),
/// exercising `encode_array_body` / `read_array_body` (the null bitmap, the empty array, a NULL
/// array). The on-disk array body is version-independent (spec/design/array.md §4).
#[test]
fn array_image_roundtrip() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])",
    );
    run(&mut db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3], '{}')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL, NULL)");
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image).expect("load image");
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

/// A NULL element renders as the unquoted `NULL` token; a NULL array renders `NULL`; the three are
/// distinct (spec/design/array.md §1).
#[test]
fn array_null_levels() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])");
    run(&mut db, "INSERT INTO t VALUES (2, NULL)");
    run(&mut db, "INSERT INTO t VALUES (3, '{}')");
    assert_eq!(
        query(&mut db, "SELECT xs FROM t ORDER BY id"),
        vec![
            vec!["{1,NULL,3}".to_string()],
            vec!["NULL".to_string()],
            vec!["{}".to_string()],
        ]
    );
    // IS NULL tests the whole value only — a non-NULL array with NULL elements is NOT NULL.
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE xs IS NULL ORDER BY id"),
        vec![vec!["2".to_string()]]
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE xs IS NOT NULL ORDER BY id"),
        vec![vec!["1".to_string()], vec!["3".to_string()]]
    );
}

/// Array equality uses PG btree semantics, NOT 3VL: NULL elements are mutually equal, so the
/// comparison is always a definite boolean (spec/design/array.md §5).
#[test]
fn array_equality_btree_semantics() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])");
    run(&mut db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3])");
    run(&mut db, "INSERT INTO t VALUES (3, ARRAY[1, 2])");
    // exact match
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE xs = ARRAY[1,2,3]"),
        vec![vec!["1".to_string()]]
    );
    // {1,NULL,3} = {1,NULL,3} is TRUE (NULLs mutually equal — not UNKNOWN).
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE xs = ARRAY[1,NULL,3]"),
        vec![vec!["2".to_string()]]
    );
    // a shorter array is not equal to a longer one.
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE xs = ARRAY[1,2]"),
        vec![vec!["3".to_string()]]
    );
}

/// ORDER BY over an array column is element-wise with a shorter prefix sorting first and NULLs
/// last per element (the PG `array_cmp` total order — §5).
#[test]
fn array_order_by() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])");
    run(&mut db, "INSERT INTO t VALUES (2, ARRAY[1, 2])");
    run(&mut db, "INSERT INTO t VALUES (3, ARRAY[1, 3])");
    run(&mut db, "INSERT INTO t VALUES (4, ARRAY[1])");
    assert_eq!(
        query(&mut db, "SELECT xs FROM t ORDER BY xs"),
        vec![
            vec!["{1}".to_string()],
            vec!["{1,2}".to_string()],
            vec!["{1,2,3}".to_string()],
            vec!["{1,3}".to_string()],
        ]
    );
}

/// DISTINCT over arrays dedups by structural (btree) equality.
#[test]
fn array_distinct() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1, 2])");
    run(&mut db, "INSERT INTO t VALUES (2, ARRAY[1, 2])");
    run(&mut db, "INSERT INTO t VALUES (3, ARRAY[3])");
    let mut got = query(&mut db, "SELECT DISTINCT xs FROM t");
    got.sort();
    assert_eq!(
        got,
        vec![vec!["{1,2}".to_string()], vec!["{3}".to_string()]]
    );
}

/// `array_out` quotes an element that is empty, looks like `NULL`, or contains a delimiter, and
/// backslash-escapes `"`/`\` (the contrast with `record_out` doubling — §7).
#[test]
fn array_out_quoting() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, tags text[])",
    );
    run(
        &mut db,
        r#"INSERT INTO t VALUES (1, ARRAY['a,b', '', 'NULL', 'x"y'])"#,
    );
    assert_eq!(
        query(&mut db, "SELECT tags FROM t"),
        vec![vec![r#"{"a,b","","NULL","x\"y"}"#.to_string()]]
    );
}

/// An over-range element value still traps `22003` at store (the element store range-checks).
#[test]
fn array_element_overflow_is_22003() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int16[])");
    assert_eq!(
        err(&mut db, "INSERT INTO t VALUES (1, ARRAY[100000])"),
        "22003"
    );
}

/// An array `PRIMARY KEY` is rejected `0A000` — the key encoding is authored but unexercised (§8).
#[test]
fn array_primary_key_is_0a000() {
    let mut db = Database::new();
    assert_eq!(
        err(&mut db, "CREATE TABLE t (xs int32[] PRIMARY KEY)"),
        "0A000"
    );
}

/// A malformed array literal is `22P02`.
#[test]
fn malformed_array_literal_is_22p02() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    assert_eq!(err(&mut db, "INSERT INTO t VALUES (1, '{1,2')"), "22P02");
}

/// Comparing arrays of different element types is `42804`.
#[test]
fn array_cross_element_compare_is_42804() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], ts text[])",
    );
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1], ARRAY['a'])");
    assert_eq!(err(&mut db, "SELECT id FROM t WHERE xs = ts"), "42804");
}

/// S3: `a[i]` reads the i-th element, **1-based** (spec/design/array.md §6). The element type is the
/// column's element type; the un-aliased output column is named after the base array.
#[test]
fn subscript_is_one_based() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])",
    );
    assert_eq!(query(&mut db, "SELECT xs[1] FROM t"), vec![vec!["10"]]);
    assert_eq!(query(&mut db, "SELECT xs[3] FROM t"), vec![vec!["30"]]);
    assert_eq!(query(&mut db, "SELECT tags[2] FROM t"), vec![vec!["b"]]);
}

/// S3: an out-of-bounds subscript (0, negative, or past the end) yields NULL — never an error (PG).
#[test]
fn subscript_out_of_bounds_is_null() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])");
    assert_eq!(query(&mut db, "SELECT xs[0] FROM t"), vec![vec!["NULL"]]);
    assert_eq!(query(&mut db, "SELECT xs[4] FROM t"), vec![vec!["NULL"]]);
    assert_eq!(query(&mut db, "SELECT xs[-1] FROM t"), vec![vec!["NULL"]]);
}

/// S3: a NULL subscript and a subscript of a NULL array both yield NULL.
#[test]
fn subscript_null_index_or_null_array_is_null() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])");
    run(&mut db, "INSERT INTO t VALUES (2, NULL)");
    assert_eq!(
        query(&mut db, "SELECT xs[NULL] FROM t WHERE id = 1"),
        vec![vec!["NULL"]]
    );
    assert_eq!(
        query(&mut db, "SELECT xs[1] FROM t WHERE id = 2"),
        vec![vec!["NULL"]]
    );
}

/// S3: a subscript reading a NULL *element* yields NULL (distinct from out-of-bounds, same render).
#[test]
fn subscript_null_element() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])");
    assert_eq!(query(&mut db, "SELECT xs[2] FROM t"), vec![vec!["NULL"]]);
    assert_eq!(query(&mut db, "SELECT xs[3] FROM t"), vec![vec!["3"]]);
}

/// S3: subscripting a non-array base is `42804` at resolve.
#[test]
fn subscript_non_array_is_42804() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)");
    run(&mut db, "INSERT INTO t VALUES (1, 5)");
    assert_eq!(err(&mut db, "SELECT n[1] FROM t"), "42804");
}

/// S3: the subscript can be an arbitrary integer expression, and subscripting an `ARRAY[…]`
/// constructor works directly (`(ARRAY[…])[i]`).
#[test]
fn subscript_expression_index_and_constructor_base() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
    run(&mut db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])");
    assert_eq!(query(&mut db, "SELECT xs[1 + 1] FROM t"), vec![vec!["20"]]);
    assert_eq!(
        query(&mut db, "SELECT (ARRAY[100, 200, 300])[3] FROM t"),
        vec![vec!["300"]]
    );
}
