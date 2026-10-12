//! Phase 7: parameterized queries (`$N` bind parameters) — spec/design/api.md §5.
//! Parameters are a host-API surface (not the shared corpus): their type is inferred from
//! context and supplied values are coerced two-phase before any row is touched.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in sql {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn rows(db: &mut Session, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    match db
        .query_outcome(sql, params)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected a query result"),
    }
}

fn err_code(db: &mut Session, sql: &str, params: &[Value]) -> String {
    db.query_outcome(sql, params)
        .err()
        .unwrap_or_else(|| panic!("{sql:?}: expected an error"))
        .code()
        .to_string()
}

#[test]
fn where_pk_eq_param_point_lookup() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ]);
    let got = rows(&mut db, "SELECT v FROM t WHERE id = $1", &[Value::Int(2)]);
    assert_eq!(got, vec![vec![Value::Int(20)]]);

    match db
        .query_outcome("SELECT v FROM t WHERE id = $1", &[Value::Null])
        .unwrap()
    {
        Outcome::Query { rows, cost, .. } => {
            assert!(rows.is_empty(), "NULL point parameter is 3VL-empty");
            assert_eq!(cost, 0, "NULL point parameter performs no descent");
        }
        _ => panic!("expected a query result"),
    }
    assert_eq!(
        err_code(
            &mut db,
            "SELECT v FROM t WHERE id = $1",
            &[Value::Int(i32::MAX as i64 + 1)]
        ),
        "22003",
        "an out-of-range point parameter fails during typed binding"
    );
}

#[test]
fn composite_pk_param_tuple_bound() {
    let mut db = db_with(&[
        "CREATE TABLE t (a i32, b i16, v i32, PRIMARY KEY (b, a))",
        "INSERT INTO t VALUES (1, 1, 10), (2, 1, 20), (3, 1, 30), (1, 2, 40)",
    ]);
    let got = rows(
        &mut db,
        "SELECT v FROM t WHERE b = $1 AND a >= $2 ORDER BY a",
        &[Value::Int(1), Value::Int(2)],
    );
    assert_eq!(got, vec![vec![Value::Int(20)], vec![Value::Int(30)]]);
}

#[test]
fn composite_pk_float_param_widens_soundly() {
    let mut db = db_with(&[
        "CREATE TABLE t (f f64, a i32, v i32, PRIMARY KEY (f, a))",
        "INSERT INTO t VALUES (1.5, 1, 10), (2.5, 1, 20)",
    ]);
    let got = rows(
        &mut db,
        "SELECT v FROM t WHERE f = $1 AND a = $2",
        &[Value::Float64(1.5), Value::Int(1)],
    );
    assert_eq!(got, vec![vec![Value::Int(10)]]);
}

#[test]
fn interval_set_runtime_union_and_clip() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
    ]);
    let got = rows(
        &mut db,
        "SELECT id FROM t WHERE id < $1 OR id >= $2 ORDER BY id",
        &[Value::Int(3), Value::Int(4)],
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(4)]
        ]
    );
    let clipped = rows(
        &mut db,
        "SELECT id FROM t WHERE id IN ($1, $2, $3) AND id > $4 ORDER BY id",
        &[Value::Int(1), Value::Int(3), Value::Int(4), Value::Int(2)],
    );
    assert_eq!(clipped, vec![vec![Value::Int(3)], vec![Value::Int(4)]]);
}

#[test]
fn param_adopts_narrow_column_type_and_traps_overflow() {
    // `$1` compared against an i16 column is typed i16; a value out of i16 range traps
    // 22003 at bind, before any scan.
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, s i16)"]);
    db.query_outcome("INSERT INTO t VALUES (1, 100)", &[])
        .unwrap();
    assert_eq!(
        err_code(
            &mut db,
            "SELECT id FROM t WHERE s = $1",
            &[Value::Int(100000)]
        ),
        "22003"
    );
    // In range: it just matches (or not) normally.
    let got = rows(&mut db, "SELECT id FROM t WHERE s = $1", &[Value::Int(100)]);
    assert_eq!(got, vec![vec![Value::Int(1)]]);
}

#[test]
fn insert_values_params_round_trip() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, name text)"]);
    db.query_outcome(
        "INSERT INTO t VALUES ($1, $2)",
        &[Value::Int(7), Value::Text("alice".into())],
    )
    .unwrap();
    let got = rows(
        &mut db,
        "SELECT id, name FROM t WHERE id = $1",
        &[Value::Int(7)],
    );
    assert_eq!(got, vec![vec![Value::Int(7), Value::Text("alice".into())]]);
}

#[test]
fn insert_param_null_into_not_null_traps_23502() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, name text NOT NULL)"]);
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES ($1, $2)",
            &[Value::Int(1), Value::Null],
        ),
        "23502"
    );
}

#[test]
fn insert_param_wrong_family_traps_42804() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, n i32)"]);
    // `$2` is typed i32 (its column); binding text is a family mismatch.
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES ($1, $2)",
            &[Value::Int(1), Value::Text("x".into())],
        ),
        "42804"
    );
}

#[test]
fn update_set_and_where_params() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
    ]);
    db.query_outcome(
        "UPDATE t SET v = $1 WHERE id = $2",
        &[Value::Int(99), Value::Int(2)],
    )
    .unwrap();
    let got = rows(&mut db, "SELECT v FROM t WHERE id = $1", &[Value::Int(2)]);
    assert_eq!(got, vec![vec![Value::Int(99)]]);
}

#[test]
fn delete_where_param() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    db.query_outcome("DELETE FROM t WHERE id = $1", &[Value::Int(2)])
        .unwrap();
    let got = rows(&mut db, "SELECT id FROM t", &[]);
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
}

#[test]
fn text_param_inference() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, name text)",
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
    ]);
    let got = rows(
        &mut db,
        "SELECT id FROM t WHERE name = $1",
        &[Value::Text("bob".into())],
    );
    assert_eq!(got, vec![vec![Value::Int(2)]]);
}

#[test]
fn coalesce_infers_params_from_common_type() {
    // COALESCE resolves its arguments before it knows their common type, then feeds that type back
    // to bare parameters before binding (grammar.md §51).
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let first = rows(&mut db, "SELECT COALESCE($1, 10)", &[Value::Int(7)]);
    assert_eq!(first, vec![vec![Value::Int(7)]]);
    let middle = rows(&mut db, "SELECT COALESCE(NULL, $1, 10)", &[Value::Int(7)]);
    assert_eq!(middle, vec![vec![Value::Int(7)]]);
}

#[test]
fn case_infers_params_from_common_result_type() {
    // CASE result arms use the same common-type feedback as COALESCE (grammar.md §23).
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let then_arm = rows(
        &mut db,
        "SELECT CASE WHEN true THEN $1 ELSE 10 END",
        &[Value::Int(7)],
    );
    assert_eq!(then_arm, vec![vec![Value::Int(7)]]);
    let else_arm = rows(
        &mut db,
        "SELECT CASE WHEN false THEN 10 ELSE $1 END",
        &[Value::Int(7)],
    );
    assert_eq!(else_arm, vec![vec![Value::Int(7)]]);
}

#[test]
fn least_infers_param_from_common_type() {
    // GREATEST/LEAST note a bare parameter at their unified scalar type, like a comparison operand
    // (grammar.md §52). Source branch A skipped this, so LEAST($1, 10) failed 42P18. A per-core
    // test because param binding is a host-API surface (not the shared corpus).
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let got = rows(&mut db, "SELECT LEAST($1, 10)", &[Value::Int(7)]);
    assert_eq!(got, vec![vec![Value::Int(7)]]);
}

#[test]
fn bare_select_param_is_indeterminate_42p18() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    assert_eq!(
        err_code(&mut db, "SELECT $1 FROM t", &[Value::Int(1)]),
        "42P18"
    );
}

#[test]
fn gap_in_param_indices_is_42p18() {
    // `$1` and `$3` referenced, `$2` never — the missing slot is indeterminate.
    let mut db = db_with(&["CREATE TABLE t (a i32 PRIMARY KEY, b i32)"]);
    assert_eq!(
        err_code(
            &mut db,
            "SELECT a FROM t WHERE a = $1 OR b = $3",
            &[Value::Int(1), Value::Int(2), Value::Int(3)],
        ),
        "42P18"
    );
}

#[test]
fn conflicting_inference_is_42804() {
    let mut db = db_with(&["CREATE TABLE t (a i32 PRIMARY KEY, name text)"]);
    assert_eq!(
        err_code(
            &mut db,
            "SELECT a FROM t WHERE a = $1 OR name = $1",
            &[Value::Int(1)],
        ),
        "42804"
    );
}

#[test]
fn count_mismatch_is_42601() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    assert_eq!(
        err_code(&mut db, "SELECT id FROM t WHERE id = $1", &[]),
        "42601"
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT id FROM t WHERE id = $1",
            &[Value::Int(1), Value::Int(2)],
        ),
        "42601"
    );
}

#[test]
fn null_param_three_valued() {
    // `col = $1` with a NULL bound yields UNKNOWN, so no rows; IS NOT DISTINCT FROM matches NULL.
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10)",
    ]);
    let got = rows(&mut db, "SELECT id FROM t WHERE v = $1", &[Value::Null]);
    assert!(got.is_empty());
}

#[test]
fn param_in_in_list() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let got = rows(
        &mut db,
        "SELECT id FROM t WHERE id IN ($1, $2)",
        &[Value::Int(1), Value::Int(3)],
    );
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
}

#[test]
fn ddl_with_params_traps_42601() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (id i32 PRIMARY KEY)",
            &[Value::Int(1)]
        ),
        "42601"
    );
}

#[test]
fn param_typed_by_cast_operator() {
    // `$1::int` declares `$1` as int — PostgreSQL types a parameter by its cast target
    // (api.md §5, grammar.md §37). No surrounding context is needed, so this is NOT 42P18.
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let got = rows(&mut db, "SELECT $1::int", &[Value::Int(42)]);
    assert_eq!(got, vec![vec![Value::Int(42)]]);
    // The `CAST(... AS ...)` spelling infers the parameter's type identically.
    let got = rows(&mut db, "SELECT CAST($1 AS int)", &[Value::Int(7)]);
    assert_eq!(got, vec![vec![Value::Int(7)]]);
}

#[test]
fn param_cast_operator_narrows_and_traps_22003() {
    // `$1::smallint` declares `$1` as i16; a bound value out of i16 range traps 22003 at
    // bind, before any scan (the same two-phase binding as a column-typed parameter).
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    assert_eq!(
        err_code(&mut db, "SELECT $1::smallint", &[Value::Int(100000)]),
        "22003"
    );
}

#[test]
fn cast_operator_inherits_matrix_and_rejects_lone_colon() {
    // `::` desugars to CAST, so an integer → interval cast is the same forbidden pair (42804) the
    // CAST spelling reports.
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    assert_eq!(err_code(&mut db, "SELECT 5::interval", &[]), "42804");
    // A lone `:` is not part of jed's surface — a 42601 syntax error from the lexer.
    assert_eq!(err_code(&mut db, "SELECT 1 : 2", &[]), "42601");
}

#[test]
fn lexer_rejects_bad_param_tokens() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    for sql in [
        "SELECT id FROM t WHERE id = $0",
        "SELECT id FROM t WHERE id = $",
        "SELECT id FROM t WHERE id = $01",
    ] {
        assert_eq!(
            db.query_outcome(sql, &[])
                .err()
                .map(|e| e.code().to_string()),
            Some("42601".to_string()),
            "{sql:?} should be 42601"
        );
    }
}

/// LIKE … ESCAPE desugars to like_escape(p, e) (grammar.md §22). A bare $N pattern or escape is typed
/// text — the type like_escape declares, as PostgreSQL infers it — so either may be bound, and a bound
/// pattern/escape charges exactly like the same literal (a parameter is a constant operand, so the
/// growth kernel charges no output bytes).
#[test]
fn like_escape_bound_parameters() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, s text)",
        "INSERT INTO t VALUES (1, 'a%'), (2, 'ab'), (3, NULL)",
    ]);
    let literal = "SELECT id FROM t WHERE s LIKE 'a#%' ESCAPE '#' ORDER BY id";
    let by_pattern = "SELECT id FROM t WHERE s LIKE $1 ESCAPE '#' ORDER BY id";
    let by_escape = "SELECT id FROM t WHERE s LIKE 'a#%' ESCAPE $1 ORDER BY id";
    let text = |s: &str| Value::Text(s.to_string());
    let cases: [(&str, Value, Vec<i64>); 4] = [
        (by_pattern, text("a#%"), vec![1]),
        (by_pattern, text("a%"), vec![1, 2]),
        (by_escape, text("#"), vec![1]),
        (by_escape, Value::Null, vec![]),
    ];
    for (sql, param, want) in cases {
        let got: Vec<i64> = rows(&mut db, sql, &[param.clone()])
            .iter()
            .map(|r| match r[0] {
                Value::Int(x) => x,
                _ => panic!("expected an int"),
            })
            .collect();
        assert_eq!(got, want, "{sql:?} {param:?}");
    }
    assert_eq!(err_code(&mut db, by_escape, &[text("xy")]), "22025");
    let cost = |db: &mut Session, sql: &str, params: &[Value]| match db
        .query_outcome(sql, params)
        .unwrap()
    {
        Outcome::Query { cost, .. } => cost,
        _ => panic!("expected a query result"),
    };
    let lit = cost(&mut db, literal, &[]);
    assert_eq!(cost(&mut db, by_pattern, &[text("a#%")]), lit);
    assert_eq!(cost(&mut db, by_escape, &[text("#")]), lit);
}

/// A bare $N operand of `||` with no array operand is typed text (string-functions.md §5), as
/// PostgreSQL infers its unknown parameter for textcat / textanycat: it concatenates with a text,
/// string-literal, or non-text operand, and a NULL binding is a NULL result.
#[test]
fn text_concat_bound_parameters() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, s text)",
        "INSERT INTO t VALUES (1, 'x')",
    ]);
    let text = |s: &str| Value::Text(s.to_string());
    let cases: [(&str, Vec<Value>, Value); 5] = [
        ("SELECT $1 || 'b' FROM t", vec![text("a")], text("ab")),
        ("SELECT s || $1 FROM t", vec![text("a")], text("xa")),
        ("SELECT $1 || id FROM t", vec![text("a")], text("a1")),
        (
            "SELECT $1 || $2 FROM t",
            vec![text("a"), text("b")],
            text("ab"),
        ),
        ("SELECT $1 || s FROM t", vec![Value::Null], Value::Null),
    ];
    for (sql, params, want) in cases {
        let got = rows(&mut db, sql, &params);
        assert_eq!(got.len(), 1, "{sql:?}");
        assert_eq!(got[0][0], want, "{sql:?} {params:?}");
    }
    // The parameter is declared text, so an integer binding does not coerce.
    assert_eq!(
        err_code(&mut db, "SELECT $1 || id FROM t", &[Value::Int(5)]),
        "42804"
    );
}

/// A bare $N scalar-function argument adopts the family the catalog declares for its slot
/// (functions.md §9.1): one candidate decides outright, `text` wins among several, and an overload
/// set that cannot decide is 42725. An `any` slot fixes no type (42P18).
#[test]
fn function_argument_param_adopts_declared_family() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, s text)",
        "INSERT INTO t VALUES (1, 'xyz')",
    ]);
    let text = |s: &str| Value::Text(s.to_string());
    let one = |db: &mut Session, sql: &str, params: &[Value]| rows(db, sql, params)[0][0].clone();
    assert_eq!(
        one(&mut db, "SELECT translate($1, 'a', 'b')", &[text("banana")]),
        text("bbnbnb")
    );
    assert_eq!(
        one(
            &mut db,
            "SELECT translate($1, $2, $3)",
            &[text("abc"), text("b"), text("x")]
        ),
        text("axc")
    );
    // An integer slot types the parameter i64; a wrong-family value is a binding error.
    assert_eq!(
        one(
            &mut db,
            "SELECT left($1, $2)",
            &[text("abcd"), Value::Int(2)]
        ),
        text("ab")
    );
    assert_eq!(
        err_code(&mut db, "SELECT left($1, $2)", &[text("abcd"), text("2")]),
        "42804"
    );
    assert_eq!(one(&mut db, "SELECT chr($1)", &[Value::Int(65)]), text("A"));
    // length/substr have text and bytea overloads: the unknown argument takes text.
    assert_eq!(
        one(&mut db, "SELECT length($1)", &[text("héllo")]),
        Value::Int(5)
    );
    assert_eq!(
        err_code(&mut db, "SELECT length($1)", &[Value::Bytea(vec![1, 2])]),
        "42804"
    );
    assert_eq!(
        one(&mut db, "SELECT substr($1, 2)", &[text("abc")]),
        text("bc")
    );
    // A typed argument narrows the candidates first: bytea picks the bytea overload.
    assert_eq!(
        one(
            &mut db,
            "SELECT substr('\\x010203'::bytea, $1)",
            &[Value::Int(2)]
        ),
        Value::Bytea(vec![2, 3])
    );
    // A family other than text/integer: encode(bytea, text), json_typeof(json).
    assert_eq!(
        one(
            &mut db,
            "SELECT encode($1, 'hex')",
            &[Value::Bytea(vec![0xab])]
        ),
        text("ab")
    );
    assert_eq!(
        one(&mut db, "SELECT json_typeof($1)", &[text("[1]")]),
        text("array")
    );
    // A NULL bound to an adopted slot propagates.
    assert_eq!(
        one(&mut db, "SELECT translate($1, 'a', 'b')", &[Value::Null]),
        Value::Null
    );
    // The parameter's adopted type unifies with its other uses.
    assert_eq!(
        one(
            &mut db,
            "SELECT left($1, 1) FROM t WHERE s = $1",
            &[text("xyz")]
        ),
        text("x")
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT left($1, 1) FROM t WHERE id = $1",
            &[text("x")]
        ),
        "42804"
    );
    // Overloads the rule cannot decide among are 42725; a cast chooses one.
    for sql in [
        "SELECT abs($1)",
        "SELECT round($1, 2)",
        "SELECT sqrt($1)",
        "SELECT pow($1, $2)",
        "SELECT date_part('year', $1)",
    ] {
        assert_eq!(
            err_code(&mut db, sql, &[Value::Int(1), Value::Int(1)]),
            "42725",
            "{sql}"
        );
    }
    assert_eq!(
        one(&mut db, "SELECT abs($1::i32)", &[Value::Int(-3)]),
        Value::Int(3)
    );
    // An `any` slot fixes no type.
    assert_eq!(
        err_code(&mut db, "SELECT to_jsonb($1)", &[text("x")]),
        "42P18"
    );
    // No candidate stays 42883.
    assert_eq!(
        err_code(&mut db, "SELECT left($1, 'x')", &[text("abc")]),
        "42883"
    );
}

/// A $N operand of LIKE/ILIKE or a regex match adopts text whatever its sibling (api.md §5).
#[test]
fn like_and_regex_param_operand_is_text() {
    let mut db = db_with(&[]);
    let text = |s: &str| Value::Text(s.to_string());
    let one = |db: &mut Session, sql: &str, params: &[Value]| rows(db, sql, params)[0][0].clone();
    assert_eq!(
        one(&mut db, "SELECT 'x%' LIKE $1", &[text("x\\%")]),
        Value::Bool(true)
    );
    assert_eq!(
        one(&mut db, "SELECT $1 LIKE $2", &[text("abc"), text("a%")]),
        Value::Bool(true)
    );
    assert_eq!(
        one(&mut db, "SELECT $1 ILIKE 'A%'", &[text("abc")]),
        Value::Bool(true)
    );
    assert_eq!(
        one(&mut db, "SELECT 'abc' ~ $1", &[text("^a.c$")]),
        Value::Bool(true)
    );
    assert_eq!(
        one(&mut db, "SELECT $1 !~ $2", &[text("abc"), text("z")]),
        Value::Bool(true)
    );
    assert_eq!(
        one(&mut db, "SELECT 'x' LIKE $1", &[Value::Null]),
        Value::Null
    );
    assert_eq!(
        err_code(&mut db, "SELECT 'x' LIKE $1", &[Value::Int(1)]),
        "42804"
    );
    // A non-text sibling is still a LIKE type error.
    assert_eq!(err_code(&mut db, "SELECT 5 LIKE $1", &[text("5")]), "42804");
}
