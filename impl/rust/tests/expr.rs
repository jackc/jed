//! Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
//! minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
//! connectives, operator precedence, and parentheses. These complement the conformance
//! corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and return its rows.
fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

/// Run a single-row, single-column query and return the lone value.
fn scalar(db: &mut Database, sql: &str) -> Value {
    let rows = query(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?}: expected one row");
    assert_eq!(rows[0].len(), 1, "{sql:?}: expected one column");
    rows[0][0].clone()
}

fn err_code(db: &mut Database, sql: &str) -> &'static str {
    execute(db, sql)
        .expect_err(&format!("{sql:?} should fail"))
        .code()
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
        "INSERT INTO t VALUES (1, 6, 4)",
        "INSERT INTO t VALUES (2, 20, 6)",
        "INSERT INTO t VALUES (3, -7, 3)",
    ])
}

#[test]
fn arithmetic_and_precedence() {
    let mut db = setup();
    // * binds tighter than +; parens override.
    assert_eq!(
        scalar(&mut db, "SELECT 6 + 4 * 2 FROM t WHERE id = 1"),
        Value::Int(14)
    );
    assert_eq!(
        scalar(&mut db, "SELECT (6 + 4) * 2 FROM t WHERE id = 1"),
        Value::Int(20)
    );
    // the four binary ops over columns.
    assert_eq!(
        scalar(&mut db, "SELECT a + b FROM t WHERE id = 2"),
        Value::Int(26)
    );
    assert_eq!(
        scalar(&mut db, "SELECT a * b FROM t WHERE id = 2"),
        Value::Int(120)
    );
    // integer division truncates toward zero; remainder takes the dividend's sign.
    assert_eq!(
        scalar(&mut db, "SELECT a / b FROM t WHERE id = 3"),
        Value::Int(-2)
    );
    assert_eq!(
        scalar(&mut db, "SELECT a % b FROM t WHERE id = 3"),
        Value::Int(-1)
    );
}

#[test]
fn arithmetic_in_where() {
    let mut db = setup();
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE a + b = 26 ORDER BY id"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn overflow_traps_at_the_result_type() {
    // int32 + int32 overflows at the int32 boundary even though it fits int64.
    let mut db = db_with(&[
        "CREATE TABLE e (id int32 PRIMARY KEY, a int32, b int32)",
        "INSERT INTO e VALUES (1, 2147483647, 1)",
    ]);
    assert_eq!(
        err_code(&mut db, "SELECT a + b FROM e WHERE id = 1"),
        "22003"
    );
    // Widening the operand to int64 lifts the boundary.
    assert_eq!(
        scalar(&mut db, "SELECT CAST(a AS int64) + b FROM e WHERE id = 1"),
        Value::Int(2147483648)
    );
}

#[test]
fn division_and_modulo_by_zero_trap_22012() {
    let mut db = setup();
    assert_eq!(
        err_code(&mut db, "SELECT a / 0 FROM t WHERE id = 1"),
        "22012"
    );
    assert_eq!(
        err_code(&mut db, "SELECT a % 0 FROM t WHERE id = 1"),
        "22012"
    );
}

#[test]
fn unary_minus_and_int64_min() {
    let mut db = setup();
    assert_eq!(
        scalar(&mut db, "SELECT -a FROM t WHERE id = 1"),
        Value::Int(-6)
    );
    assert_eq!(
        scalar(&mut db, "SELECT - -a FROM t WHERE id = 1"),
        Value::Int(6)
    );
    // int64's minimum is reachable only via unary minus.
    assert_eq!(
        scalar(&mut db, "SELECT -9223372036854775808 FROM t WHERE id = 1"),
        Value::Int(i64::MIN)
    );
    // a bare 2^63 fits no signed type (22003); a larger magnitude is a lex error (42601).
    assert_eq!(
        err_code(&mut db, "SELECT 9223372036854775808 FROM t WHERE id = 1"),
        "22003"
    );
    assert_eq!(
        err_code(&mut db, "SELECT 9223372036854775809 FROM t WHERE id = 1"),
        "42601"
    );
}

#[test]
fn comparisons_project_booleans() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
        "INSERT INTO t VALUES (1, 5, 5)",
        "INSERT INTO t VALUES (2, 5, 9)",
        "INSERT INTO t VALUES (3, 5, NULL)",
    ]);
    // true / false / NULL (unknown, from the NULL comparison).
    assert_eq!(
        query(&mut db, "SELECT a = b FROM t ORDER BY id"),
        vec![
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Null],
        ]
    );
    // literals render canonically.
    assert_eq!(
        scalar(&mut db, "SELECT TRUE FROM t WHERE id = 1"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut db, "SELECT FALSE FROM t WHERE id = 1"),
        Value::Bool(false)
    );
    assert_eq!(Value::Bool(true).render(), "true");
    assert_eq!(Value::Bool(false).render(), "false");
}

#[test]
fn kleene_connectives() {
    // p, q encode T/F/U via (col = 1): a dominant operand absorbs unknown.
    let mut db = db_with(&[
        "CREATE TABLE tv (id int32 PRIMARY KEY, p int32, q int32)",
        "INSERT INTO tv VALUES (1, 0, 0)", // false, false
        "INSERT INTO tv VALUES (2, 0, 1)", // false, true
    ]);
    // false AND unknown = false (NULL does not propagate through a dominant FALSE).
    assert_eq!(
        scalar(
            &mut db,
            "SELECT (p = 1) AND (q = NULL) FROM tv WHERE id = 1"
        ),
        Value::Bool(false)
    );
    // true OR unknown = true.
    assert_eq!(
        scalar(&mut db, "SELECT (q = 1) OR (p = NULL) FROM tv WHERE id = 2"),
        Value::Bool(true)
    );
    // NOT unknown = unknown (genuine propagation).
    assert_eq!(
        scalar(&mut db, "SELECT NOT (p = NULL) FROM tv WHERE id = 1"),
        Value::Null
    );
}

#[test]
fn type_errors_and_boolean_narrowings() {
    let mut db = setup();
    // a WHERE expression must be boolean.
    assert_eq!(err_code(&mut db, "SELECT id FROM t WHERE a"), "42804");
    // logical connectives need boolean operands; arithmetic needs integer operands.
    assert_eq!(err_code(&mut db, "SELECT id FROM t WHERE a AND b"), "42804");
    assert_eq!(
        err_code(&mut db, "SELECT (a = b) + 1 FROM t WHERE id = 1"),
        "42804"
    );
    // comparisons are integer-only — comparing two booleans is a type error.
    assert_eq!(
        err_code(&mut db, "SELECT id FROM t WHERE (a = b) = (a = b)"),
        "42804"
    );
    // boolean is not a storable column type, nor a CAST target (0A000).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE bt (id int32 PRIMARY KEY, flag boolean)"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(&mut db, "SELECT CAST(a AS boolean) FROM t WHERE id = 1"),
        "0A000"
    );
    // there is no boolean -> integer cast.
    assert_eq!(
        err_code(&mut db, "SELECT CAST(a = b AS int32) FROM t WHERE id = 1"),
        "42804"
    );
}
