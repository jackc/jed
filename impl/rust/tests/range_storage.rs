//! Range storage (spec/design/ranges.md, R2–R4) — the divergences + introspection the oracle corpus
//! cannot express (CLAUDE.md §10): the deliberate `0A000` narrowings PostgreSQL does NOT share that
//! remain after range became a key (a range DEFAULT and INSERT…SELECT into a range column — PG accepts
//! the DEFAULT outright), the jed-canonical `i32range` spelling (PG reports `int4range`), the
//! cross-element comparison code (jed's uniform `42804` where PG reports `42883`), and the
//! whole-image store/load round-trip of a range column (the byte layout is pinned cross-core by
//! range_table.jed; this is the behavioral check). A range PRIMARY KEY / ordered index / `UNIQUE` / FK
//! now WORK (range-PK slice, R4 — PG also allows them via its range btree opclass), so they live
//! oracle-clean in types/range.test; the byte-exact key is pinned by range_pk_table.jed +
//! tests/range_key.rs (encoding.md §2.11). The agreeing behavior — render, canonicalization,
//! `IS NULL`, the range_cmp total order (=/</ORDER BY/DISTINCT), 22000/22P02/22003/42704 — lives in
//! types/range.test (oracle-clean), not here.

use jed::{Engine, Outcome, execute};

fn run(db: &mut Engine, sql: &str) {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Engine, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

fn query(db: &mut Engine, sql: &str) -> Vec<Vec<String>> {
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
    let mut db = Engine::new();
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
    let mut loaded = Engine::from_image(&image).expect("load image");
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
    let mut db = Engine::new();
    // The PG alias is accepted on the column; the value renders the same as the canonical spelling.
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r int4range)");
    run(&mut db, "INSERT INTO t VALUES (1, '[1,5)')");
    assert_eq!(query(&mut db, "SELECT r FROM t"), vec![vec!["[1,5)"]]);
    // A range PRIMARY KEY now WORKS even when declared with the PG alias, and the value behaves as a
    // key: `[1,4]` canonicalizes to `[1,5)`, so re-inserting it is a duplicate key (23505). Range is
    // keyable since R4 (encoding.md §2.11).
    let mut db2 = Engine::new();
    run(&mut db2, "CREATE TABLE k (r int4range PRIMARY KEY, n i32)");
    run(&mut db2, "INSERT INTO k VALUES ('[1,5)', 1)");
    assert_eq!(err(&mut db2, "INSERT INTO k VALUES ('[1,4]', 2)"), "23505");
    // A still-rejected path reports the canonical i32range even when declared with the alias: GIN needs
    // an array/jsonb opclass, so GIN over a plain range column is 42704 and names the canonical type
    // (PG agrees a range has no gin opclass but reports int4range — the naming divergence, per-core).
    let mut db3 = Engine::new();
    run(&mut db3, "CREATE TABLE u (id i32 PRIMARY KEY, r int4range)");
    let msg = execute(&mut db3, "CREATE INDEX ON u USING gin (r)")
        .expect_err("a gin index over a plain range column is rejected")
        .message;
    assert!(msg.contains("i32range"), "message names i32range: {msg}");
}

/// The staged `0A000` narrowings PostgreSQL does NOT share that REMAIN after range became a key (R4):
/// a range DEFAULT and INSERT…SELECT into a range column (PG accepts a range DEFAULT outright —
/// spec/design/ranges.md §8). A range PRIMARY KEY / ordered index / `UNIQUE` now work (oracle-clean,
/// types/range.test) — PG also allows them via its range btree opclass. These remaining cases are
/// jed-stricter, so they cannot live in the oracle-clean corpus.
#[test]
fn range_narrowings_are_0a000() {
    let mut db = Engine::new();
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE b (id i32 PRIMARY KEY, r i32range DEFAULT '[1,5)')",
        ),
        "0A000",
    );
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    // A range ordered (btree) index now WORKS (the range-bounds key, encoding.md §2.11) — a positive
    // check that the former 0A000 narrowing is lifted.
    run(&mut db, "CREATE INDEX ri ON t (r)");
    // INSERT … SELECT into a range column is deferred (the VALUES + literal path is the input).
    run(&mut db, "CREATE TABLE src (id i32 PRIMARY KEY, r i32range)");
    run(&mut db, "INSERT INTO src VALUES (1, '[1,5)')");
    assert_eq!(err(&mut db, "INSERT INTO t SELECT id, r FROM src"), "0A000",);
}

/// Updating a range COLUMN works (ranges.md §4, oracle-clean in types/range.test) but three sub-cases
/// stay `0A000` — PG supports them, so they are jed-stricter and cannot live in the oracle corpus: a
/// `$N` parameter into a range column, the ON CONFLICT DO UPDATE conflict-action path, and a composite
/// column (a separate slice). The happy-path forms (literal / cast / constructor / set-op / NULL /
/// re-key) and the 42804 type errors live in types/range.test.
#[test]
fn range_update_deferrals_are_0a000() {
    let mut db = Engine::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)");
    run(&mut db, "INSERT INTO t VALUES (1, '[1,5)')");
    assert_eq!(err(&mut db, "UPDATE t SET r = $1 WHERE id = 1"), "0A000");
    assert_eq!(
        err(
            &mut db,
            "INSERT INTO t VALUES (1, '[2,6)') ON CONFLICT (id) DO UPDATE SET r = '[9,10)'",
        ),
        "0A000",
    );
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TABLE p (id i32 PRIMARY KEY, a addr)");
    run(&mut db, "INSERT INTO p VALUES (1, ROW('x', 5))");
    assert_eq!(
        err(&mut db, "UPDATE p SET a = ROW('y', 9) WHERE id = 1"),
        "0A000"
    );
}

/// Range comparison (R3) is restricted to the SAME element type (spec/design/ranges.md §6): a range
/// is comparable only to a range over an equal element, never to a different-element range or to a
/// bare scalar. jed reports its uniform comparison-mismatch code `42804`; PostgreSQL reports `42883`
/// ("operator does not exist") — a deliberate divergence, so this cannot live in the oracle corpus.
/// The agreeing same-element comparison (=/</ORDER BY) is covered by types/range.test.
#[test]
fn cross_element_comparison_is_42804() {
    let mut db = Engine::new();
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
    let mut db = Engine::new();
    assert_eq!(
        err(&mut db, "CREATE TYPE rec AS (lo i32, span i32range)"),
        "0A000",
    );
}

/// The range CONSTRUCTORS (RF2) under jed's own spellings + assignment-style bound coercion — the two
/// places jed diverges from PG's strict function-argument matching (spec/design/range-functions.md
/// §2), which the oracle corpus (PG-clean) cannot express. The agreeing constructor behavior —
/// default `[)`, explicit bounds, NULL→infinite, canonicalize/empty, 22000/42601/22003 — lives in
/// expr/range_constructors.test.
#[test]
fn range_constructor_divergences() {
    let mut db = Engine::new();
    // (1) jed ACCEPTS the i/f-prefix spellings i32range/i64range as constructor names (PG ships only
    // int4range/int8range). The result is identical to the PG-spelled alias.
    assert_eq!(query(&mut db, "SELECT i32range(1, 5)"), vec![vec!["[1,5)"]]);
    assert_eq!(
        query(&mut db, "SELECT i64range(100, 200, '[]')"),
        vec![vec!["[100,201)"]],
    );
    // (2) jed accepts a WIDER integer for a narrower range and range-checks at eval — PG rejects the
    // int4range(bigint, …) overload outright (42883). A value that fits is built; one that overflows
    // the element domain is 22003 (the same assignment range-check INSERT applies).
    assert_eq!(
        query(&mut db, "SELECT int4range(1::i64, 5::i64)"),
        vec![vec!["[1,5)"]],
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT int4range(3000000000::i64, 4000000000::i64)"
        ),
        "22003",
    );
    // (3) Conversely jed is STRICTER on the unknown-literal corner: a string literal is NOT a valid
    // integer/decimal bound (no unknown→number coercion), so it is 42883 — where PG coerces '1' to
    // integer. (A string DOES adapt to a temporal element, exercised in the corpus.)
    assert_eq!(err(&mut db, "SELECT int4range('1', 5)"), "42883");
    assert_eq!(err(&mut db, "SELECT numrange('1', 2)"), "42883");
    // Arity: only the 2-arg and 3-arg forms exist; anything else is no overload.
    assert_eq!(err(&mut db, "SELECT int4range(1)"), "42883");
    assert_eq!(err(&mut db, "SELECT int4range(1, 2, '[]', 3)"), "42883");
}

/// The range BOOLEAN operators (RF3) — the error cases the oracle corpus (which only carries
/// value-producing rows) cannot express, plus the one real divergence (spec/design/range-functions.md
/// §3). The agreeing value behavior of all eight operators lives in expr/range_operators.test.
#[test]
fn range_operator_divergences() {
    let mut db = Engine::new();
    // THE divergence: jed has no integer bit-shift, so the `<<` / `>>` tokens are RANGE-only. An
    // integer `<<` / `>>` is "operator does not exist" (42883) — PostgreSQL would compute a bit shift
    // (5 << 2 = 20). A documented divergence (jed owns its surface), so it cannot live in the corpus.
    assert_eq!(err(&mut db, "SELECT 5 << 2"), "42883");
    assert_eq!(err(&mut db, "SELECT 5 >> 2"), "42883");
    // A range operator pairs only with a range over the SAME element type (this AGREES with PG's
    // "operator does not exist" 42883, but an error row is awkward in the value-oriented corpus).
    assert_eq!(
        err(&mut db, "SELECT '[1,5)'::int4range @> '[1,5)'::int8range"),
        "42883",
    );
    assert_eq!(
        err(&mut db, "SELECT '[1,5)'::int4range && '[1,5)'::int8range"),
        "42883",
    );
    // The positional operators have no element overload — `range << element` is 42883 (only @>/<@
    // take an element). And `-|-` on non-ranges is 42883 (it is range-only, like PG).
    assert_eq!(err(&mut db, "SELECT '[1,5)'::int4range << 5"), "42883");
    assert_eq!(err(&mut db, "SELECT 1 -|- 2"), "42883");
    // `-|-` lexes greedily and is NOT confused with `-` then a comment / minus: this is the adjacency
    // operator over two ranges (true here), proving the token won the `--` race.
    assert_eq!(
        query(&mut db, "SELECT '[1,5)'::int4range -|- '[5,9)'::int4range"),
        vec![vec!["true"]],
    );
}
