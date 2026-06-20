//! S5 session variables — the host-API surface (spec/design/session.md §6.1). The SQL-observable
//! `current_setting` behavior (a set variable read back, the 42704-on-unset, missing_ok → NULL, the
//! per-record reset) is corpus-tested across all three cores (`suites/session/variables.test`); these
//! per-core tests cover what the directive-driven corpus cannot *call* or *observe*: the host setters
//! and getter (`set_var`/`reset_var`/`var`), the `42704` rejection of a non-dotted name, case folding
//! at the host API, NULL propagation through a text-typed NULL value, that variables are **session
//! state, not snapshot state** (they do not roll back with a transaction), an additional session's
//! independent variables, and `reset_vars` (PG `RESET ALL`). CLAUDE.md §10.

use jed::value::Value;
use jed::{Database, Outcome, SessionOptions, execute};

/// Run a single-row, single-column query and return the lone value.
fn scalar(db: &mut Database, sql: &str) -> Value {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql:?}: expected one row");
            assert_eq!(rows[0].len(), 1, "{sql:?}: expected one column");
            rows[0][0].clone()
        }
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
        .code()
        .to_string()
}

#[test]
fn host_set_and_read_round_trip() {
    // set_var stores; var() reads it back through the host API; current_setting reads it in SQL.
    let mut db = Database::new();
    assert_eq!(db.var("myapp.tenant"), None); // unset
    db.set_var("myapp.tenant", "acme").unwrap();
    assert_eq!(db.var("myapp.tenant"), Some("acme".to_string()));
    assert_eq!(
        scalar(&mut db, "SELECT current_setting('myapp.tenant')"),
        Value::Text("acme".to_string())
    );
}

#[test]
fn set_and_reset_var_reject_a_non_dotted_name() {
    // A variable must be namespaced (dotted) — a non-dotted name is a built-in setting name, and v1
    // exposes none through this map (the time_zone built-in is its own slice), so it is 42704.
    let mut db = Database::new();
    assert_eq!(db.set_var("bogus", "x").err().unwrap().code(), "42704");
    assert_eq!(db.reset_var("bogus").err().unwrap().code(), "42704");
    // The host getter never errors — a non-dotted (or any unset) name simply reads as None.
    assert_eq!(db.var("bogus"), None);
}

#[test]
fn reset_var_removes_and_is_idempotent() {
    let mut db = Database::new();
    db.set_var("myapp.k", "v").unwrap();
    db.reset_var("myapp.k").unwrap();
    assert_eq!(db.var("myapp.k"), None);
    // current_setting on the now-unset name is 42704 again.
    assert_eq!(
        err_code(&mut db, "SELECT current_setting('myapp.k')"),
        "42704"
    );
    // Resetting an unset variable is a no-op success (PG RESET of an unset custom variable).
    db.reset_var("myapp.k").unwrap();
}

#[test]
fn names_are_case_insensitive_but_values_are_verbatim() {
    // The NAME folds to lowercase (PG GUC names are case-insensitive); the VALUE is preserved exactly.
    let mut db = Database::new();
    db.set_var("myApp.Tenant", "AcmeCorp").unwrap();
    assert_eq!(db.var("myapp.tenant"), Some("AcmeCorp".to_string()));
    assert_eq!(db.var("MYAPP.TENANT"), Some("AcmeCorp".to_string()));
    assert_eq!(
        scalar(&mut db, "SELECT current_setting('MyApp.TENANT')"),
        Value::Text("AcmeCorp".to_string())
    );
}

#[test]
fn missing_ok_turns_the_unset_error_into_null() {
    let mut db = Database::new();
    assert_eq!(
        err_code(&mut db, "SELECT current_setting('myapp.unset')"),
        "42704"
    );
    assert_eq!(
        scalar(&mut db, "SELECT current_setting('myapp.unset', true)"),
        Value::Null
    );
    // false behaves like the one-arg form.
    assert_eq!(
        err_code(&mut db, "SELECT current_setting('myapp.unset', false)"),
        "42704"
    );
}

#[test]
fn a_null_name_propagates_to_null() {
    // null = "propagates": a NULL name short-circuits to NULL before the lookup. A text column holding
    // a NULL is the typed-NULL the corpus cannot write (jed defers text casts, so no NULL::text yet).
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, n text)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, NULL)").unwrap();
    db.set_var("myapp.x", "set").unwrap();
    assert_eq!(
        scalar(&mut db, "SELECT current_setting(n) FROM t WHERE id = 1"),
        Value::Null
    );
}

#[test]
fn variables_are_session_state_not_snapshot_state() {
    // Variables are SESSION state, not snapshot state (§6.1): a ROLLBACK undoes DATA but never a
    // session variable (PG SET SESSION). Set one outside, one inside a block, roll back — both survive.
    let mut db = Database::new();
    db.set_var("myapp.outer", "a").unwrap();
    execute(&mut db, "BEGIN").unwrap();
    db.set_var("myapp.inner", "b").unwrap();
    execute(&mut db, "ROLLBACK").unwrap();
    assert_eq!(db.var("myapp.outer"), Some("a".to_string()));
    assert_eq!(db.var("myapp.inner"), Some("b".to_string()));
    assert_eq!(
        scalar(&mut db, "SELECT current_setting('myapp.inner')"),
        Value::Text("b".to_string())
    );
}

#[test]
fn an_additional_session_has_independent_variables() {
    // db.session(opts) mints an independent session (§2.1): its variable map is its own — a variable
    // set on it is invisible to the default session and vice versa.
    let mut db = Database::new();
    db.set_var("myapp.who", "default").unwrap();

    let mut other = db.session(SessionOptions::default());
    other.set_var("myapp.who", "other").unwrap();

    // Each session reads its own value.
    match other
        .execute(&mut db, "SELECT current_setting('myapp.who')", &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows[0][0], Value::Text("other".to_string()))
        }
        _ => panic!("expected a query result"),
    }
    assert_eq!(db.var("myapp.who"), Some("default".to_string()));
    assert_eq!(other.var("myapp.who"), Some("other".to_string()));

    // A variable only on the additional session is not visible to the default at all.
    other.set_var("myapp.only", "x").unwrap();
    assert_eq!(db.var("myapp.only"), None);
}

#[test]
fn reset_vars_clears_every_variable() {
    // reset_vars is PG RESET ALL for the variable map.
    let mut db = Database::new();
    db.set_var("myapp.a", "1").unwrap();
    db.set_var("myapp.b", "2").unwrap();
    db.reset_vars();
    assert_eq!(db.var("myapp.a"), None);
    assert_eq!(db.var("myapp.b"), None);
    assert_eq!(
        err_code(&mut db, "SELECT current_setting('myapp.a')"),
        "42704"
    );
}
