//! The `jsonpath` type (spec/design/jsonpath.md, slice P1a) — the per-core checks the conformance
//! corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (the deferred P1b constructs
//! are `0A000`, where PG compiles them; a jsonpath is non-comparable / a jsonpath column is `0A000`).
//! The agreeing behavior (the canonical render, malformed → 42601) is oracle-clean in
//! suites/json/jsonpath_literal.test.

use jed::{Database, execute};

fn err(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: expected an error"))
        .code()
        .to_string()
}

/// The P1b path-expression constructs — filters `?(…)`, item methods `.m()`, arithmetic, `like_regex`,
/// and the `@`/`$name` filter-context primaries — are a deferred `0A000` at compile (P1a parses only
/// the structural-accessor subset). PostgreSQL compiles them, so each is a documented divergence; the
/// supported subset is oracle-clean in suites/json/jsonpath_literal.test.
#[test]
fn jsonpath_p1b_constructs_are_0a000() {
    let mut db = Database::new();
    for path in [
        "$.a ? (@ > 1)", // filter
        "$.a.size()",    // item method
        "$.a + 2",       // arithmetic
        "$[$x]",         // a non-literal subscript expression
        "$x",            // a path variable
    ] {
        assert_eq!(
            err(&mut db, &format!("SELECT '{path}'::jsonpath")),
            "0A000",
            "path `{path}` should defer 0A000"
        );
    }
}

/// A `jsonpath` value is NOT comparable — every comparison / ORDER BY is `42883` (PG ships no
/// opclass). A documented contract (jsonpath.md §1); only `IS [NOT] NULL` applies.
#[test]
fn jsonpath_is_not_comparable() {
    let mut db = Database::new();
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
    let mut db = Database::new();
    assert_eq!(err(&mut db, "CREATE TABLE t (p jsonpath)"), "0A000");
}

/// A malformed jsonpath literal is `42601` (PG's syntax-error class), distinct from the `0A000` of a
/// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
/// distinction against the 0A000 ones above.)
#[test]
fn malformed_jsonpath_is_42601() {
    let mut db = Database::new();
    assert_eq!(err(&mut db, "SELECT '$.'::jsonpath"), "42601");
    assert_eq!(err(&mut db, "SELECT '$['::jsonpath"), "42601");
    assert_eq!(err(&mut db, "SELECT '$[1 to'::jsonpath"), "42601");
}
