//! Any built-in scalar → text (spec/types/casts.toml, spec/design/types.md §5) — only what the
//! PG-clean oracle corpus cannot hold. The rendering of every type, the session-zone timestamptz
//! form, the 42P17 index rejection, and element-wise `T[] → text[]` agree with PostgreSQL and live in
//! `suites/cast/scalar_to_text.test`. Here: the casts are EXPLICIT only (an assignment or implicit
//! use is 42804 where PG accepts it), range → text stays 0A000 (PG renders it), and the
//! `$1::text` parameter declaration. Mirrors impl/go/cast_scalar_text_test.go.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn db() -> Session {
    Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default())
}

fn exec(db: &mut Session, sql: &str) {
    db.query_outcome(sql, &[]).unwrap();
}

fn err_code(db: &mut Session, sql: &str, params: &[Value]) -> String {
    match db.query_outcome(sql, params) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

fn one(db: &mut Session, sql: &str, params: &[Value]) -> Value {
    match db.query_outcome(sql, params).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

/// Only the explicit spelling converts: storing or comparing a non-text value against text stays a
/// 42804 datatype mismatch (PostgreSQL assignment-casts to text and would accept the INSERT).
#[test]
fn text_cast_is_explicit_only() {
    let mut db = db();
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)");
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, 42)", &[]),
        "42804"
    );
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, true)", &[]),
        "42804"
    );
    assert_eq!(
        err_code(&mut db, "INSERT INTO t SELECT 1, date '2024-01-02'", &[]),
        "42804"
    );
    assert_eq!(
        err_code(&mut db, "SELECT id FROM t WHERE s = 42", &[]),
        "42804"
    );
    exec(&mut db, "INSERT INTO t SELECT 1, 42::text");
    assert_eq!(
        one(&mut db, "SELECT s FROM t WHERE s = 42::text", &[]),
        Value::Text("42".into())
    );
}

/// A range value has no text cast yet (0A000), though PostgreSQL renders it (composite → text
/// landed — cast/composite_casts.test).
#[test]
fn range_to_text_is_0a000() {
    let mut db = db();
    assert_eq!(
        err_code(&mut db, "SELECT ('[1,5)'::i32range)::text", &[]),
        "0A000"
    );
}

/// `$1::text` declares the parameter as text (the cast-target parameter typing, api.md §5); a
/// `varchar(n)` target truncates the bound value.
#[test]
fn param_cast_to_text_declares_text() {
    let mut db = db();
    assert_eq!(
        one(&mut db, "SELECT $1::text", &[Value::Text("abc".into())]),
        Value::Text("abc".into())
    );
    assert_eq!(
        one(
            &mut db,
            "SELECT $1::varchar(2)",
            &[Value::Text("abc".into())]
        ),
        Value::Text("ab".into())
    );
    assert_eq!(one(&mut db, "SELECT $1::text", &[Value::Null]), Value::Null);
}
