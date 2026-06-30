//! Expression / query nesting-depth limit (CLAUDE.md §13; spec/design/cost.md §7). The §13
//! native-stack-safety gate: the recursive-descent parser and the resolve/eval walks recurse to a
//! statement's nesting depth, so deeply-nested untrusted input would overflow the call stack
//! BEFORE the cost meter runs — `54P01` cannot catch it. A fixed `MAX_EXPR_DEPTH` checked in the
//! parser aborts such input with `54001` (`statement_too_complex`) instead. The conformance corpus
//! (spec/conformance/suites/resource/depth_limit.test) pins the cross-core boundary on small
//! shapes; this exercises the per-vector boundary and that the abort is independent of `max_cost`.

use jed::parser::{MAX_EXPR_DEPTH, Parser};
use jed::{Database, Session, SessionOptions};

fn db() -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[]).unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)", &[]).unwrap();
    db
}

/// The SQLSTATE of running `sql`, or `"ok"` if it succeeded.
fn code(db: &mut Session, sql: &str) -> String {
    match db.execute(sql, &[]) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.code().to_string(),
    }
}

/// A chain of `n` `+` operators over one row: `1 + 1 + … + 1` (the canonical `1+1+…` vector).
fn chain(n: usize) -> String {
    format!("SELECT {} FROM t", vec!["1"; n + 1].join(" + "))
}

#[test]
fn the_limit_is_generous() {
    // Far above any realistic query, so ordinary SQL is never rejected (spec/design/cost.md §7).
    assert_eq!(MAX_EXPR_DEPTH, 256);
}

#[test]
fn deep_operator_chain_aborts_with_54001() {
    let mut db = db();
    // One level past the limit aborts at parse time (the additive loop's counter, so this is
    // O(1) parser stack — no deep recursion even at huge depth).
    assert_eq!(code(&mut db, &chain(MAX_EXPR_DEPTH)), "54001");
    assert_eq!(code(&mut db, &chain(MAX_EXPR_DEPTH * 4)), "54001");
    // A moderately-nested expression still evaluates end to end — the guard does not perturb
    // ordinary queries (a shallow depth so the *debug* giant-frame evaluator stays well within
    // the test-thread stack; see impl/rust/.cargo/config.toml).
    assert_eq!(code(&mut db, &chain(64)), "ok");
}

#[test]
fn the_exact_boundary_is_max_expr_depth() {
    // Pin the precise accept/reject boundary at the *parser* (where the 54001 is raised), which
    // also keeps the success side off the evaluator's deep recursion: a `1+1+…` chain parses with
    // O(1) parser stack, so `MAX_EXPR_DEPTH - 1` levels parse fine and `MAX_EXPR_DEPTH` is the
    // first rejected depth. This is the cross-core contract the corpus mirrors.
    assert!(Parser::parse_sql(&chain(MAX_EXPR_DEPTH - 1)).is_ok());
    let err = Parser::parse_sql(&chain(MAX_EXPR_DEPTH)).unwrap_err();
    assert_eq!(err.code(), "54001");
}

#[test]
fn the_abort_is_independent_of_max_cost() {
    // The overflow this guards strikes during PARSE, before the cost meter runs — so even an
    // unlimited (or tiny) ceiling cannot let a stack-busting statement through. The depth gate
    // fires regardless (CLAUDE.md §13).
    let mut db = db();
    db.set_max_cost(0); // unlimited
    assert_eq!(code(&mut db, &chain(MAX_EXPR_DEPTH * 8)), "54001");
    db.set_max_cost(1); // the tightest possible ceiling
    assert_eq!(code(&mut db, &chain(MAX_EXPR_DEPTH * 8)), "54001");
}

#[test]
fn every_nesting_vector_aborts_not_crashes() {
    // Each recursion vector — nested parens, ARRAY, NOT, unary minus, scalar subqueries, postfix
    // casts, and UNION chains — is bounded by the same counter and returns 54001 deterministically
    // rather than overflowing the native stack. `n` well past the limit for each.
    let mut db = db();
    let n = MAX_EXPR_DEPTH * 2;
    let vectors = [
        format!("SELECT {}1{} FROM t", "(".repeat(n), ")".repeat(n)),
        format!("SELECT {}1{} FROM t", "ARRAY[".repeat(n), "]".repeat(n)),
        format!("SELECT {}true FROM t", "NOT ".repeat(n)),
        format!("SELECT {}1 FROM t", "- ".repeat(n)),
        format!("SELECT {}1{} FROM t", "(SELECT ".repeat(n), ")".repeat(n)),
        format!("SELECT 1{} FROM t", "::int4".repeat(n)),
        format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(n)),
    ];
    for sql in &vectors {
        assert_eq!(
            code(&mut db, sql),
            "54001",
            "expected 54001 for a {}-deep vector",
            n
        );
    }
}

#[test]
fn deep_nesting_in_where_and_check_is_bounded() {
    // The guard sits in the parser, so it protects every clause that holds an expression — WHERE
    // and a CHECK constraint included (these reach the pre-resolve structural walks, which the
    // parser bound keeps shallow). A normal table is unaffected.
    let mut db = db();
    let pred = vec!["1"; MAX_EXPR_DEPTH + 2].join(" + ");
    assert_eq!(
        code(&mut db, &format!("SELECT v FROM t WHERE {pred} = 0")),
        "54001"
    );
    assert_eq!(
        code(
            &mut db,
            &format!("CREATE TABLE u (a i32 CHECK ({pred} > 0))")
        ),
        "54001"
    );
}
