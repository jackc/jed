//! Bound parameters (`$N`) — spec/design/api.md §5. Typing, binding, and the errors they raise are
//! pinned once for every core by the shared corpus (suites/params, the `# params:` directive —
//! conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a
//! binary float, raw bytes, a range value) and the ergonomic `Array` mapping.

use jed::value::{RangeVal, Value};
use jed::{Array, CreateOptions, Database, Outcome, Session, SessionOptions};

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

/// A bytea host value binds to a bytea slot and is the wrong family for a text one.
#[test]
fn bytea_host_value_binds_to_adopted_slot() {
    let mut db = db_with(&[]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT encode($1, 'hex')",
            &[Value::Bytea(vec![0xab])]
        ),
        vec![vec![Value::Text("ab".to_string())]]
    );
    assert_eq!(
        err_code(&mut db, "SELECT length($1)", &[Value::Bytea(vec![1, 2])]),
        "42804"
    );
}

/// A range host value binds canonicalized, as a range constructor's result is (api.md §5): an
/// inclusive discrete upper bound moves to the canonical `[)` form, and crossed bounds are 22000.
#[test]
fn range_host_value_binds_canonicalized() {
    let mut db = db_with(&[]);
    let range = |lower: i64, upper: i64, upper_inc: bool| {
        Value::Range(RangeVal {
            empty: false,
            lower: Some(Box::new(Value::Int(lower))),
            upper: Some(Box::new(Value::Int(upper))),
            lower_inc: true,
            upper_inc,
        })
    };
    let got = rows(&mut db, "SELECT $1::i32range", &[range(1, 3, true)]);
    assert_eq!(got[0][0].render(), "[1,4)");
    assert_eq!(
        err_code(&mut db, "SELECT $1::i32range", &[range(3, 1, false)]),
        "22000"
    );
}

/// `Array(vec![…])` binds a list as one array parameter (api.md §11).
#[test]
fn array_newtype_binds_as_array() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, tags text[])",
        "INSERT INTO t VALUES (1, '{a}'), (2, '{b}'), (3, '{a,b}')",
    ]);
    let ids: Vec<i64> = db
        .query_map(
            "SELECT id FROM t WHERE id = ANY($1) ORDER BY id",
            (Array(vec![1i64, 3]),),
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(ids, vec![1, 3]);
    let n: Option<i64> = db
        .query_row(
            "SELECT count(*) FROM t WHERE tags @> $1",
            (Array(vec!["a"]),),
            |row| row.get::<i64>(0),
        )
        .unwrap();
    assert_eq!(n, Some(2));
}
