//! Expression / query nesting-depth limit (CLAUDE.md §13; spec/design/cost.md §7). The §13
//! native-stack-safety gate: the recursive-descent parser and the resolve/eval walks recurse to a
//! statement's nesting depth, so deeply-nested untrusted input would overflow the call stack
//! BEFORE the cost meter runs — `54P01` cannot catch it. A fixed `MAX_EXPR_DEPTH` checked in the
//! parser aborts such input with `54001` (`statement_too_complex`) instead. The conformance corpus
//! (spec/conformance/suites/resource/depth_limit.test) pins the cross-core boundary on small
//! shapes; this exercises the per-vector boundary and that the abort is independent of `max_cost`.

use jed::parser::{MAX_EXPR_DEPTH, Parser};
use jed::{CreateOptions, Database, Session, SessionOptions};

fn db() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO t VALUES (1, 1)", &[])
        .unwrap();
    db
}

/// The SQLSTATE of running `sql`, or `"ok"` if it succeeded.
fn code(db: &mut Session, sql: &str) -> String {
    match db.query_outcome(sql, &[]) {
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
    // A program limit is not a mistake at a place: no Position (spec/design/error-fields.md §10.2).
    assert_eq!(err.position, None);
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

/// The native-stack budget the depth gate holds an optimized build to (spec/design/cost.md §7.1):
/// every nesting vector, at the deepest depth the parser accepts, parses, plans, and executes within
/// it, and the over-limit form is rejected within it. An unoptimized build's frames are several
/// times larger, so there the shapes still run end to end, on a stack sized for that build.
#[cfg(jed_optimized)]
const TEST_STACK: usize = 1 << 20;
#[cfg(not(jed_optimized))]
const TEST_STACK: usize = 256 << 20;

/// One recursion vector: its nesting `shape(n)` and any statements it needs first.
struct Shape {
    name: &'static str,
    shape: fn(usize) -> String,
    setup: fn(usize) -> Vec<String>,
}

fn no_setup(_: usize) -> Vec<String> {
    Vec::new()
}

fn nest(open: &str, inner: &str, close: &str, n: usize) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

fn shapes() -> Vec<Shape> {
    fn s(name: &'static str, shape: fn(usize) -> String) -> Shape {
        Shape {
            name,
            shape,
            setup: no_setup,
        }
    }
    vec![
        s("parens", |n| {
            format!("SELECT {} FROM t", nest("(", "1", ")", n))
        }),
        s("ARRAY", |n| {
            format!("SELECT {} FROM t", nest("ARRAY[", "1", "]", n))
        }),
        s("ROW", |n| {
            format!("SELECT {} FROM t", nest("ROW(", "1", ")", n))
        }),
        s("NOT", |n| format!("SELECT {}true FROM t", "NOT ".repeat(n))),
        s("unary minus", |n| {
            format!("SELECT {}1 FROM t", "- ".repeat(n))
        }),
        s("operator chain", |n| {
            format!("SELECT {} FROM t", vec!["v"; n + 1].join(" + "))
        }),
        // The two-pass operand resolvers (cost.md §7): the `-` jsonb peek, `||`, the polymorphic
        // array functions, containment, and `= ANY(ARRAY[…])`.
        s("minus chain", |n| {
            format!("SELECT {} FROM t", vec!["v"; n + 1].join(" - "))
        }),
        s("array concat chain", |n| {
            format!("SELECT {} FROM t", vec!["ARRAY[v]"; n + 1].join(" || "))
        }),
        s("jsonb concat chain", |n| {
            format!("SELECT {} FROM t", vec!["'{}'::jsonb"; n + 1].join(" || "))
        }),
        s("array function", |n| {
            format!(
                "SELECT {} FROM t",
                nest("array_append(", "ARRAY[v]", ", 1)", n)
            )
        }),
        s("containment", |n| {
            format!(
                "SELECT {} FROM t",
                nest("ARRAY[true] @> ARRAY[", "true", "]", n)
            )
        }),
        s("= ANY", |n| {
            format!(
                "SELECT {} FROM t",
                nest("true = ANY(ARRAY[", "true", "])", n)
            )
        }),
        s("AND chain", |n| {
            format!("SELECT {} FROM t", vec!["true"; n + 1].join(" AND "))
        }),
        s("comparison operand", |n| {
            format!("SELECT {} FROM t", nest("(true = ", "true", ")", n))
        }),
        s("cast chain", |n| {
            format!("SELECT 1{} FROM t", "::int4".repeat(n))
        }),
        s("CAST", |n| {
            format!("SELECT {} FROM t", nest("CAST(", "1", " AS i64)", n))
        }),
        s("function call", |n| {
            format!("SELECT {} FROM t", nest("abs(", "1", ")", n))
        }),
        s("COALESCE", |n| {
            format!("SELECT {} FROM t", nest("coalesce(NULL, ", "1", ")", n))
        }),
        s("CASE", |n| {
            format!(
                "SELECT {} FROM t",
                nest("CASE WHEN true THEN ", "1", " END", n)
            )
        }),
        s("subscript index", |n| {
            format!("SELECT {} FROM t", nest("ARRAY[1, 2][", "1", "]", n))
        }),
        s("scalar subquery", |n| {
            format!("SELECT {} FROM t", nest("(SELECT ", "1", ")", n))
        }),
        s("correlated subquery", |n| {
            format!("SELECT {} FROM t", nest("(SELECT ", "t.v", ")", n))
        }),
        s("EXISTS", |n| {
            format!("SELECT {} FROM t", nest("EXISTS (SELECT ", "true", ")", n))
        }),
        s("IN subquery", |n| {
            format!("SELECT {} FROM t", nest("true IN (SELECT ", "true", ")", n))
        }),
        s("derived table", |n| {
            format!(
                "SELECT * FROM {}",
                nest("(SELECT * FROM ", "t", ") AS x", n)
            )
        }),
        s("parenthesized query", |n| nest("(", "SELECT 1", ")", n)),
        s("parenthesized join", |n| {
            format!(
                "SELECT * FROM {}",
                nest("(", "t AS a JOIN t AS b ON true", ")", n)
            )
        }),
        s("UNION chain", |n| {
            format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(n))
        }),
        s("nested WITH", |n| {
            nest("WITH x AS (", "SELECT 1", ") SELECT * FROM x", n)
        }),
        s("WHERE", |n| {
            format!("SELECT v FROM t WHERE {} = 1", nest("(", "v", " + 0)", n))
        }),
        s("GROUP BY expression", |n| {
            let e = nest("(", "v", " + 1)", n);
            format!("SELECT {e} FROM t GROUP BY {e}")
        }),
        s("ORDER BY expression", |n| {
            format!("SELECT v FROM t ORDER BY {}", nest("(", "v", " + 1)", n))
        }),
        s("aggregate argument", |n| {
            format!("SELECT sum({}) FROM t", nest("(", "v", " + 1)", n))
        }),
        s("EXPLAIN VERBOSE", |n| {
            format!(
                "EXPLAIN (VERBOSE) SELECT {} FROM t",
                nest("(", "v", " + 1)", n)
            )
        }),
        s("UPDATE", |n| {
            format!("UPDATE t SET v = {}", nest("(", "v", " + 0)", n))
        }),
        s("INSERT SELECT", |n| {
            format!(
                "INSERT INTO t SELECT id + 10, {} FROM t",
                nest("(", "v", " + 0)", n)
            )
        }),
        s("DELETE", |n| {
            format!("DELETE FROM t WHERE {} = 99", nest("(", "v", " + 0)", n))
        }),
        Shape {
            name: "CHECK",
            shape: |_| "INSERT INTO c VALUES (1)".to_string(),
            setup: |n| {
                vec![format!(
                    "CREATE TABLE c (a i32 CHECK ({} > 0))",
                    nest("(", "a", " + 0)", n)
                )]
            },
        },
        Shape {
            name: "DEFAULT",
            shape: |_| "INSERT INTO d (k) VALUES (1)".to_string(),
            setup: |n| {
                vec![format!(
                    "CREATE TABLE d (k i32 PRIMARY KEY, a i32 DEFAULT {})",
                    nest("(", "1", " + 0)", n)
                )]
            },
        },
        Shape {
            name: "view",
            shape: |_| "SELECT x FROM vp".to_string(),
            setup: |n| {
                vec![format!(
                    "CREATE VIEW vp AS SELECT {} AS x FROM t",
                    nest("(", "v", " + 0)", n)
                )]
            },
        },
    ]
}

/// The SQLSTATE of running `stmts` in order, or `"ok"`; stops at the first error.
fn run_all(db: &mut Session, stmts: &[String]) -> String {
    for sql in stmts {
        let c = code(db, sql);
        if c != "ok" {
            return c;
        }
    }
    "ok".to_string()
}

/// The deepest `n` at which `shape` + its setup is not rejected `54001`, by binary search (the
/// rejection is monotone in `n`).
fn deepest_accepted(sh: &Shape) -> usize {
    let rejected = |n: usize| {
        let mut stmts = (sh.setup)(n);
        stmts.push((sh.shape)(n));
        stmts
            .iter()
            .any(|sql| matches!(Parser::parse_sql(sql), Err(e) if e.code() == "54001"))
    };
    let (mut lo, mut hi) = (1, MAX_EXPR_DEPTH * 2);
    assert!(
        !rejected(lo) && rejected(hi),
        "{}: no 54001 boundary",
        sh.name
    );
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if rejected(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    lo
}

#[test]
fn every_nesting_vector_fits_the_stack_budget() {
    // Each shape runs end to end at its deepest accepted depth, and is rejected 54001 past it, on a
    // thread of exactly the documented budget — so the bound does not depend on the host thread or on
    // RUST_MIN_STACK (impl/rust/.cargo/config.toml). Exceeding the budget aborts this test binary.
    std::thread::Builder::new()
        .stack_size(TEST_STACK)
        .spawn(|| {
            for sh in shapes() {
                let n = deepest_accepted(&sh);
                let mut stmts = (sh.setup)(n);
                stmts.push((sh.shape)(n));
                assert_eq!(run_all(&mut db(), &stmts), "ok", "{} at depth {n}", sh.name);
                let mut stmts = (sh.setup)(MAX_EXPR_DEPTH * 2);
                stmts.push((sh.shape)(MAX_EXPR_DEPTH * 2));
                assert_eq!(
                    run_all(&mut db(), &stmts),
                    "54001",
                    "{} past the limit",
                    sh.name
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_view_chain_fits_the_stack_budget() {
    // Each view reference adds its body's depth (spec/design/views.md §4.1): a chain of views is as
    // deep as the nested derived tables it stands for. Create the chain until the next view would be
    // rejected, then read through all of it.
    std::thread::Builder::new()
        .stack_size(TEST_STACK)
        .spawn(|| {
            let mut db = db();
            assert_eq!(code(&mut db, "CREATE VIEW v0 AS SELECT id, v FROM t"), "ok");
            let mut last = 0;
            loop {
                let next = format!("CREATE VIEW v{} AS SELECT id, v FROM v{last}", last + 1);
                match code(&mut db, &next).as_str() {
                    "ok" => last += 1,
                    "54001" => break,
                    other => panic!("unexpected {other} creating view {}", last + 1),
                }
            }
            assert!(last > MAX_EXPR_DEPTH / 2, "the chain reached {last}");
            assert_eq!(code(&mut db, &format!("SELECT * FROM v{last}")), "ok");
        })
        .unwrap()
        .join()
        .unwrap();
}
