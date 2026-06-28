//! S4 session lifetime cost budget — the host-API surface (spec/design/session.md §5.4). The
//! SQL-observable `54P02` schedule (in-flight abort + admission rejection) is corpus-tested across
//! all three cores (`suites/session/lifetime_cost.test`); these per-core tests cover what the
//! single-session corpus cannot *call* or *observe*: the cumulative-cost gauge (`lifetime_cost()`),
//! the budget setters, that the cumulative is **session state, not snapshot state** (it does not roll
//! back with a transaction), the exact partial cost an aborted statement leaves, the precise
//! `54P01`-vs-`54P02` precedence (and its exact tie), and an additional session's independent budget
//! (CLAUDE.md §10).

use jed::{Database, Engine, PrivilegeSet, SessionOptions};

fn code(db: &mut Engine, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("expected an error from: {sql}"))
        .code()
        .to_string()
}

/// `SELECT 1 + 1 + 1 + 1 + 1` — five `1`s, four `+` — costs 5 (4 operator_eval + 1 row_produced).
const COST5: &str = "SELECT 1 + 1 + 1 + 1 + 1";

#[test]
fn default_session_has_no_budget_but_tracks_the_cumulative() {
    // A fresh session is unlimited (budget 0) yet still TRACKS the cumulative cost — the gauge is
    // always readable (§5.4), it just never aborts.
    let mut db = Engine::new();
    assert_eq!(db.lifetime_max_cost(), 0);
    assert_eq!(db.lifetime_cost(), 0);
    db.execute("SELECT 1", &[]).unwrap(); // cost 1
    assert_eq!(db.lifetime_cost(), 1);
    db.execute(COST5, &[]).unwrap(); // cost 5
    assert_eq!(db.lifetime_cost(), 6);
}

#[test]
fn budget_aborts_in_flight_then_rejects_at_admission() {
    // Set a budget of 3. The cumulative builds across statements; the one that drives it to the budget
    // aborts 54P02 mid-flight, and every further statement is then rejected 54P02 at admission.
    let mut db = Engine::new();
    db.set_lifetime_max_cost(3);
    assert_eq!(db.lifetime_max_cost(), 3);

    db.execute("SELECT 1", &[]).unwrap(); // cumulative 1
    db.execute("SELECT 1", &[]).unwrap(); // cumulative 2
    // The third SELECT 1 drives the cumulative to 3 and aborts 54P02 (the instant it reaches the
    // budget). Its partial cost counts, so the cumulative is now exactly the budget.
    assert_eq!(code(&mut db, "SELECT 1"), "54P02");
    assert_eq!(db.lifetime_cost(), 3);
    // Spent: every further statement is rejected at admission — even a trivial one, even a write.
    assert_eq!(code(&mut db, "SELECT 1"), "54P02");
    assert_eq!(
        code(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)"),
        "54P02"
    );
}

#[test]
fn partial_cost_of_an_aborted_statement_counts() {
    // A single statement larger than the whole budget aborts mid-flight, and the partial work it did
    // (up to the budget) still counts — the cumulative lands exactly at the budget (unit charges are
    // 1, so the guard fires precisely as it reaches it).
    let mut db = Engine::new();
    db.set_lifetime_max_cost(3);
    assert_eq!(code(&mut db, COST5), "54P02"); // would cost 5; aborts at 3
    assert_eq!(db.lifetime_cost(), 3);
}

#[test]
fn the_cumulative_is_session_state_and_does_not_roll_back() {
    // The cumulative is SESSION state, not snapshot state (§5.4): a ROLLBACK undoes a statement's
    // DATA effects but NOT the compute it spent. Run work inside an explicit block, roll it back, and
    // the cumulative still reflects every statement's cost.
    let mut db = Engine::new();
    db.execute("BEGIN", &[]).unwrap();
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)", &[]).unwrap();
    db.execute(COST5, &[]).unwrap(); // cost 5
    let before_rollback = db.lifetime_cost();
    assert!(
        before_rollback >= 5,
        "cumulative should include the block's cost"
    );
    db.execute("ROLLBACK", &[]).unwrap();
    // The table is gone (data rolled back) but the cumulative is unchanged (compute was spent).
    assert_eq!(db.lifetime_cost(), before_rollback);
    assert_eq!(code(&mut db, "SELECT v FROM t"), "42P01"); // table really did roll back
    // And the cumulative keeps building from there.
    db.execute("SELECT 1", &[]).unwrap();
    assert_eq!(db.lifetime_cost(), before_rollback + 1);
}

#[test]
fn a_statement_aborts_at_whichever_ceiling_it_reaches_first() {
    // max_cost (54P01) and lifetime_max_cost (54P02) compose: a statement aborts at whichever it
    // reaches first. With the per-statement ceiling tight and the session budget far, the per-statement
    // ceiling wins (54P01) — and its partial cost still counts toward the session budget.
    let mut db = Engine::new();
    db.set_lifetime_max_cost(1000);
    db.set_max_cost(3);
    assert_eq!(code(&mut db, COST5), "54P01"); // max_cost 3 reached before the far budget
    assert_eq!(db.lifetime_cost(), 3); // the 54P01 partial counted toward the session

    // Now the session budget is the nearer ceiling: a tight budget, the per-statement ceiling far.
    let mut db = Engine::new();
    db.set_lifetime_max_cost(3);
    db.set_max_cost(1000);
    assert_eq!(code(&mut db, COST5), "54P02"); // the budget is reached first
}

#[test]
fn an_exact_tie_breaks_to_the_per_statement_ceiling() {
    // When both ceilings are reached at the very same accrued value, the inner per-statement ceiling
    // wins the tie (54P01) — the documented, deterministic, cross-core tie rule (§5.4, cost.rs guard).
    let mut db = Engine::new();
    db.set_lifetime_max_cost(3);
    db.set_max_cost(3);
    assert_eq!(code(&mut db, COST5), "54P01");
}

#[test]
fn an_additional_session_carries_its_own_budget() {
    // db.session(opts) mints an independent session with its own cumulative + budget (§2.1/§2.4/§5.4):
    // a budgeted additional session aborts at its budget while a permissive one keeps running, and
    // the two cumulatives are independent (each session owns its envelope).
    let db = Database::new_in_memory();
    let mut a = db.session(SessionOptions::default());
    a.execute("SELECT 1", &[]).unwrap(); // a's cumulative 1

    let mut budgeted = db.session(SessionOptions {
        lifetime_max_cost: 2,
        ..SessionOptions::default()
    });
    budgeted.execute("SELECT 1", &[]).unwrap(); // its cumulative 1
    // Its second statement drives its own budget to 2 and aborts 54P02 — independent of `a`.
    let err = budgeted.execute("SELECT 1", &[]).err().unwrap();
    assert_eq!(err.code(), "54P02");
    assert_eq!(budgeted.lifetime_cost(), 2);

    // `a` is untouched by the additional session's budget — it still runs, and its cumulative
    // reflects only its own statements.
    assert_eq!(a.lifetime_cost(), 1);
    a.execute("SELECT 1", &[]).unwrap();
    assert_eq!(a.lifetime_cost(), 2);
}

#[test]
fn admission_is_checked_before_existence_and_privileges() {
    // The budget admission check runs ahead of privileges AND existence (§5.4): once a session is
    // exhausted, even a query naming a missing table is 54P02, not 42P01 — nothing runs.
    let mut db = Engine::new();
    db.set_lifetime_max_cost(1);
    // SELECT 1 costs 1, reaching the budget — it aborts 54P02 (and spends the budget).
    assert_eq!(code(&mut db, "SELECT 1"), "54P02");
    // Now exhausted: a missing table is rejected at admission (54P02) before the 42P01 existence
    // check, and likewise a restricted privilege envelope is never consulted.
    db.set_default_privileges(PrivilegeSet::EMPTY.with(jed::Privilege::Select));
    assert_eq!(code(&mut db, "SELECT * FROM does_not_exist"), "54P02");
}
