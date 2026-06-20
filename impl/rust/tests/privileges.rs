//! S3 session privileges — the host-API surface (spec/design/session.md §5.3). The SQL-observable
//! `42501` behavior (every table/function/DDL gate) is corpus-tested across all three cores
//! (`suites/session/privileges.test`); these per-core tests cover what the single-statement corpus
//! cannot *call*: configuring the envelope through the Rust host API directly, the value-level
//! `Privilege`/`PrivilegeSet` surface, the per-session independence of an additional session, and
//! the introspection accessors (CLAUDE.md §10).

use jed::{Database, Outcome, Privilege, PrivilegeSet, SessionOptions};

fn code(db: &mut Database, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
        .code()
        .to_string()
}

fn ok(db: &mut Database, sql: &str) -> Outcome {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("expected ok from {sql}, got {}: {}", e.code(), e.message))
}

#[test]
fn default_session_is_fully_permissive() {
    // A fresh Database's default session grants every table privilege and allows DDL, so nothing is
    // gated until the host narrows the envelope.
    let mut db = Database::new();
    assert!(db.allow_ddl());
    assert!(db.privileges().is_permissive());
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    ok(&mut db, "UPDATE t SET v = 20 WHERE id = 1");
    ok(&mut db, "DELETE FROM t WHERE id = 1");
}

#[test]
fn set_default_privileges_makes_a_read_only_session() {
    // A {SELECT} default is the read-only session (§5.3): reads pass, every write is 42501.
    let mut db = Database::new();
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");
    db.set_default_privileges(PrivilegeSet::EMPTY.with(Privilege::Select));
    ok(&mut db, "SELECT v FROM t WHERE id = 1");
    assert_eq!(code(&mut db, "INSERT INTO t VALUES (2, 20)"), "42501");
    assert_eq!(code(&mut db, "UPDATE t SET v = 0 WHERE id = 1"), "42501");
    assert_eq!(code(&mut db, "DELETE FROM t WHERE id = 1"), "42501");
}

#[test]
fn grant_adds_and_revoke_wins() {
    // grant adds a privilege beyond an empty default; revoke beats a contradictory grant (deny
    // wins, order-independent — §5.3).
    let mut db = Database::new();
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");

    db.set_default_privileges(PrivilegeSet::EMPTY);
    db.grant(PrivilegeSet::EMPTY.with(Privilege::Insert), "t");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)"); // bare INSERT needs only INSERT

    // Revoking what was granted denies it (deny wins regardless of the grant).
    db.revoke(PrivilegeSet::EMPTY.with(Privilege::Insert), "t");
    assert_eq!(code(&mut db, "INSERT INTO t VALUES (2, 20)"), "42501");

    // The introspection accessor reflects the effective set.
    assert!(!db.privileges().allows_table("t", Privilege::Insert));
}

#[test]
fn allow_ddl_gate_is_independent_of_table_privileges() {
    // allow_ddl gates only schema changes; DML over a permissive default still runs.
    let mut db = Database::new();
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    db.set_allow_ddl(false);
    assert_eq!(
        code(&mut db, "CREATE TABLE u (id i32 PRIMARY KEY)"),
        "42501"
    );
    assert_eq!(code(&mut db, "DROP TABLE t"), "42501");
    ok(&mut db, "INSERT INTO t VALUES (1, 10)"); // DML untouched
}

#[test]
fn function_execute_is_revocable() {
    // Functions default to EXECUTE on all; a revoke blocks calls to that one (the determinism-
    // pinning use case), operators stay ungated.
    let mut db = Database::new();
    assert!(db.privileges().allows_function("abs"));
    ok(&mut db, "SELECT abs(-5)");
    db.revoke(PrivilegeSet::EMPTY.with(Privilege::Execute), "abs");
    assert!(!db.privileges().allows_function("abs"));
    assert_eq!(code(&mut db, "SELECT abs(-5)"), "42501");
    ok(&mut db, "SELECT 1 + 2"); // the + operator is not a named function — never gated
}

#[test]
fn an_additional_session_carries_its_own_envelope() {
    // db.session(opts) mints an independent session: a restricted one rejects a write the permissive
    // default still allows, and the two share committed storage (spec/design/session.md §2.1/§5.3).
    let mut db = Database::new();
    ok(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");

    let mut restricted = db.session(SessionOptions {
        default_privileges: PrivilegeSet::EMPTY.with(Privilege::Select),
        ..SessionOptions::default()
    });
    // The restricted session may read but not write.
    restricted
        .execute(&mut db, "SELECT * FROM t", &[])
        .expect("read allowed on the restricted session");
    let err = restricted
        .execute(&mut db, "INSERT INTO t VALUES (1, 10)", &[])
        .err()
        .unwrap();
    assert_eq!(err.code(), "42501");

    // The default session is unaffected — it still writes.
    ok(&mut db, "INSERT INTO t VALUES (1, 10)");

    // A grant on the additional session lifts the restriction for it alone.
    restricted.grant(PrivilegeSet::EMPTY.with(Privilege::Insert), "t");
    restricted
        .execute(&mut db, "INSERT INTO t VALUES (2, 20)", &[])
        .expect("insert allowed after grant on the restricted session");
}

#[test]
fn missing_object_is_42p01_not_42501() {
    // Authorization gates only resolved objects: a missing table is 42P01 even under an empty
    // envelope (existence before authorization — §5.3).
    let mut db = Database::new();
    db.set_default_privileges(PrivilegeSet::EMPTY);
    assert_eq!(code(&mut db, "SELECT * FROM does_not_exist"), "42P01");
}
