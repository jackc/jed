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
fn param_cast_to_deferred_target_is_0a000() {
    // Casting a parameter to a deferred target (text) is 0A000, like any non-string-literal
    // cast to text — the `::` operator adds no behavior of its own beyond the spelling.
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    assert_eq!(
        err_code(&mut db, "SELECT $1::text", &[Value::Int(1)]),
        "0A000"
    );
}

#[test]
fn cast_operator_inherits_deferred_narrowings_and_rejects_lone_colon() {
    // `::` desugars to CAST, so casting a non-string-literal value to text is the same deferred
    // 0A000 narrowing the CAST spelling carries (a documented PG divergence). The boolean cast has
    // since landed — `5::boolean` is now valid (→ true; tests/cast_bool_int.rs).
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    assert_eq!(err_code(&mut db, "SELECT 5::text", &[]), "0A000");
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
