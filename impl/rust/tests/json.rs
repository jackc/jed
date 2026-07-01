//! Storable `json` / `jsonb` columns (spec/design/json.md, slices J1/J1b) — the per-core checks
//! the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (a
//! json/jsonb PRIMARY KEY / index / UNIQUE is `0A000` where PG allows a jsonb key) and the on-disk
//! internals (a large json/jsonb document spills out-of-line and round-trips through a
//! serialize + reload). The agreeing behavior (store + canonical/verbatim round-trip, NULL) lives
//! in suites/json/json_storage.test.

use jed::{Database, Outcome, Session, SessionOptions};

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

/// A `jsonb` PRIMARY KEY is `0A000` — the order-preserving jsonb key (encoding.md §2.13) is authored
/// but unexercised this slice (the staged-key narrowing text/decimal/bytea/array carried). PG ALLOWS
/// a jsonb PK (it has a jsonb btree opclass), so this is a documented divergence.
#[test]
fn jsonb_primary_key_is_unsupported() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "CREATE TABLE t (k jsonb PRIMARY KEY)"),
        "0A000"
    );
}

/// A `json` PRIMARY KEY is `0A000` — `json` is never keyable (it is not even comparable; PG ships no
/// json opclass at all, so PG rejects it too, but with its own undefined-function shape).
#[test]
fn json_primary_key_is_unsupported() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(err(&mut db, "CREATE TABLE t (k json PRIMARY KEY)"), "0A000");
}

/// A jsonb secondary index / UNIQUE is likewise `0A000` (no key encoding exercised yet).
#[test]
fn jsonb_index_and_unique_are_unsupported() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)");
    assert_eq!(err(&mut db, "CREATE INDEX i ON t (j)"), "0A000");
    let mut db2 = Database::new_in_memory().session(SessionOptions::default());
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
    let mut db = Database::new_in_memory().session(SessionOptions::default());
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
    let mut db = Database::new_in_memory().session(SessionOptions::default());
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
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)");
    run(&mut db, "INSERT INTO t VALUES (1, '{\"a\":1}')");
    assert_eq!(err(&mut db, "SELECT j -> 'a' FROM t"), "0A000");
    assert_eq!(err(&mut db, "SELECT j ->> 'a' FROM t"), "0A000");
    assert_eq!(err(&mut db, "SELECT j #> '{a}' FROM t"), "0A000");
}

/// The `json` set-returning variants `json_array_elements` / `json_array_elements_text` are a
/// deferred `0A000` follow-on (they would have to preserve the verbatim element sub-text — json.md
/// §4); the jsonb variants + `json_object_keys` are oracle-clean in suites/json/json_srf.test.
#[test]
fn json_array_elements_srf_is_deferred() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "SELECT * FROM json_array_elements('[1,2]'::json)"),
        "0A000"
    );
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM json_array_elements_text('[1,2]'::json)"
        ),
        "0A000"
    );
}

/// `to_jsonb` over the type-info-dependent / float-divergent sources (float, composite, datetime,
/// uuid, bytea, interval, multidim array) is a deferred `0A000` follow-on; the supported set
/// (scalars/jsonb/json/1-D arrays) is oracle-clean in suites/json/json_to_jsonb.test.
#[test]
fn to_jsonb_unsupported_sources_are_deferred() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(err(&mut db, "SELECT to_jsonb(1.5::f64)"), "0A000");
    assert_eq!(err(&mut db, "SELECT to_jsonb('2020-01-01'::date)"), "0A000");
    assert_eq!(
        err(&mut db, "SELECT to_jsonb(ARRAY[ARRAY[1,2],ARRAY[3,4]])"),
        "0A000"
    );
}

/// `jsonb_pretty` renders the PG indented multi-line form (4-space indent, one space after `:`, a
/// container ALWAYS multi-lines — an empty `{}` is `{` newline `}`). Pinned against the postgres:18
/// oracle; the multi-line output can't live in the line-based corpus.
#[test]
fn jsonb_pretty_matches_pg() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    let q = |db: &mut Session, sql: &str| -> String {
        match db.execute(sql, &[]).unwrap() {
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
    let mut db = Database::new_in_memory_with_page_size(4096).session(SessionOptions::default());
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
    let mut db = Database::new_in_memory_with_page_size(4096).session(SessionOptions::default());
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

/// A NULL element inside the `jsonb_set` / `jsonb_insert` path array propagates a SQL NULL result —
/// a documented divergence from PostgreSQL, which raises `22004` ("path element at position N is
/// null"). jed treats the path strictly, like the `#-` delete-path operator's text[] handling. The
/// agreeing behavior (set/insert/no-op/22023/22P02) is oracle-clean in suites/json/json_set.test.
#[test]
fn jsonb_set_null_path_element_propagates_null() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        query(
            &mut db,
            "SELECT jsonb_set('{\"a\":1}', ARRAY['a', NULL], '99')"
        )[0][0],
        "NULL"
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT jsonb_insert('{\"a\":1}', ARRAY[NULL], '99')"
        )[0][0],
        "NULL"
    );
}

/// The `json` two-column SRFs `json_each` / `json_each_text` are a deferred `0A000` follow-on (they
/// would have to preserve the verbatim member sub-text — json.md §4); the jsonb variants are
/// oracle-clean in suites/json/json_each.test. PostgreSQL supports the json variants, so this is a
/// documented divergence (the json_array_elements precedent).
#[test]
fn json_each_srf_is_deferred() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "SELECT * FROM json_each('{\"a\":1}'::json)"),
        "0A000"
    );
    assert_eq!(
        err(&mut db, "SELECT * FROM json_each_text('{\"a\":1}'::json)"),
        "0A000"
    );
}

/// The json/jsonb construction builders (to_json / json[b]_build_array / _object) reuse the
/// `to_jsonb` element kernel, so a deferred-source element (float, like to_jsonb) is `0A000`. PG
/// supports these sources, so this is a documented divergence; the supported set is oracle-clean in
/// suites/json/json_builders.test.
#[test]
fn json_builder_deferred_element_source_is_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(err(&mut db, "SELECT to_json(1.5::f64)"), "0A000");
    assert_eq!(err(&mut db, "SELECT jsonb_build_array(1.5::f64)"), "0A000");
    assert_eq!(err(&mut db, "SELECT json_build_array(1.5::f64)"), "0A000");
    assert_eq!(
        err(&mut db, "SELECT jsonb_build_object('k', 1.5::f64)"),
        "0A000"
    );
}

/// `JSON_SERIALIZE` over a `jsonb` value renders its canonical text — a documented divergence from
/// PostgreSQL 18, which returns SQL NULL for a jsonb input (a PG quirk; only `json` input serializes).
/// The json-input behavior is oracle-clean in suites/json/json_ctor.test.
#[test]
fn json_serialize_jsonb_diverges_from_pg() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        query(&mut db, "SELECT JSON_SERIALIZE('{\"b\":2,\"a\":1}'::jsonb)")[0][0],
        "{\"a\": 1, \"b\": 2}" // jed: the jsonb canonical text; PG 18: NULL
    );
}

/// `JSON_SCALAR` over a non-basic scalar (date / float / uuid / …) is a deferred `0A000` — only
/// integer/decimal/boolean/text coerce this slice. PostgreSQL renders any scalar's text as a JSON
/// string, so this is a documented divergence (the basic scalars are oracle-clean in the suite).
#[test]
fn json_scalar_deferred_types_are_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "SELECT JSON_SCALAR('2020-01-01'::date)"),
        "0A000"
    );
    assert_eq!(err(&mut db, "SELECT JSON_SCALAR(1.5::f64)"), "0A000");
}

/// `array_to_json` of a MULTIDIMENSIONAL array is a deferred `0A000` (the to_jsonb multidim
/// deferral) — a documented divergence from PostgreSQL, which renders nested arrays. The 1-D case is
/// oracle-clean in suites/json/json_builders.test.
#[test]
fn array_to_json_multidim_is_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(
            &mut db,
            "SELECT array_to_json(ARRAY[ARRAY[1,2],ARRAY[3,4]])"
        ),
        "0A000"
    );
}

/// A non-scalar `json[b]_build_object` KEY (e.g. a date) is a deferred `0A000` — only text / integer /
/// decimal / boolean keys coerce to text this slice. PostgreSQL renders any type's text output as the
/// key, so this is a documented divergence (the text/int/bool key coercions are oracle-clean in the
/// suite).
#[test]
fn json_build_object_non_scalar_key_is_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "SELECT jsonb_build_object('2020-01-01'::date, 1)"),
        "0A000"
    );
}

/// `json[b]_agg` over a deferred-source value (float, like to_jsonb) is `0A000` — the aggregate
/// reuses the `to_jsonb` element kernel, so the same float/datetime/composite/uuid/bytea/interval
/// sources propagate the deferral (json-sql-functions.md §4). The supported element types are
/// oracle-clean in suites/json/json_agg.test.
#[test]
fn json_agg_deferred_element_source_is_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    run(&mut db, "CREATE TABLE f (id i32 PRIMARY KEY, x f64)");
    run(&mut db, "INSERT INTO f VALUES (1, 1.5)");
    assert_eq!(err(&mut db, "SELECT jsonb_agg(x) FROM f"), "0A000");
    assert_eq!(err(&mut db, "SELECT json_agg(x) FROM f"), "0A000");
}

/// `json[b]_populate_record` over a NON-composite base argument is `42804` (jed's datatype-mismatch
/// convention); a composite whose field is an array/nested composite is `0A000` (the same
/// coerce-to-field deferral as the record functions). PostgreSQL handles both, so these are
/// documented divergences; the scalar-field behavior is oracle-clean in suites/json/json_populate.test.
#[test]
fn json_populate_non_composite_and_complex_field_divergences() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
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
    let mut db = Database::new_in_memory().session(SessionOptions::default());
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

/// A rename-only column-alias list `AS g(col)` (no types) on a table function is a deferred `0A000`
/// (only the TYPED column-definition list `AS t(col type, …)` — C0 — is parsed). PostgreSQL accepts a
/// rename list on an SRF, so this is a documented divergence.
#[test]
fn srf_rename_only_column_list_is_deferred() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err(
            &mut db,
            "SELECT * FROM jsonb_to_recordset('[{\"a\":1}]') AS t(a, b)"
        ),
        "0A000"
    );
}

/// `json[b]_object_agg` over a deferred-source VALUE (float, like to_jsonb) is `0A000` — the value
/// conversion reuses the to_jsonb element kernel. PG supports it, so this is a documented divergence;
/// the supported value types are oracle-clean in suites/json/json_object_agg.test.
#[test]
fn json_object_agg_deferred_value_source_is_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    run(
        &mut db,
        "CREATE TABLE f (id i32 PRIMARY KEY, k text, x f64)",
    );
    run(&mut db, "INSERT INTO f VALUES (1, 'a', 1.5)");
    assert_eq!(
        err(&mut db, "SELECT jsonb_object_agg(k, x) FROM f"),
        "0A000"
    );
    assert_eq!(err(&mut db, "SELECT json_object_agg(k, x) FROM f"), "0A000");
}

/// `json_agg` over a `json` element CANONICALIZES it (the element conversion runs through the
/// jsonb node tree), dropping the input whitespace — a documented divergence from PostgreSQL, which
/// preserves the verbatim sub-text (`[{ "a" : 1 }]`). This is the same verbatim divergence the json
/// SRFs / accessor operators carry (json.md §4); it can't live in the PG-clean corpus.
#[test]
fn json_agg_canonicalizes_json_elements() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    run(&mut db, "CREATE TABLE j (id i32 PRIMARY KEY, doc json)");
    run(&mut db, "INSERT INTO j VALUES (1, '{ \"a\" : 1 }')");
    // jed canonicalizes the element; PG would render the verbatim `[{ "a" : 1 }]`.
    assert_eq!(
        query(&mut db, "SELECT json_agg(doc) FROM j")[0][0],
        "[{\"a\": 1}]"
    );
}

/// A `jsonb` column round-trips every node kind (object/array/number/string/bool/null) through a
/// serialize + reload, confirming the tagged-node value codec decodes back to the canonical render.
/// The deferred S2 sub-clauses of the SQL/JSON query functions are `0A000` — PASSING (path vars),
/// ON ERROR/EMPTY DEFAULT expr, JSON_QUERY OMIT QUOTES, and JSON_QUERY RETURNING a non-json type.
/// PostgreSQL supports all of these, so each is a documented divergence; the supported subset is
/// oracle-clean in suites/json/json_query_fns.test.
#[test]
fn json_query_fn_deferred_clauses_are_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    for sql in [
        "SELECT JSON_VALUE('{\"a\":1}', '$.a' PASSING 1 AS x)",
        "SELECT JSON_VALUE('{\"a\":1}', '$.b' DEFAULT 'z' ON EMPTY)",
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' OMIT QUOTES)",
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' RETURNING int)",
    ] {
        assert_eq!(err(&mut db, sql), "0A000", "{sql} should defer 0A000");
    }
}

/// The deferred T1 sub-features of JSON_TABLE are `0A000` — an explicit PLAN, PASSING, an array
/// column, a WRAPPER on a scalar column, OMIT QUOTES; an unknown column type is `42704`. PostgreSQL
/// supports the first set, so each is a documented divergence; the supported subset is oracle-clean
/// in suites/json/json_table.test.
#[test]
fn json_table_deferred_features_are_0a000() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    for sql in [
        "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x') PLAN DEFAULT (x))",
        "SELECT * FROM JSON_TABLE('{}', '$' PASSING 1 AS y COLUMNS (x i32 PATH '$.x'))",
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
    let mut db = Database::new_in_memory_with_page_size(4096).session(SessionOptions::default());
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
