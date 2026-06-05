//! Uncorrelated subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS
//! (SELECT …)`. These complement the conformance corpus (spec/conformance/suites/subquery) with
//! finer-grained per-feature assertions: plan-time folding semantics (execute once → constant),
//! the typed-NULL of an empty scalar, three-valued `IN`, EXISTS ignoring the select list, the
//! cost contract (the subquery's cost added once, the fold is a leaf), and the error / narrowing
//! codes (21000 / 42601 / 0A000). See spec/design/grammar.md §26.

use jed::value::Value;
use jed::{Database, Outcome, execute};

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
        ints(&mut db, "SELECT (SELECT max(k) FROM a) + 1 FROM a WHERE id = 1"),
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
        ints(&mut db, "SELECT id FROM t WHERE n = (SELECT m FROM big WHERE id = 1)"),
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
        ints(&mut db, "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b) ORDER BY id"),
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
        ints(&mut db, "SELECT id FROM a WHERE EXISTS (SELECT 1, 2, 3 FROM b) ORDER BY id"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&mut db, "SELECT id FROM a WHERE EXISTS (SELECT * FROM b) ORDER BY id"),
        vec![1, 2, 3]
    );
}

// ---- cost -----------------------------------------------------------------------------------

#[test]
fn cost_adds_the_subquery_once() {
    let mut db = ab();
    // Baseline: scan a (3 storage_row_read) + filter `k = const` per row (3 operator_eval) +
    // produce 0 rows (the const 40 matches nothing). The scalar subquery `(SELECT max(k) FROM b)`
    // runs ONCE: scan b (3) + accumulate max over 3 rows (3) + produce 1 row (1) = 7.
    let base = cost(&mut db, "SELECT id FROM a WHERE k = 999");
    let with_sub = cost(&mut db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)");
    // The folded constant is a leaf (no extra operator_eval), so the only delta is the
    // subquery's own cost — added exactly once, not once per outer row.
    assert_eq!(with_sub - base, 7);
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
        // correlated reference (bare) -> 0A000
        (
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE k = a.k)",
            "0A000",
        ),
        // correlated reference (qualified outer label) in a scalar subquery -> 0A000
        (
            "SELECT (SELECT max(k) FROM b WHERE b.id = a.id) FROM a",
            "0A000",
        ),
        // bind parameter inside a subquery -> 0A000
        ("SELECT id FROM a WHERE k = (SELECT $1 FROM a LIMIT 1)", "0A000"),
        // subquery outside a SELECT (DELETE WHERE) -> 0A000 this slice
        ("DELETE FROM a WHERE k IN (SELECT k FROM b)", "0A000"),
    ];
    for (sql, code) in cases {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), code, "{sql}");
    }
}
