//! Common table expressions — `WITH [RECURSIVE] name [(cols)] AS [NOT] MATERIALIZED (query) [, …]
//! <query>` (spec/design/cte.md, spec/design/recursive-cte.md). The row/name/error assertions and
//! the inline/materialize cost contract live in the shared conformance corpus
//! (spec/conformance/suites/cte/*.test). What remains here is what the corpus cannot express: the
//! MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), and — for `WITH RECURSIVE` — the
//! cost-ceiling termination of a non-terminating recursion (`54P01`, a host-API `max_cost`) and the
//! inert materialization hint.

use jed::{Database, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

/// A 3-row, single-node table `t(id, n)` = {(1,10),(2,20),(3,30)}.
fn t3() -> Database {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, n i32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ])
}

#[test]
fn materialized_hint_forces_buffering() {
    let mut db = t3();
    // MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
    // row_produced = 13 (vs the inlined 10).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"
        ),
        13
    );
    // NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
    // bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"
        ),
        23
    );
}

/// A non-terminating recursion (`UNION ALL` with no stopping predicate) is bounded by the cost
/// ceiling. Each iteration is cheap (a 1-row working table), so this trips `54P01` ONLY through the
/// CONTINUOUS cross-iteration meter (recursive-cte.md §5) — the untrusted-query safety mechanism
/// doing real work. A per-iteration meter would never fire here, so the corpus cannot express it.
#[test]
fn recursive_unbounded_aborts_at_cost_ceiling() {
    let mut db = Database::new();
    db.set_max_cost(1000);
    let err = execute(
        &mut db,
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT n FROM c",
    )
    .expect_err("an unbounded recursion must abort, not loop forever");
    assert_eq!(err.code(), "54P01", "got {}", err.message);
}

/// A recursion whose total cost fits under the ceiling runs to completion (the ceiling bounds the
/// *actual* accrued cost, not a per-iteration figure).
#[test]
fn recursive_under_ceiling_succeeds() {
    let mut db = Database::new();
    // The 5-row counter accrues 29 (the corpus cost contract); a ceiling above it lets it through.
    db.set_max_cost(1000);
    let r = execute(
        &mut db,
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT n FROM c",
    )
    .expect("a terminating recursion under the ceiling must succeed");
    assert_eq!(r.cost(), 29);
}

/// A recursive CTE is ALWAYS materialized — `NOT MATERIALIZED` is inert (recursive-cte.md §1), so a
/// single-reference recursive CTE still iterates to a fixpoint rather than inlining its body.
#[test]
fn recursive_hint_is_inert() {
    let mut db = Database::new();
    for hint in ["", "MATERIALIZED ", "NOT MATERIALIZED "] {
        let sql = format!(
            "WITH RECURSIVE c(n) AS {hint}(SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT n FROM c ORDER BY n"
        );
        let r = execute(&mut db, &sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
        // Three rows regardless of the hint; the recursive cost is identical (the hint is ignored).
        match r {
            jed::Outcome::Query { rows, cost, .. } => {
                assert_eq!(rows.len(), 3, "hint {hint:?}");
                assert_eq!(cost, 17, "hint {hint:?} cost");
            }
            _ => panic!("expected a query result"),
        }
    }
}

/// Nested-WITH narrowing (cte.md §7): a nested WITH establishes its OWN CTE scope and does NOT inherit
/// the enclosing statement's CTE bindings — a documented DIVERGENCE from PostgreSQL (which inherits
/// them), so it cannot live in the oracle corpus. Inside the nested WITH, an enclosing CTE name with
/// no base table is `42P01`; one that shadows a base table reads the BASE TABLE (PG would read the CTE).
#[test]
fn nested_with_does_not_inherit_enclosing_ctes() {
    // (a) No base table named `e`: the inner reference to the enclosing CTE `e` is unresolved → 42P01.
    let mut db = t3();
    let err = execute(
        &mut db,
        "WITH e AS (SELECT 1 AS v) SELECT * FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s",
    )
    .expect_err("an enclosing CTE is invisible inside a nested WITH (cte.md §7)");
    assert_eq!(err.code(), "42P01", "{}", err.message);

    // (b) A base table `e` exists: inside the nested WITH the enclosing CTE `e` is invisible, so the
    // reference resolves to the BASE TABLE (the rows are the table's, not the CTE's). PG diverges —
    // it would read the enclosing CTE `e` (the single row 1).
    let mut db = db_with(&[
        "CREATE TABLE e (v i32 PRIMARY KEY)",
        "INSERT INTO e VALUES (7), (8)",
    ]);
    let r = execute(
        &mut db,
        "WITH e AS (SELECT 1 AS v) SELECT v FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s ORDER BY v",
    )
    .expect("the base table e resolves inside the nested WITH");
    match r {
        jed::Outcome::Query { rows, .. } => assert_eq!(
            rows,
            vec![vec![jed::Value::Int(7)], vec![jed::Value::Int(8)]],
            "the nested WITH reads the BASE TABLE e, not the enclosing CTE"
        ),
        _ => panic!("expected a query result"),
    }
}
