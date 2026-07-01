//! FOREIGN KEY constraints — `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES …` and the
//! column-level `REFERENCES` (spec/design/constraints.md §6, grammar.md §43). Covers what the
//! oracle corpus (`ddl/foreign_key.test`) cannot: the jed-specific divergences from PostgreSQL
//! (strict same-type pairing, the deferred referential actions, the end-state parent UPDATE), and
//! catalog introspection (constraint names, the resolved ordinals). The agreeing behavior — the
//! 23503 enforcement at every write site, MATCH SIMPLE, the batch end state, 42830/2BP01 — is the
//! corpus's job. Mirrored in impl/go/foreign_key_test.go and impl/ts/tests/foreign_key.test.ts.

use jed::{Database, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    for s in sql {
        db.execute(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

fn fk_names(db: &Session, table: &str) -> Vec<String> {
    db.table(table)
        .unwrap()
        .foreign_keys
        .iter()
        .map(|f| f.name.clone())
        .collect()
}

/// Auto-naming follows PostgreSQL's `<table>_<localcols>_fkey`; an explicit `CONSTRAINT` name is
/// used as written; the catalog holds FKs in ascending lowercased-name order.
#[test]
fn naming_and_catalog_order() {
    let db = db_with(&[
        "CREATE TABLE p (a i32, b i32, code i32 UNIQUE, PRIMARY KEY (a, b))",
        "CREATE TABLE c (id i32 PRIMARY KEY, pa i32, pb i32, pcode i32, \
         CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code), \
         FOREIGN KEY (pa, pb) REFERENCES p (a, b))",
    ]);
    // c_code_fk (explicit) < c_pa_pb_fkey (auto: <table>_<col>_<col>_fkey) — ascending name order.
    assert_eq!(fk_names(&db, "c"), vec!["c_code_fk", "c_pa_pb_fkey"]);

    // A duplicate auto-name walks the suffix (two FKs on the same column).
    let db2 = db_with(&[
        "CREATE TABLE q (id i32 PRIMARY KEY)",
        "CREATE TABLE r (id i32 PRIMARY KEY, x i32 REFERENCES q, FOREIGN KEY (x) REFERENCES q (id))",
    ]);
    assert_eq!(fk_names(&db2, "r"), vec!["r_x_fkey", "r_x_fkey1"]);
}

/// jed is STRICTER than PostgreSQL on type pairing: corresponding columns must be the SAME scalar
/// type (42804), where PG allows any comparable pair (e.g. i32 ↔ i64). A documented divergence
/// (spec/design/constraints.md §6.7).
#[test]
fn strict_same_type_pairing() {
    let mut db = db_with(&["CREATE TABLE p (id i32 PRIMARY KEY)"]);
    // i64 referencing an i32 PK — jed rejects (PG would allow).
    assert_eq!(
        err(&mut db, "CREATE TABLE c1 (x i64 REFERENCES p)"),
        "42804"
    );
    // text referencing an i32 PK — both jed and PG reject 42804 (sanity).
    assert_eq!(
        err(&mut db, "CREATE TABLE c2 (x text REFERENCES p)"),
        "42804"
    );
    // The same type is accepted.
    db.execute("CREATE TABLE c3 (x i32 REFERENCES p)", &[])
        .unwrap();
}

/// The referential actions CASCADE / SET NULL / SET DEFAULT parse but are rejected at CREATE TABLE
/// (0A000); NO ACTION and RESTRICT are accepted (spec/design/constraints.md §6.6).
#[test]
fn referential_actions_narrowed() {
    let mut db = db_with(&["CREATE TABLE p (id i32 PRIMARY KEY)"]);
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE c1 (x i32 REFERENCES p ON DELETE CASCADE)"
        ),
        "0A000"
    );
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE c2 (x i32 REFERENCES p ON UPDATE SET NULL)"
        ),
        "0A000"
    );
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE c3 (x i32 REFERENCES p ON DELETE SET DEFAULT)"
        ),
        "0A000"
    );
    // NO ACTION / RESTRICT (and the default) are fine.
    db.execute(
        "CREATE TABLE c4 (x i32 REFERENCES p ON DELETE NO ACTION ON UPDATE RESTRICT)",
        &[],
    )
    .unwrap();
}

/// jed validates the parent side against the statement's END STATE, like UNIQUE: a swap of two
/// referenced UNIQUE values keeps every referenced tuple present, so children stay valid and the
/// UPDATE succeeds — where PostgreSQL's per-row check fails on the transient (a documented
/// divergence, spec/design/constraints.md §6.7).
#[test]
fn parent_update_end_state_swap_allowed() {
    let mut db = db_with(&[
        "CREATE TABLE p (id i32 PRIMARY KEY, code i32 UNIQUE)",
        "INSERT INTO p VALUES (1, 100), (2, 200)",
        "CREATE TABLE c (id i32 PRIMARY KEY, pc i32 REFERENCES p (code))",
        "INSERT INTO c VALUES (10, 100), (11, 200)",
    ]);
    // Swap 100 ⇄ 200 across the two parent rows: the end state still contains {100, 200}, so both
    // children remain valid. jed accepts this (PG would reject the transient collision).
    db.execute(
        "UPDATE p SET code = CASE code WHEN 100 THEN 200 ELSE 100 END",
        &[],
    )
    .unwrap();
    // But genuinely removing a referenced value still traps 23503.
    assert_eq!(
        err(&mut db, "UPDATE p SET code = 999 WHERE id = 1"),
        "23503"
    );
}
