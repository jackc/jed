//! Bound parameters (`$N`) — spec/design/api.md §5. Typing, binding, and the errors they raise are
//! pinned once for every core by the shared corpus (suites/params, the `# params:` directive —
//! conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a
//! binary float, raw bytes).

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
