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

// ---- scalar subqueries ----------------------------------------------------------------------

#[test]
fn scalar_in_where_and_select_list() {
    let mut db = ab();
    // In WHERE: only a's row whose k equals b's max k (40) — none here, so empty.
    assert_eq!(
        ints(&mut db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)"),
        Vec::<i64>::new()
    );
    // max k of a is 30; the row id 3.
    assert_eq!(
        ints(&mut db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM a)"),
        vec![3]
    );
    // In the select list — a constant appended to each row.
    assert_eq!(
        ints(
            &mut db,
            "SELECT (SELECT count(*) FROM b) FROM a ORDER BY id"
        ),
        vec![3, 3, 3]
    );
}

#[test]
fn scalar_nested_and_in_expression() {
    let mut db = ab();
    // Nested subquery folds inner-first (every SELECT needs a FROM in jed).
    assert_eq!(
        ints(
            &mut db,
            "SELECT (SELECT (SELECT max(k) FROM b) FROM a WHERE id = 1) FROM a WHERE id = 1"
        ),
        vec![40]
    );
    // Folded into a larger expression.
    assert_eq!(
        ints(
            &mut db,
            "SELECT (SELECT max(k) FROM a) + 1 FROM a WHERE id = 1"
        ),
        vec![31]
    );
    // Folded constant participating per-row in a projection expression.
    assert_eq!(
        ints(
            &mut db,
            "SELECT k + (SELECT max(k) FROM b) FROM a ORDER BY id"
        ),
        vec![10 + 40, 20 + 40, 30 + 40]
    );
}

#[test]
fn scalar_empty_is_null() {
    let mut db = ab();
    // 0 rows -> NULL; `k = NULL` is never TRUE, so no rows.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE k = (SELECT k FROM b WHERE id = 99)"
        ),
        Vec::<i64>::new()
    );
    // The NULL itself projects as NULL.
    assert_eq!(
        query(
            &mut db,
            "SELECT (SELECT k FROM b WHERE id = 99) FROM a WHERE id = 1"
        ),
        vec![vec![Value::Null]]
    );
}

#[test]
fn scalar_cross_type_promotes() {
    // A scalar subquery returning bigint compares with an int32 column via promotion (not a
    // family error): the folded constant carries bigint, and int32<->int64 compare by value.
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

// ---- IN subqueries --------------------------------------------------------------------------

#[test]
fn in_and_not_in() {
    let mut db = ab();
    // a's k values (10,20,30) that are also in b's k (20,30,40): 20,30 -> ids 2,3.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE k IN (SELECT k FROM b) ORDER BY id"
        ),
        vec![2, 3]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b) ORDER BY id"
        ),
        vec![1]
    );
}

#[test]
fn in_empty_result_is_false() {
    let mut db = ab();
    // Empty subquery -> IN is FALSE for every row, NOT IN is TRUE for every row.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE k IN (SELECT k FROM b WHERE id = 99)"
        ),
        Vec::<i64>::new()
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b WHERE id = 99) ORDER BY id"
        ),
        vec![1, 2, 3]
    );
}

#[test]
fn in_with_null_is_three_valued() {
    let mut db = db_with(&[
        "CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
        // a single-column table with a NULL among the values
        "CREATE TABLE vals (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO s VALUES (1, 5), (2, 10)",
        "INSERT INTO vals VALUES (1, 10), (2, NULL)",
    ]);
    // 10 matches -> TRUE (id 2 kept). 5 matches nothing but the NULL makes it UNKNOWN, not
    // FALSE, so id 1 is dropped (only TRUE keeps a row) — same as a literal IN (10, NULL).
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM s WHERE k IN (SELECT v FROM vals) ORDER BY id"
        ),
        vec![2]
    );
    // NOT IN: 5 NOT IN (10, NULL) is also UNKNOWN -> dropped; 10 NOT IN (...) is FALSE -> dropped.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM s WHERE k NOT IN (SELECT v FROM vals)"
        ),
        Vec::<i64>::new()
    );
}

// ---- EXISTS ---------------------------------------------------------------------------------

#[test]
fn exists_and_not_exists() {
    let mut db = ab();
    // EXISTS is a whole-query gate (uncorrelated): b has rows -> TRUE -> all a rows kept.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b) ORDER BY id"
        ),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE id = 99) ORDER BY id"
        ),
        vec![1, 2, 3]
    );
    // Empty -> EXISTS FALSE -> no rows.
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE id = 99)"
        ),
        Vec::<i64>::new()
    );
}

#[test]
fn exists_ignores_select_list() {
    let mut db = ab();
    // Multi-column / star select lists are legal under EXISTS (columns are irrelevant).
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE EXISTS (SELECT 1, 2, 3 FROM b) ORDER BY id"
        ),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT id FROM a WHERE EXISTS (SELECT * FROM b) ORDER BY id"
        ),
        vec![1, 2, 3]
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

// ---- errors + narrowings --------------------------------------------------------------------

#[test]
fn subquery_error_codes() {
    let mut db = ab();
    let cases = [
        // scalar returning more than one row -> cardinality violation
        ("SELECT (SELECT k FROM b) FROM a WHERE id = 1", "21000"),
        // scalar returning more than one column -> 42601
        (
            "SELECT (SELECT id, k FROM b WHERE id = 1) FROM a WHERE id = 1",
            "42601",
        ),
        // IN subquery returning more than one column -> 42601
        ("SELECT id FROM a WHERE k IN (SELECT id, k FROM b)", "42601"),
        // the >1-column check is plan-time, so it fires even over an empty subquery result
        (
            "SELECT (SELECT id, k FROM b WHERE id = 99) FROM a WHERE id = 1",
            "42601",
        ),
        // A bind parameter inside a subquery is now allowed (see params tests below); a $N with NO
        // type context anywhere (here a bare select-list $1) stays uninferable -> 42P18 (PG instead
        // defaults it to text, then `int = text` errors — a documented divergence, §26).
        (
            "SELECT id FROM a WHERE k = (SELECT $1 FROM a LIMIT 1)",
            "42P18",
        ),
        // grouping / ordering a subquery BY an enclosing-query column -> 0A000 (degenerate, §26)
        (
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b GROUP BY a.k)",
            "0A000",
        ),
        (
            "SELECT id FROM a WHERE EXISTS (SELECT k FROM b ORDER BY a.k)",
            "0A000",
        ),
    ];
    for (sql, code) in cases {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), code, "{sql}");
    }
}

// ---- correlated subqueries (spec/design/grammar.md §26) -------------------------------------

fn t123() -> Database {
    db_with(&[
        "CREATE TABLE t1 (id int32 PRIMARY KEY, v int32)",
        "CREATE TABLE t2 (id int32 PRIMARY KEY, v int32)",
        "CREATE TABLE t3 (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO t1 VALUES (1, 10), (2, 20)",
        "INSERT INTO t2 VALUES (1, 10), (2, 30)",
        "INSERT INTO t3 VALUES (1, 10), (2, 20)",
    ])
}

#[test]
fn correlated_exists() {
    let mut db = t123();
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"
        ),
        vec![1]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"
        ),
        vec![2]
    );
}

#[test]
fn correlated_scalar_and_empty_is_null() {
    let mut db = t123();
    // count over a correlated WHERE (the outer ref is a constant in the inner WHERE).
    assert_eq!(
        query(
            &mut db,
            "SELECT t1.id, (SELECT count(*) FROM t2 WHERE t2.v > t1.v) FROM t1 ORDER BY t1.id"
        ),
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1)],
        ]
    );
    // a 0-row correlated scalar is NULL, evaluated per outer row.
    assert_eq!(
        query(
            &mut db,
            "SELECT t1.id, (SELECT t2.v FROM t2 WHERE t2.v = t1.v * 100) FROM t1 ORDER BY t1.id"
        ),
        vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Null],
        ]
    );
}

#[test]
fn correlated_in() {
    let mut db = t123();
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE t1.v IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"
        ),
        vec![1]
    );
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE t1.v NOT IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"
        ),
        vec![2]
    );
}

#[test]
fn correlated_in_join_on() {
    let mut db = t123();
    // the inner self-join's ON predicate references the OUTER t1 (correlation in a JOIN ON).
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 JOIN t2 AS t2b ON t2b.v = t1.v WHERE t2.id = t1.id) ORDER BY t1.id"
        ),
        vec![1]
    );
}

#[test]
fn correlated_multi_level_and_skip_level() {
    let mut db = t123();
    // two-level nesting, each level correlating to its IMMEDIATE parent.
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v AND EXISTS (SELECT 1 FROM t3 WHERE t3.v = t2.v)) ORDER BY t1.id"
        ),
        vec![1]
    );
    // skip-level: the innermost references the GRANDPARENT t1, skipping t2.
    assert_eq!(
        ints(
            &mut db,
            "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE EXISTS (SELECT 1 FROM t3 WHERE t3.v = t1.v)) ORDER BY t1.id"
        ),
        vec![1, 2]
    );
}

#[test]
fn correlated_outer_ref_in_aggregate_arg() {
    let mut db = t123();
    // An outer reference inside an aggregate ARGUMENT, mixed with an inner column:
    // sum(t2.v + t1.v) over t2 for each t1 row -> (10+10)+(30+10)=60 ; (10+20)+(30+20)=80.
    assert_eq!(
        query(
            &mut db,
            "SELECT t1.id, (SELECT sum(t2.v + t1.v) FROM t2) FROM t1 ORDER BY t1.id"
        ),
        vec![
            vec![Value::Int(1), Value::Int(60)],
            vec![Value::Int(2), Value::Int(80)],
        ]
    );
}

#[test]
fn correlated_subquery_cost_is_per_outer_row() {
    let mut db = t123();
    // A correlated subquery re-runs once per outer row (unlike the uncorrelated fold-once), and
    // each re-scan of the inner table charges its page_read too. The derivation is in
    // spec/conformance/suites/subquery/correlated.test (cost = 17).
    assert_eq!(
        cost(
            &mut db,
            "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v)"
        ),
        17
    );
}

#[test]
fn correlated_inner_error_raised_over_empty_outer() {
    // The subquery is PLANNED once, so a structural error (here >1 column) is raised even when the
    // outer query is empty and the subquery never executes (PostgreSQL parity).
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

// ---- subqueries in UPDATE / DELETE (spec/design/grammar.md §26) -----------------------------
// A subquery is legal in a DELETE/UPDATE WHERE and an UPDATE assignment RHS. An uncorrelated one
// folds once (cost added once); a correlated one references the TARGET row via the per-row outer
// environment and re-runs per matching row. The mutation stays two-phase / all-or-nothing: the
// subquery reads the pre-statement snapshot (DELETE collects keys first; UPDATE writes in phase 2).

#[test]
fn delete_where_uncorrelated_in_subquery() {
    let mut db = ab();
    // delete a's rows whose k is one of b's k values {20,30,40}: ids 2 (20) and 3 (30) go.
    assert!(matches!(
        execute(&mut db, "DELETE FROM a WHERE k IN (SELECT k FROM b)").unwrap(),
        Outcome::Statement { .. }
    ));
    assert_eq!(ints(&mut db, "SELECT id FROM a ORDER BY id"), vec![1]);
}

#[test]
fn delete_where_correlated_exists_subquery() {
    let mut db = ab();
    // EXISTS a b row whose k equals THIS a row's k: a.k ∈ {10,20,30}, b.k ∈ {20,30,40} -> 20,30 match.
    assert!(matches!(
        execute(
            &mut db,
            "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)"
        )
        .unwrap(),
        Outcome::Statement { .. }
    ));
    assert_eq!(ints(&mut db, "SELECT id FROM a ORDER BY id"), vec![1]);
    // NOT EXISTS is the complement.
    let mut db = ab();
    execute(
        &mut db,
        "DELETE FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
    )
    .unwrap();
    assert_eq!(ints(&mut db, "SELECT id FROM a ORDER BY id"), vec![2, 3]);
}

#[test]
fn update_set_correlated_scalar_subquery() {
    let mut db = ab();
    // each a.k becomes max(b.k) over b rows with b.k > the OLD a.k: 10->40, 20->40, 30->40.
    execute(
        &mut db,
        "UPDATE a SET k = (SELECT max(b.k) FROM b WHERE b.k > a.k)",
    )
    .unwrap();
    assert_eq!(
        ints(&mut db, "SELECT k FROM a ORDER BY id"),
        vec![40, 40, 40]
    );
}

#[test]
fn update_set_correlated_scalar_empty_is_null() {
    let mut db = db_with(&[
        "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
        "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
        "INSERT INTO a VALUES (1, 5), (2, 100)",
        "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
    ]);
    // id1 (k=5): max(b.k>5)=40 ; id2 (k=100): no b.k>100 -> empty scalar -> NULL.
    execute(
        &mut db,
        "UPDATE a SET k = (SELECT max(b.k) FROM b WHERE b.k > a.k)",
    )
    .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id, k FROM a ORDER BY id"),
        vec![
            vec![Value::Int(1), Value::Int(40)],
            vec![Value::Int(2), Value::Null],
        ]
    );
}

#[test]
fn update_where_correlated_with_uncorrelated_set() {
    let mut db = ab();
    // WHERE: a.k + 10 is one of b's k {20,30,40} -> all three rows (20,30,40). SET: uncorrelated
    // min(b.k)=20, folded once. So every row -> 20.
    execute(
        &mut db,
        "UPDATE a SET k = (SELECT min(k) FROM b) WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k + 10)",
    )
    .unwrap();
    assert_eq!(
        ints(&mut db, "SELECT k FROM a ORDER BY id"),
        vec![20, 20, 20]
    );
}

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
