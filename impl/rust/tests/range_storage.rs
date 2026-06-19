//! Range storage (spec/design/ranges.md, R2–R3) — the divergences + introspection the oracle corpus
//! cannot express (CLAUDE.md §10): the deliberate `0A000` narrowings PostgreSQL does NOT share (a
//! range PRIMARY KEY / DEFAULT / index — PG allows them via its btree/GiST opclasses), the
//! jed-canonical `i32range` spelling (PG reports `int4range`), INSERT…SELECT deferral, the
//! cross-element comparison code (jed's uniform `42804` where PG reports `42883`), and the
//! whole-image store/load round-trip of a range column (the byte layout is pinned cross-core by
//! range_table.jed; this is the behavioral check). The agreeing behavior — render, canonicalization,
//! `IS NULL`, the range_cmp total order (=/</ORDER BY/DISTINCT), 22000/22P02/22003/42704 — lives in
//! types/range.test (oracle-clean), not here.

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

/// A range column survives a whole-image serialize + reload (`to_image` → `from_image`), exercising
/// `encode_range_body` / `read_range_body` (the empty range, infinite bounds, a NULL range, the
/// canonical `[)` storage). The on-disk byte layout is pinned cross-core by range_table.jed; this is
/// the behavioral round-trip.
#[test]
fn range_image_roundtrip() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')");
    run(&mut db, "INSERT INTO t VALUES (2, '[1,5]', NULL)"); // canonical [1,6)
    run(&mut db, "INSERT INTO t VALUES (3, 'empty', '(,100)')");
    run(&mut db, "INSERT INTO t VALUES (4, '(,)', '(5,)')"); // canonical [6,)
    run(&mut db, "INSERT INTO t VALUES (5, NULL, '[1,1]')"); // canonical [1,2)
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image).expect("load image");
    assert_eq!(
        query(&mut loaded, "SELECT id, r, br FROM t ORDER BY id"),
        vec![
            vec!["1", "[1,5)", "[10,20)"],
            vec!["2", "[1,6)", "NULL"],
            vec!["3", "empty", "(,100)"],
            vec!["4", "(,)", "[6,)"],
            vec!["5", "NULL", "[1,2)"],
        ],
    );
}

/// The jed-canonical name is `i32range` (PG reports `int4range`), and `int4range`/`int8range` are
/// accepted as aliases (the i/f-prefix rename — CLAUDE.md §4). The PG alias declares a column whose
/// stored value renders identically to the canonical spelling, and the canonical name (not the PG
/// `int4range`) appears in a jed message.
#[test]
fn canonical_name_and_aliases() {
    let mut db = Database::new();
    // The PG alias is accepted on the column; the value renders the same as the canonical spelling.
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r int4range)");
    run(&mut db, "INSERT INTO t VALUES (1, '[1,5)')");
    assert_eq!(query(&mut db, "SELECT r FROM t"), vec![vec!["[1,5)"]]);
    // The canonical name appears in the 0A000 PK-narrowing message (canonical_name()), even though
    // the column was declared with the PG alias int4range.
    let mut db2 = Database::new();
    let msg = execute(&mut db2, "CREATE TABLE u (r int4range PRIMARY KEY)")
        .expect_err("a range primary key is rejected")
        .message;
    assert!(msg.contains("i32range"), "message names i32range: {msg}");
}

/// The staged `0A000` narrowings PostgreSQL does NOT share: a range PRIMARY KEY, a range DEFAULT, a
/// range index, and INSERT…SELECT into a range column (PG accepts a range key via its default btree
/// opclass and a range DEFAULT outright — spec/design/ranges.md §8). These are jed-stricter, so they
/// cannot live in the oracle-clean corpus.
#[test]
fn range_narrowings_are_0a000() {
    let mut db = Database::new();
    assert_eq!(
        err(&mut db, "CREATE TABLE a (r i32range PRIMARY KEY)"),
        "0A000",
    );
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE b (id i32 PRIMARY KEY, r i32range DEFAULT '[1,5)')",
        ),
        "0A000",
    );
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    // A range index needs a GiST opclass jed does not ship (§8/§10).
    assert_eq!(err(&mut db, "CREATE INDEX ri ON t (r)"), "0A000");
    // INSERT … SELECT into a range column is deferred (the VALUES + literal path is the input).
    run(&mut db, "CREATE TABLE src (id i32 PRIMARY KEY, r i32range)");
    run(&mut db, "INSERT INTO src VALUES (1, '[1,5)')");
    assert_eq!(err(&mut db, "INSERT INTO t SELECT id, r FROM src"), "0A000",);
}

/// Range comparison (R3) is restricted to the SAME element type (spec/design/ranges.md §6): a range
/// is comparable only to a range over an equal element, never to a different-element range or to a
/// bare scalar. jed reports its uniform comparison-mismatch code `42804`; PostgreSQL reports `42883`
/// ("operator does not exist") — a deliberate divergence, so this cannot live in the oracle corpus.
/// The agreeing same-element comparison (=/</ORDER BY) is covered by types/range.test.
#[test]
fn cross_element_comparison_is_42804() {
    let mut db = Database::new();
    // A range over i32 vs a range over i64 — different element types, no implicit cross-range cast.
    assert_eq!(
        err(&mut db, "SELECT '[1,5)'::i32range = '[1,5)'::i64range"),
        "42804",
    );
    assert_eq!(
        err(&mut db, "SELECT '[1,5)'::i32range < '[1,5)'::i64range"),
        "42804",
    );
    // A range vs a bare scalar of its own element type is still a 42804 (a range is not its element).
    assert_eq!(err(&mut db, "SELECT '[1,5)'::i32range = 5"), "42804");
}

/// A range-typed composite field is deferred (`0A000`) — only range *columns* are storable this
/// slice. The type name IS known, so it is `0A000`, not the `42704` an unknown type would give.
#[test]
fn composite_range_field_is_0a000() {
    let mut db = Database::new();
    assert_eq!(
        err(&mut db, "CREATE TYPE rec AS (lo i32, span i32range)"),
        "0A000",
    );
}
