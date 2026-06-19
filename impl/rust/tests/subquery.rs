//! Subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS (SELECT …)`, both
//! uncorrelated and CORRELATED. These complement the conformance corpus
//! (spec/conformance/suites/subquery) with finer-grained per-feature assertions: the uncorrelated
//! fold (execute once → constant, cost added once), the typed-NULL of an empty scalar, three-valued
//! `IN`, EXISTS ignoring the select list; and for correlated subqueries the scope-chain resolution,
//! per-outer-row execution + cost, correlation in a JOIN ON and inside an aggregate argument,
//! multi-level + skip-level (grandparent) correlation, and the error / narrowing codes
//! (21000 / 42601 / 0A000). See spec/design/grammar.md §26.

use jed::value::Value;
use jed::{Database, Outcome, execute, execute_params};

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

fn ab() -> Database {
    db_with(&[
        "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
        "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
    ])
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    query(db, sql)
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref v => panic!("expected int, got {v:?}"),
        })
        .collect()
}

// ---- cases the corpus covers only compositionally (kept per review) -------------------------

#[test]
fn scalar_cross_type_promotes() {
    // A scalar subquery returning bigint compares with an int32 column via promotion (not a
    // family error): the folded constant carries bigint, and int32<->int64 compare by value.
    // Both legs are pinned in the corpus (compare/promotion + scalar.test), but not this exact
    // int64-subquery-vs-int32-column path — so kept here.
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
        "CREATE TABLE big (id int32 PRIMARY KEY, m int64)",
        "INSERT INTO t VALUES (1, 30), (2, 40)",
        "INSERT INTO big VALUES (1, 30)",
    ]);
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM t WHERE n = (SELECT m FROM big WHERE id = 1)"
        ),
        vec![1]
    );
}

#[test]
fn correlated_inner_error_raised_over_empty_outer() {
    // The subquery is PLANNED once, so a structural error (here >1 column) is raised even when the
    // outer query is empty and the subquery never executes (PostgreSQL parity). The corpus pins the
    // same guarantee via an empty inner filter, not an empty outer — so this trigger shape is kept.
    let mut db = db_with(&[
        "CREATE TABLE e (id int32 PRIMARY KEY, v int32)",
        "CREATE TABLE f (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO f VALUES (1, 1)",
    ]);
    assert_eq!(
        execute(&mut db, "SELECT (SELECT id, v FROM f WHERE v = e.v) FROM e")
            .unwrap_err()
            .code(),
        "42601"
    );
}

// ---- cost -----------------------------------------------------------------------------------

#[test]
fn cost_adds_the_subquery_once() {
    let mut db = ab();
    // Baseline: 1 page_read (a) + scan a (3 storage_row_read) + filter `k = const` per row (3
    // operator_eval) + produce 0 rows (the const 40 matches nothing). The scalar subquery
    // `(SELECT max(k) FROM b)` runs ONCE: 1 page_read (b is one leaf) + scan b (3) + accumulate
    // max over 3 rows (3) + produce 1 row (1) = 8.
    let base = cost(&mut db, "SELECT id FROM a WHERE k = 999");
    let with_sub = cost(&mut db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)");
    // The folded constant is a leaf (no extra operator_eval), so the only delta is the
    // subquery's own cost — added exactly once, not once per outer row.
    assert_eq!(with_sub - base, 8);
}

// ---- subqueries in UPDATE / DELETE (spec/design/grammar.md §26) -----------------------------
// A subquery is legal in a DELETE/UPDATE WHERE and an UPDATE assignment RHS. An uncorrelated one
// folds once (cost added once); a correlated one references the TARGET row via the per-row outer
// environment and re-runs per matching row. The mutation stays two-phase / all-or-nothing: the
// subquery reads the pre-statement snapshot (DELETE collects keys first; UPDATE writes in phase 2).

#[test]
fn delete_correlated_subquery_cost_is_per_row() {
    // A correlated DELETE subquery re-runs per scanned row; an uncorrelated one folds once. The
    // correlated cost therefore exceeds the uncorrelated baseline on the same data — proving the
    // per-row execution (not a fold). Both are deterministic + cross-core identical (CLAUDE.md §13).
    let corr = cost(
        &mut ab(),
        "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
    );
    let uncorr = cost(&mut ab(), "DELETE FROM a WHERE k IN (SELECT k FROM b)");
    assert!(
        corr > uncorr,
        "correlated {corr} should exceed uncorrelated {uncorr}"
    );
}

// ---- bind parameters inside a subquery (spec/design/grammar.md §26) -------------------------
// A $N inside a subquery is allowed once it gets a type from an INNER context; inference is
// statement-wide (one ParamTypes threaded through the whole plan tree), so the same $N may be
// used inside and outside, and a correlated subquery may compare a $N against the outer row.

#[test]
fn param_inside_subquery_inner_context() {
    let mut db = ab();
    let i = |v: i64| Value::Int(v);
    let run = |db: &mut Database, sql: &str, p: &[Value]| -> Vec<i64> {
        match execute_params(db, sql, p).unwrap() {
            Outcome::Query { rows, .. } => rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(n) => n,
                    ref v => panic!("expected int, got {v:?}"),
                })
                .collect(),
            _ => panic!("expected query"),
        }
    };
    // $1 typed by `b.k = $1` (inner); also correlated to the outer a.k. a.k ∈ {10,20,30},
    // b.k ∈ {20,30,40}; the row survives iff some b.k equals BOTH $1 and a.k.
    assert_eq!(
        run(
            &mut db,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = $1 AND b.k = a.k) ORDER BY id",
            &[i(20)]
        ),
        vec![2]
    );
    // $1 typed by `b.id = $1` inside an IN subquery.
    assert_eq!(
        run(
            &mut db,
            "SELECT id FROM a WHERE k IN (SELECT b.k FROM b WHERE b.id = $1) ORDER BY id",
            &[i(1)]
        ),
        vec![2]
    );
    // The same $1 used both OUTSIDE and INSIDE the subquery — one statement-wide inference.
    assert_eq!(
        run(
            &mut db,
            "SELECT id FROM a WHERE k > $1 AND EXISTS (SELECT 1 FROM b WHERE b.k = $1 + 10) ORDER BY id",
            &[i(10)]
        ),
        vec![2, 3]
    );
}

#[test]
fn param_inside_subquery_uninferable_is_42p18() {
    // A $N whose ONLY position is a context-free select-list slot can't be typed -> 42P18, even
    // with a value bound (the type, not the value, is what's missing). PG diverges (defaults text).
    let mut db = ab();
    assert_eq!(
        execute_params(
            &mut db,
            "SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)",
            &[Value::Int(10)]
        )
        .unwrap_err()
        .code(),
        "42P18"
    );
}
