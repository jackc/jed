//! Storable `json` / `jsonb` columns (spec/design/json.md, slices J1/J1b) — the per-core checks
//! the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (a
//! json/jsonb PRIMARY KEY / index / UNIQUE is `0A000` where PG allows a jsonb key) and the on-disk
//! internals (a large json/jsonb document spills out-of-line and round-trips through a
//! serialize + reload). The agreeing behavior (store + canonical/verbatim round-trip, NULL) lives
//! in suites/json/json_storage.test.

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
}

fn err(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

fn query(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match db
        .query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
    {
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "CREATE TABLE t (k jsonb PRIMARY KEY)"),
        "0A000"
    );
}

/// A `json` PRIMARY KEY is `0A000` — `json` is never keyable (it is not even comparable; PG ships no
/// json opclass at all, so PG rejects it too, but with its own undefined-function shape).
#[test]
fn json_primary_key_is_unsupported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(err(&mut db, "CREATE TABLE t (k json PRIMARY KEY)"), "0A000");
}

/// A jsonb secondary index / UNIQUE is likewise `0A000` (no key encoding exercised yet).
#[test]
fn jsonb_index_and_unique_are_unsupported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    assert_eq!(err(&mut db, "CREATE INDEX i ON t (j)"), "0A000");
    let mut db2 = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(err(&mut db, "SELECT 5::jsonb"), "42804");
    assert_eq!(err(&mut db, "SELECT (1.5)::json"), "42804");
    assert_eq!(err(&mut db, "SELECT true::jsonb"), "42804");
}

/// `jsonb_pretty` renders the PG indented multi-line form (4-space indent, one space after `:`, a
/// container ALWAYS multi-lines — an empty `{}` is `{` newline `}`). Pinned against the postgres:18
/// oracle; the multi-line output can't live in the line-based corpus.
#[test]
fn jsonb_pretty_matches_pg() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let q = |db: &mut Session, sql: &str| -> String {
        match db.query_outcome(sql, &[]).unwrap() {
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
    let mut db = Database::create(CreateOptions {
        page_size: 4096,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    // A ~6000-byte string node — far above RECORD_MAX (~2034 at page 4096) — forces a spill.
    let big = "a".repeat(6000);
    run(&mut db, &format!("INSERT INTO t VALUES (1, '\"{big}\"')"));
    // A second row with a small value, so the table spans the spilled + inline cases.
    run(&mut db, "INSERT INTO t VALUES (2, '{\"k\": 42}')");

    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image)
        .expect("load image")
        .session(SessionOptions::default());

    let rows = query(&mut loaded, "SELECT id, j FROM t ORDER BY id");
    assert_eq!(rows[0][0], "1");
    assert_eq!(rows[0][1], format!("\"{big}\"")); // the canonical render of the big string node
    assert_eq!(rows[1], vec!["2".to_string(), "{\"k\": 42}".to_string()]);
}

/// A large verbatim `json` document spills and round-trips, preserving the input bytes EXACTLY
/// (insignificant whitespace included — the json verbatim contract, §4).
#[test]
fn large_json_spills_verbatim() {
    let mut db = Database::create(CreateOptions {
        page_size: 4096,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
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
    let mut loaded = Database::from_image(&image)
        .expect("load image")
        .session(SessionOptions::default());
    let rows = query(&mut loaded, "SELECT j FROM t WHERE id = 1");
    assert_eq!(rows[0][0], verbatim); // verbatim bytes, whitespace preserved
}

/// `JSON_SERIALIZE` over a `jsonb` value renders its canonical text — a documented divergence from
/// PostgreSQL 18, which returns SQL NULL for a jsonb input (a PG quirk; only `json` input serializes).
/// The json-input behavior is oracle-clean in suites/json/json_ctor.test.
#[test]
fn json_serialize_jsonb_diverges_from_pg() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        query(&mut db, "SELECT JSON_SERIALIZE('{\"b\":2,\"a\":1}'::jsonb)")[0][0],
        "{\"a\": 1, \"b\": 2}" // jed: the jsonb canonical text; PG 18: NULL
    );
}

/// `json[b]_populate_record` over a NON-composite base argument is `42804` (jed's datatype-mismatch
/// convention); a composite whose field is an array/nested composite is `0A000` (the same
/// coerce-to-field deferral as the record functions). PostgreSQL handles both, so these are
/// documented divergences; the scalar-field behavior is oracle-clean in suites/json/json_populate.test.
#[test]
fn json_populate_non_composite_and_complex_field_divergences() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    run(&mut db, "CREATE TYPE poly AS (name text, pts i32[])");
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM jsonb_populate_record(NULL::i32, '{\"a\":1}')"
        ),
        "42804"
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM jsonb_populate_record(NULL::poly, '{\"name\":\"x\",\"pts\":[1,2]}')"
        ),
        "0A000"
    );
}

/// A composite or array COLUMN in a record function's column-definition list is a deferred `0A000`
/// (only scalar / json / jsonb columns coerce this slice). PostgreSQL supports them, so this is a
/// documented divergence; the scalar columns are oracle-clean in suites/json/json_record.test.
#[test]
fn json_record_composite_array_column_is_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TYPE addr AS (street text, zip i32)");
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM jsonb_to_record('{\"a\":1}') AS t(a addr)"
        ),
        "0A000"
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM jsonb_to_record('{\"a\":1}') AS t(a i32[])"
        ),
        "0A000"
    );
}

/// A `jsonb` column round-trips every node kind (object/array/number/string/bool/null) through a
/// serialize + reload, confirming the tagged-node value codec decodes back to the canonical render.
/// The deferred S2 sub-clauses of the SQL/JSON query functions are `0A000` — ON ERROR/EMPTY
/// DEFAULT expr, JSON_QUERY OMIT QUOTES, and JSON_QUERY RETURNING a non-json type.
/// PostgreSQL supports all of these, so each is a documented divergence; the supported subset is
/// oracle-clean in suites/json/json_query_fns.test.
#[test]
fn json_query_fn_deferred_clauses_are_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "SELECT JSON_VALUE('{\"a\":1}', '$.b' DEFAULT 'z' ON EMPTY)",
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' OMIT QUOTES)",
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' RETURNING int)",
    ] {
        assert_eq!(err(&mut db, sql), "0A000", "{sql} should defer 0A000");
    }
}

/// The deferred T1 sub-features of JSON_TABLE are `0A000` — an explicit PLAN, an array
/// column, a WRAPPER on a scalar column, OMIT QUOTES; an unknown column type is `42704`. PostgreSQL
/// supports the first set, so each is a documented divergence; the supported subset is oracle-clean
/// in suites/json/json_table.test.
#[test]
fn json_table_deferred_features_are_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x') PLAN DEFAULT (x))",
        "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32[] PATH '$.x'))",
        "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' WITH WRAPPER))",
        "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' OMIT QUOTES))",
    ] {
        assert_eq!(err(&mut db, sql), "0A000", "{sql} should defer 0A000");
    }
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x nosuchtype PATH '$.x'))"
        ),
        "42704"
    );
}

#[test]
fn jsonb_all_node_kinds_round_trip() {
    let mut db = Database::create(CreateOptions {
        page_size: 4096,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}')",
    );
    let image = db.to_image(4096, 1).expect("serialize image");
    let mut loaded = Database::from_image(&image)
        .expect("load image")
        .session(SessionOptions::default());
    let rows = query(&mut loaded, "SELECT j FROM t WHERE id = 1");
    assert_eq!(
        rows[0][0],
        "{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}"
    );
}
