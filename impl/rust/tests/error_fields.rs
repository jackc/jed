//! Structured error fields (spec/design/error-fields.md) — the `constraint_name` /
//! `table_name` / `column_name` / `data_type_name` diagnostics on `EngineError`, modeled on
//! pgx's `pgconn.PgError`. Out of the conformance corpus's reach (it matches on `code`/prose,
//! never on a structured field — CLAUDE.md §10), so this is the host-API surface test.
//! Mirrored in impl/go/error_fields_test.go and impl/ts/tests/error_fields.test.ts.

use jed::{CreateOptions, Database, EngineError, Session, SessionOptions};

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

fn err(db: &mut Session, sql: &str) -> EngineError {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
}

/// 23505 on a PRIMARY KEY reports the derived `<table>_pkey` constraint + the table; the
/// rendered message is unchanged (fields are additive metadata, not a message change).
#[test]
fn unique_violation_primary_key() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    let e = err(&mut db, "INSERT INTO t VALUES (1)");
    assert_eq!(e.code(), "23505");
    assert_eq!(e.constraint_name.as_deref(), Some("t_pkey"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
    assert_eq!(e.column_name, None);
    assert_eq!(
        e.message,
        "duplicate key value violates unique constraint: t_pkey"
    );
}

/// 23505 on a named UNIQUE index reports the index (= constraint) name.
#[test]
fn unique_violation_secondary_index() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, email text)",
        "CREATE UNIQUE INDEX t_email_key ON t (email)",
        "INSERT INTO t VALUES (1, 'a')",
    ]);
    let e = err(&mut db, "INSERT INTO t VALUES (2, 'a')");
    assert_eq!(e.code(), "23505");
    assert_eq!(e.constraint_name.as_deref(), Some("t_email_key"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
}

/// 23514 reports the CHECK constraint + the relation.
#[test]
fn check_violation() {
    let mut db =
        db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, n i32 CONSTRAINT n_pos CHECK (n > 0))"]);
    let e = err(&mut db, "INSERT INTO t VALUES (1, -1)");
    assert_eq!(e.code(), "23514");
    assert_eq!(e.constraint_name.as_deref(), Some("n_pos"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
}

/// 23503 (child side) reports the FK constraint + the written table.
#[test]
fn foreign_key_violation_insert() {
    let mut db = db_with(&[
        "CREATE TABLE p (id i32 PRIMARY KEY)",
        "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
    ]);
    let e = err(&mut db, "INSERT INTO c VALUES (1, 99)");
    assert_eq!(e.code(), "23503");
    assert_eq!(e.constraint_name.as_deref(), Some("c_pid_fk"));
    assert_eq!(e.table_name.as_deref(), Some("c"));
}

/// 23503 (parent side) reports the FK constraint + the modified (parent) table.
#[test]
fn foreign_key_violation_delete() {
    let mut db = db_with(&[
        "CREATE TABLE p (id i32 PRIMARY KEY)",
        "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1, 1)",
    ]);
    let e = err(&mut db, "DELETE FROM p WHERE id = 1");
    assert_eq!(e.code(), "23503");
    assert_eq!(e.constraint_name.as_deref(), Some("c_pid_fk"));
    assert_eq!(e.table_name.as_deref(), Some("p"));
}

/// 23P01 reports the EXCLUDE constraint (its backing GiST index name) + the table.
#[test]
fn exclusion_violation() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, \
         CONSTRAINT t_r_excl EXCLUDE USING gist (r WITH &&))",
        "INSERT INTO t VALUES (1, '[1,5)')",
    ]);
    let e = err(&mut db, "INSERT INTO t VALUES (2, '[3,8)')");
    assert_eq!(e.code(), "23P01");
    assert_eq!(e.constraint_name.as_deref(), Some("t_r_excl"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
}

/// 23502 reports the column (unnamed constraint, as in PostgreSQL); the table is stamped at
/// the DML boundary.
#[test]
fn not_null_violation() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, n i32 NOT NULL)"]);
    let e = err(&mut db, "INSERT INTO t VALUES (1, NULL)");
    assert_eq!(e.code(), "23502");
    assert_eq!(e.column_name.as_deref(), Some("n"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
    assert_eq!(e.constraint_name, None);
}

/// 22003 (integer overflow on column store) reports the data type + the table.
#[test]
fn numeric_value_out_of_range() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, n i16)"]);
    let e = err(&mut db, "INSERT INTO t VALUES (1, 99999)");
    assert_eq!(e.code(), "22003");
    assert_eq!(e.data_type_name.as_deref(), Some("i16"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
}

/// 22001 (varchar length) reports the type + the column.
#[test]
fn string_data_right_truncation() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY, s varchar(3))"]);
    let e = err(&mut db, "INSERT INTO t VALUES (1, 'abcd')");
    assert_eq!(e.code(), "22001");
    assert_eq!(e.data_type_name.as_deref(), Some("varchar(3)"));
    assert_eq!(e.column_name.as_deref(), Some("s"));
    assert_eq!(e.table_name.as_deref(), Some("t"));
}

/// A non-constraint error leaves every structured field unset.
#[test]
fn unrelated_error_has_no_fields() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let e = err(&mut db, "SELECT nonesuch FROM t");
    assert_eq!(e.constraint_name, None);
    assert_eq!(e.table_name, None);
    assert_eq!(e.column_name, None);
    assert_eq!(e.data_type_name, None);
}
