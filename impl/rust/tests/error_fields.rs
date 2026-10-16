//! Structured error fields (spec/design/error-fields.md) — the `constraint_name` /
//! `table_name` / `column_name` / `data_type_name` diagnostics on `EngineError`, modeled on
//! pgx's `pgconn.PgError`. Out of the conformance corpus's reach (it matches on `code`/prose,
//! never on a structured field — CLAUDE.md §10), so this is the host-API surface test.
//! Mirrored in impl/go/error_fields_test.go and impl/ts/tests/error_fields.test.ts.

use jed::{CreateOptions, Database, EngineError, Privilege, PrivilegeSet, Session, SessionOptions};

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
    assert_eq!(e.detail, None);
    assert_eq!(e.hint, None);
}

/// A prepared statement resolves at execute; its name errors still point into the prepared text
/// (spec/design/error-fields.md §10.1).
#[test]
fn prepared_statement_error_has_position() {
    let mut db = db_with(&["CREATE TABLE t (name i32)"]);
    let stmt = db.prepare("SELECT 1, nme FROM t").unwrap();
    let e = db.execute_prepared(&stmt, &[]).unwrap_err();
    assert_eq!(e.code(), "42703");
    assert_eq!(e.position, Some(11));
    assert_eq!(
        e.hint.as_deref(),
        Some("Perhaps you meant to reference the column \"t.name\".")
    );
}

/// An error that is not about a place in the query carries no position or hint, and stored text
/// (a persisted expression) is parsed without positions (spec/design/error-fields.md §10.1/§10.2).
#[test]
fn position_absent_off_the_named_paths() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let e = err(&mut db, "SELECT 1 / 0");
    assert_eq!(
        (e.code(), e.position, e.hint.clone()),
        ("22012", None, None)
    );
    let e = jed::parser::parse_expression("'abc").unwrap_err();
    assert_eq!((e.code(), e.position), ("42601", None));
    let e = jed::parser::parse_expression("a +").unwrap_err();
    assert_eq!((e.code(), e.position), ("42601", None));
}

/// One case of the shared Detail fixture (spec/errors/detail_cases.txt).
#[derive(Default)]
struct DetailCase {
    name: String,
    setup: Vec<String>,
    default_privileges: Option<String>,
    grants: Vec<(String, String)>,
    revokes: Vec<(String, String)>,
    run: String,
    code: String,
    detail: Option<String>,
}

fn parse_detail_cases(text: &str) -> Vec<DetailCase> {
    let mut cases: Vec<DetailCase> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("case: ") {
            cases.push(DetailCase {
                name: name.to_string(),
                ..DetailCase::default()
            });
            continue;
        }
        let c = cases.last_mut().expect("a directive before the first case");
        let priv_obj = |rest: &str| {
            let (p, o) = rest.split_once(' ').expect("PRIV object");
            (p.to_string(), o.to_string())
        };
        if let Some(v) = line.strip_prefix("setup: ") {
            c.setup.push(v.to_string());
        } else if let Some(v) = line.strip_prefix("default_privileges: ") {
            c.default_privileges = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("grant: ") {
            c.grants.push(priv_obj(v));
        } else if let Some(v) = line.strip_prefix("revoke: ") {
            c.revokes.push(priv_obj(v));
        } else if let Some(v) = line.strip_prefix("run: ") {
            c.run = v.to_string();
        } else if let Some(v) = line.strip_prefix("code: ") {
            c.code = v.to_string();
        } else if let Some(v) = line.strip_prefix("detail: ") {
            c.detail = Some(v.to_string());
        } else if line != "no-detail" {
            panic!("unknown detail_cases directive: {line}");
        }
    }
    cases
}

fn privilege_set(list: &str) -> PrivilegeSet {
    if list == "-" {
        return PrivilegeSet::EMPTY;
    }
    list.split(',').fold(PrivilegeSet::EMPTY, |set, p| {
        set.with(Privilege::from_name(p.trim()).expect("privilege name"))
    })
}

/// The `Detail` field (spec/design/error-fields.md §9): every case of the shared, language-neutral
/// fixture — the same file the Go and TS cores run, so the text is pinned byte-identical.
#[test]
fn detail_shared_cases() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spec/errors/detail_cases.txt"
    );
    let text = std::fs::read_to_string(path).expect("read detail_cases.txt");
    let cases = parse_detail_cases(&text);
    assert!(!cases.is_empty());
    let mut failures = Vec::new();
    for c in &cases {
        let setup: Vec<&str> = c.setup.iter().map(String::as_str).collect();
        let mut db = db_with(&setup);
        if let Some(list) = &c.default_privileges {
            db.set_default_privileges(privilege_set(list));
        }
        for (p, o) in &c.grants {
            db.grant(privilege_set(p), o);
        }
        for (p, o) in &c.revokes {
            db.revoke(privilege_set(p), o);
        }
        match db.query_outcome(&c.run, &[]) {
            Ok(_) => failures.push(format!("{}: expected {} but succeeded", c.name, c.code)),
            Err(e) => {
                if e.code() != c.code || e.detail != c.detail {
                    failures.push(format!(
                        "{}: got {} {:?} ({}), want {} {:?}",
                        c.name,
                        e.code(),
                        e.detail,
                        e.message,
                        c.code,
                        c.detail
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One case of the shared Position/Hint fixture (spec/errors/position_cases.txt).
#[derive(Default)]
struct PositionCase {
    name: String,
    setup: Vec<String>,
    run: String,
    script: bool,
    code: String,
    position: Option<u32>,
    hint: Option<String>,
}

fn parse_position_cases(text: &str) -> Vec<PositionCase> {
    let mut cases: Vec<PositionCase> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("case: ") {
            cases.push(PositionCase {
                name: name.to_string(),
                ..PositionCase::default()
            });
            continue;
        }
        let c = cases.last_mut().expect("a directive before the first case");
        if let Some(v) = line.strip_prefix("setup: ") {
            c.setup.push(v.to_string());
        } else if let Some(v) = line.strip_prefix("run: ") {
            c.run = v.to_string();
        } else if let Some(v) = line.strip_prefix("script: ") {
            c.run = v.to_string();
            c.script = true;
        } else if let Some(v) = line.strip_prefix("code: ") {
            c.code = v.to_string();
        } else if let Some(v) = line.strip_prefix("position: ") {
            c.position = Some(v.parse().expect("a position number"));
        } else if let Some(v) = line.strip_prefix("hint: ") {
            c.hint = Some(v.to_string());
        } else if line != "no-position" {
            panic!("unknown position_cases directive: {line}");
        }
    }
    cases
}

/// The `Position` and `Hint` fields (spec/design/error-fields.md §10, §11): every case of the
/// shared, language-neutral fixture — the same file the Go and TS cores run.
#[test]
fn position_hint_shared_cases() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spec/errors/position_cases.txt"
    );
    let text = std::fs::read_to_string(path).expect("read position_cases.txt");
    let cases = parse_position_cases(&text);
    assert!(!cases.is_empty());
    let mut failures = Vec::new();
    for c in &cases {
        let setup: Vec<&str> = c.setup.iter().map(String::as_str).collect();
        let mut db = db_with(&setup);
        let result = if c.script {
            db.execute_script(&c.run).map(|_| ())
        } else {
            db.query_outcome(&c.run, &[]).map(|_| ())
        };
        match result {
            Ok(()) => failures.push(format!("{}: expected {} but succeeded", c.name, c.code)),
            Err(e) => {
                if e.code() != c.code || e.position != c.position || e.hint != c.hint {
                    failures.push(format!(
                        "{}: got {} {:?} {:?} ({}), want {} {:?} {:?}",
                        c.name,
                        e.code(),
                        e.position,
                        e.hint,
                        e.message,
                        c.code,
                        c.position,
                        c.hint
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
