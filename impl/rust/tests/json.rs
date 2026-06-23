//! Storable `json` / `jsonb` columns (spec/design/json.md, slices J1/J1b) — the per-core checks
//! the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (a
//! json/jsonb PRIMARY KEY / index / UNIQUE is `0A000` where PG allows a jsonb key) and the on-disk
//! internals (a large json/jsonb document spills out-of-line and round-trips through a
//! serialize + reload). The agreeing behavior (store + canonical/verbatim round-trip, NULL) lives
//! in suites/json/json_storage.test.

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

/// A `jsonb` PRIMARY KEY is `0A000` — the order-preserving jsonb key (encoding.md §2.13) is authored
/// but unexercised this slice (the staged-key narrowing text/decimal/bytea/array carried). PG ALLOWS
/// a jsonb PK (it has a jsonb btree opclass), so this is a documented divergence.
#[test]
fn jsonb_primary_key_is_unsupported() {
    let mut db = Database::new();
    assert_eq!(
        err(&mut db, "CREATE TABLE t (k jsonb PRIMARY KEY)"),
        "0A000"
    );
}

/// A `json` PRIMARY KEY is `0A000` — `json` is never keyable (it is not even comparable; PG ships no
/// json opclass at all, so PG rejects it too, but with its own undefined-function shape).
#[test]
fn json_primary_key_is_unsupported() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "CREATE TABLE t (k json PRIMARY KEY)"), "0A000");
}

/// A jsonb secondary index / UNIQUE is likewise `0A000` (no key encoding exercised yet).
#[test]
fn jsonb_index_and_unique_are_unsupported() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    assert_eq!(err(&mut db, "CREATE INDEX i ON t (j)"), "0A000");
    let mut db2 = Database::new();
    assert_eq!(
        err(
            &mut db2,
            "CREATE TABLE u (id i32 PRIMARY KEY, j jsonb UNIQUE)"
        ),
        "0A000"
    );
}

/// A `jsonb` comparison with a NON-jsonb family is `42804` (jed's cross-family convention, like
/// uuid/bytea/range) — a documented divergence from PostgreSQL, which reports `42883` (operator
/// does not exist: jsonb = integer). The agreeing json-non-comparable behavior (always 42883) and
/// jsonb × jsonb ordering live in suites/json/json_compare.test.
#[test]
fn jsonb_cross_family_comparison_is_42804() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, b jsonb)");
    // jsonb vs an integer / a real text value (not an adaptable string literal): 42804.
    assert_eq!(err(&mut db, "SELECT id FROM t WHERE b = 5"), "42804");
    assert_eq!(
        err(&mut db, "SELECT id FROM t WHERE b = 'x'::text"),
        "42804"
    );
}

/// Casting a non-text/json/jsonb source to json/jsonb is `42804` (jed's invalid-cast convention,
/// like "cannot cast boolean to X") — a documented divergence from PostgreSQL, which reports
/// `42846` (cannot_coerce: cannot cast type integer to jsonb). The supported JSON cast matrix
/// (json↔jsonb, json/jsonb→text, text→json/jsonb) is oracle-clean in suites/json/json_casts.test.
#[test]
fn invalid_json_cast_source_is_42804() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "SELECT 5::jsonb"), "42804");
    assert_eq!(err(&mut db, "SELECT (1.5)::json"), "42804");
    assert_eq!(err(&mut db, "SELECT true::jsonb"), "42804");
}

/// The `json` overloads of the accessor operators (`-> ->> #> #>>`) are a deferred `0A000`
/// follow-on — they would have to preserve the verbatim sub-text (json.md §4), unlike the jsonb
/// operators that work over the canonical node tree. PostgreSQL supports them, so this is a
/// documented divergence (the jsonb operators are oracle-clean in suites/json/json_access.test).
#[test]
fn json_accessor_operators_are_deferred() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)");
    run(&mut db, "INSERT INTO t VALUES (1, '{\"a\":1}')");
    assert_eq!(err(&mut db, "SELECT j -> 'a' FROM t"), "0A000");
    assert_eq!(err(&mut db, "SELECT j ->> 'a' FROM t"), "0A000");
    assert_eq!(err(&mut db, "SELECT j #> '{a}' FROM t"), "0A000");
}

/// `jsonb_pretty` renders the PG indented multi-line form (4-space indent, one space after `:`, a
/// container ALWAYS multi-lines — an empty `{}` is `{` newline `}`). Pinned against the postgres:18
/// oracle; the multi-line output can't live in the line-based corpus.
#[test]
fn jsonb_pretty_matches_pg() {
    let mut db = Database::new();
    let q = |db: &mut Database, sql: &str| -> String {
        match execute(db, sql).unwrap() {
            Outcome::Query { rows, .. } => rows[0][0].render(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        q(
            &mut db,
            "SELECT jsonb_pretty('{\"a\":1,\"b\":[1,2]}'::jsonb)"
        ),
        "{\n    \"a\": 1,\n    \"b\": [\n        1,\n        2\n    ]\n}"
    );
    // An empty object/array still multi-lines (PG): `{` newline (indent) `}`.
    assert_eq!(q(&mut db, "SELECT jsonb_pretty('{}'::jsonb)"), "{\n}");
    assert_eq!(
        q(&mut db, "SELECT jsonb_pretty('{\"a\":{},\"b\":[]}'::jsonb)"),
        "{\n    \"a\": {\n    },\n    \"b\": [\n    ]\n}"
    );
}

/// A large `jsonb` document (a long string node well past `RECORD_MAX`) spills onto an overflow
/// chain and round-trips through a whole-image serialize + reload — exercising `is_spillable`,
/// `value_payload`, and `value_from_payload` for the jsonb body (the tree decoded from a fresh
/// cursor off the gathered chain). The rendered canonical form is preserved exactly.
#[test]
fn large_jsonb_spills_and_round_trips() {
    let mut db = Database::with_page_size(4096);
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    // A ~6000-byte string node — far above RECORD_MAX (~2034 at page 4096) — forces a spill.
    let big = "a".repeat(6000);
    run(&mut db, &format!("INSERT INTO t VALUES (1, '\"{big}\"')"));
    // A second row with a small value, so the table spans the spilled + inline cases.
    run(&mut db, "INSERT INTO t VALUES (2, '{\"k\": 42}')");

    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image).expect("load image");

    let rows = query(&mut loaded, "SELECT id, j FROM t ORDER BY id");
    assert_eq!(rows[0][0], "1");
    assert_eq!(rows[0][1], format!("\"{big}\"")); // the canonical render of the big string node
    assert_eq!(rows[1], vec!["2".to_string(), "{\"k\": 42}".to_string()]);
}

/// A large verbatim `json` document spills and round-trips, preserving the input bytes EXACTLY
/// (insignificant whitespace included — the json verbatim contract, §4).
#[test]
fn large_json_spills_verbatim() {
    let mut db = Database::with_page_size(4096);
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)");
    // Verbatim text with irregular internal spacing, padded past RECORD_MAX.
    let pad = " ".repeat(6000);
    let verbatim = format!("{{ \"a\" :{pad}1 }}");
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (1, '{}')",
            verbatim.replace('\'', "''")
        ),
    );

    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image).expect("load image");
    let rows = query(&mut loaded, "SELECT j FROM t WHERE id = 1");
    assert_eq!(rows[0][0], verbatim); // verbatim bytes, whitespace preserved
}

/// A `jsonb` column round-trips every node kind (object/array/number/string/bool/null) through a
/// serialize + reload, confirming the tagged-node value codec decodes back to the canonical render.
#[test]
fn jsonb_all_node_kinds_round_trip() {
    let mut db = Database::with_page_size(4096);
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}')",
    );
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image).expect("load image");
    let rows = query(&mut loaded, "SELECT j FROM t WHERE id = 1");
    assert_eq!(
        rows[0][0],
        "{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}"
    );
}
