//! The `jsonpath` type (spec/design/jsonpath.md) — the per-core checks the conformance corpus
//! cannot express (CLAUDE.md §10): the deliberate PG divergences (the unsupported constructs are
//! `0A000`, where PG compiles them; a `like_regex` pattern is jed's regex subset; a jsonpath is
//! non-comparable / a jsonpath column is `0A000`; an item method reads a string with jed's own
//! number grammar). The agreeing behavior (the canonical render, malformed → 42601, the item
//! methods) is oracle-clean in suites/json/jsonpath_literal.test and jsonpath_methods.test.

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions, Value};

fn err(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// The datetime item methods are `0A000` at compile, by decision (jsonpath.md §3.1), and so is the
/// `like_regex` `m` flag (jsonpath.md §4.3) unless `q` makes it moot. PostgreSQL compiles them, so
/// each is a documented divergence; the supported surface is oracle-clean in suites/json/.
#[test]
fn jsonpath_unsupported_constructs_are_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for path in [
        "$.a.datetime()", // a datetime item method
        "$.a.datetime(\"HH24\")",
        "$.a.date()",
        "$.a.time(3)",
        "$.a.time_tz()",
        "$.a.timestamp()",
        "$.a.timestamp_tz(2)",
        "$ ? (@ like_regex \"^a\" flag \"m\")", // the multi-line flag
        "$ ? (@ like_regex \"^a\" flag \"ims\")",
    ] {
        assert_eq!(
            err(&mut db, &format!("SELECT '{path}'::jsonpath")),
            "0A000",
            "path `{path}` should be 0A000"
        );
    }
    // Under `q` the pattern is a literal and `m` is ignored, as XQuery specifies.
    let sql = r#"SELECT jsonb_path_exists('"a^b"', '$ ? (@ like_regex "^b" flag "mq")')"#;
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(
            rows[0][0],
            Value::Bool(true),
            "flag mq should match the literal ^b"
        ),
        other => panic!("expected query, got {other:?}"),
    }
}

/// A `.**` level is an integer literal without PostgreSQL 16+'s `_` separators and radix prefixes,
/// as for jsonpath numbers (jsonpath.md §2): `42601` where PostgreSQL accepts them.
#[test]
fn jsonpath_any_level_literal() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for path in ["$.**{1_000}", "$.**{0x10}", "$.**{2 to 0b11}"] {
        assert_eq!(
            err(&mut db, &format!("SELECT '{path}'::jsonpath")),
            "42601",
            "path {path} should be 42601"
        );
    }
}

/// A `like_regex` pattern is jed's regex subset (regex.md §2), not PostgreSQL's ARE: a construct
/// only ARE has is `2201B` when the path compiles, where PostgreSQL accepts it (jsonpath.md §4.3).
#[test]
fn jsonpath_like_regex_subset() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for pattern in [r"\\y", r"\\u0041", "(?i)a", r"a\\b"] {
        assert_eq!(
            err(
                &mut db,
                &format!("SELECT '$ ? (@ like_regex \"{pattern}\")'::jsonpath")
            ),
            "2201B",
            "like_regex {pattern} should be 2201B"
        );
    }
}

/// A `jsonpath` value is NOT comparable — every comparison / ORDER BY is `42883` (PG ships no
/// opclass). A documented contract (jsonpath.md §1); only `IS [NOT] NULL` applies.
#[test]
fn jsonpath_is_not_comparable() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err(&mut db, "SELECT '$.a'::jsonpath = '$.a'::jsonpath"),
        "42883"
    );
    assert_eq!(
        err(&mut db, "SELECT '$.a'::jsonpath < '$.b'::jsonpath"),
        "42883"
    );
}

/// A `jsonpath` COLUMN is `0A000` — jsonpath is literal-only this slice (P1a, like a J0-stage json
/// column). PostgreSQL allows a jsonpath column, so this is a documented divergence.
#[test]
fn jsonpath_column_is_unsupported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(err(&mut db, "CREATE TABLE t (p jsonpath)"), "0A000");
}

/// A jsonpath using a datetime method is `0A000` — it fails to compile. PostgreSQL evaluates it, so
/// it is a documented divergence (jsonpath.md §3.1).
#[test]
fn jsonpath_deferred_constructs_are_0a000() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err(
            &mut db,
            "SELECT jsonb_path_query_array('[\"2020-01-01\"]', '$[*].datetime()')"
        ),
        "0A000"
    );
}

/// A malformed jsonpath literal is `42601` (PG's syntax-error class), distinct from the `0A000` of a
/// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
/// distinction against the 0A000 ones above.)
#[test]
fn malformed_jsonpath_is_42601() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(err(&mut db, "SELECT '$.'::jsonpath"), "42601");
    assert_eq!(err(&mut db, "SELECT '$['::jsonpath"), "42601");
    assert_eq!(err(&mut db, "SELECT '$[1 to'::jsonpath"), "42601");
}

/// `.double()` reads a string with jed's float grammar (float.md §4): no hex floats and no digit
/// underscores, so `"0x10"` is `22036` where PostgreSQL's `float8in` (glibc `strtod`) accepts it.
/// The integer and decimal methods read PostgreSQL's input forms (the corpus).
#[test]
fn jsonpath_method_number_grammar_divergence() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for (doc, path) in [("\"0x10\"", "$.double()")] {
        assert_eq!(
            err(
                &mut db,
                &format!("SELECT jsonb_path_query_first('{doc}', '{path}')")
            ),
            "22036",
            "{doc} {path}"
        );
    }
}
