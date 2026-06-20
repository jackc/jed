// S4 session lifetime cost budget — the host-API surface (spec/design/session.md §5.4). The
// SQL-observable 54P02 schedule (in-flight abort + admission rejection) is corpus-tested across all
// three cores (suites/session/lifetime_cost.test); these per-core tests cover what the single-session
// corpus cannot CALL or OBSERVE: the cumulative-cost gauge (lifetimeCost), the budget setters, that
// the cumulative is SESSION state not snapshot state (it does not roll back with a transaction), the
// exact partial cost an aborted statement leaves, the precise 54P01-vs-54P02 precedence (and its
// exact tie), and an additional session's independent budget. Mirrors impl/rust/tests/lifetime_cost.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute, PrivilegeSet } from "../src/lib.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

// COST5 — "SELECT 1 + 1 + 1 + 1 + 1" — five 1s, four +, costs 5 (4 operator_eval + 1 row_produced).
const COST5 = "SELECT 1 + 1 + 1 + 1 + 1";

test("default session has no budget but tracks the cumulative", () => {
  // A fresh session is unlimited (budget 0) yet still TRACKS the cumulative cost — the gauge is
  // always readable (§5.4), it just never aborts.
  const db = new Database();
  assert.equal(db.lifetimeMaxCost(), 0n);
  assert.equal(db.lifetimeCost(), 0n);
  execute(db, "SELECT 1"); // cost 1
  assert.equal(db.lifetimeCost(), 1n);
  execute(db, COST5); // cost 5
  assert.equal(db.lifetimeCost(), 6n);
});

test("budget aborts in flight then rejects at admission", () => {
  // Set a budget of 3. The cumulative builds across statements; the one that drives it to the budget
  // aborts 54P02 mid-flight, and every further statement is then rejected 54P02 at admission.
  const db = new Database();
  db.setLifetimeMaxCost(3n);
  assert.equal(db.lifetimeMaxCost(), 3n);
  execute(db, "SELECT 1"); // cumulative 1
  execute(db, "SELECT 1"); // cumulative 2
  // The third SELECT 1 drives the cumulative to 3 and aborts 54P02; its partial cost counts, so the
  // cumulative is now exactly the budget.
  assert.equal(code(() => execute(db, "SELECT 1")), "54P02");
  assert.equal(db.lifetimeCost(), 3n);
  // Spent: every further statement is rejected at admission — even a trivial one, even a write.
  assert.equal(code(() => execute(db, "SELECT 1")), "54P02");
  assert.equal(code(() => execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)")), "54P02");
});

test("partial cost of an aborted statement counts", () => {
  // A single statement larger than the whole budget aborts mid-flight, and the partial work it did
  // (up to the budget) still counts — the cumulative lands exactly at the budget (unit charges are 1).
  const db = new Database();
  db.setLifetimeMaxCost(3n);
  assert.equal(code(() => execute(db, COST5)), "54P02"); // would cost 5; aborts at 3
  assert.equal(db.lifetimeCost(), 3n);
});

test("the cumulative is session state and does not roll back", () => {
  // The cumulative is SESSION state, not snapshot state (§5.4): a ROLLBACK undoes a statement's DATA
  // effects but NOT the compute it spent. Run work inside an explicit block, roll it back, and the
  // cumulative still reflects every statement's cost.
  const db = new Database();
  execute(db, "BEGIN");
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  execute(db, COST5); // cost 5
  const beforeRollback = db.lifetimeCost();
  assert.ok(beforeRollback >= 5n, "cumulative should include the block's cost");
  execute(db, "ROLLBACK");
  // The table is gone (data rolled back) but the cumulative is unchanged (compute was spent).
  assert.equal(db.lifetimeCost(), beforeRollback);
  assert.equal(code(() => execute(db, "SELECT v FROM t")), "42P01"); // table really did roll back
  // And the cumulative keeps building from there.
  execute(db, "SELECT 1");
  assert.equal(db.lifetimeCost(), beforeRollback + 1n);
});

test("a statement aborts at whichever ceiling it reaches first", () => {
  // maxCost (54P01) and lifetimeMaxCost (54P02) compose: a statement aborts at whichever it reaches
  // first. With the per-statement ceiling tight and the budget far, the per-statement ceiling wins
  // (54P01) — and its partial cost still counts toward the session budget.
  const db = new Database();
  db.setLifetimeMaxCost(1000n);
  db.setMaxCost(3n);
  assert.equal(code(() => execute(db, COST5)), "54P01"); // max_cost 3 before the far budget
  assert.equal(db.lifetimeCost(), 3n); // the 54P01 partial counted toward the session

  // Now the session budget is the nearer ceiling: a tight budget, the per-statement ceiling far.
  const db2 = new Database();
  db2.setLifetimeMaxCost(3n);
  db2.setMaxCost(1000n);
  assert.equal(code(() => execute(db2, COST5)), "54P02"); // the budget is reached first
});

test("an exact tie breaks to the per-statement ceiling", () => {
  // When both ceilings are reached at the very same accrued value, the inner per-statement ceiling
  // wins the tie (54P01) — the documented, deterministic, cross-core tie rule (§5.4, cost.ts guard).
  const db = new Database();
  db.setLifetimeMaxCost(3n);
  db.setMaxCost(3n);
  assert.equal(code(() => execute(db, COST5)), "54P01");
});

test("an additional session carries its own budget", () => {
  // db.newSession(opts) mints an independent session with its own cumulative + budget (§2.1/§5.4): a
  // restricted additional session aborts at its budget while the permissive default keeps running,
  // and the two cumulatives are independent.
  const db = new Database();
  execute(db, "SELECT 1"); // default cumulative 1

  const budgeted = db.newSession({ lifetimeMaxCost: 2n });
  budgeted.execute(db, "SELECT 1"); // its cumulative 1
  // Its second statement drives its own budget to 2 and aborts 54P02 — independent of the default.
  assert.equal(code(() => budgeted.execute(db, "SELECT 1")), "54P02");
  assert.equal(budgeted.lifetimeCost(), 2n);

  // The default session is untouched by the additional session's budget — it still runs, and its
  // cumulative reflects only its own statements.
  assert.equal(db.lifetimeCost(), 1n);
  execute(db, "SELECT 1");
  assert.equal(db.lifetimeCost(), 2n);
});

test("admission is checked before existence and privileges", () => {
  // The budget admission check runs ahead of privileges AND existence (§5.4): once a session is
  // exhausted, even a query naming a missing table is 54P02, not 42P01 — nothing runs.
  const db = new Database();
  db.setLifetimeMaxCost(1n);
  // SELECT 1 costs 1, reaching the budget — it aborts 54P02 (and spends the budget).
  assert.equal(code(() => execute(db, "SELECT 1")), "54P02");
  // Now exhausted: a missing table is rejected at admission (54P02) before the 42P01 existence check,
  // and likewise a restricted privilege envelope is never consulted.
  db.setDefaultPrivileges(PrivilegeSet.empty().with("select"));
  assert.equal(code(() => execute(db, "SELECT * FROM does_not_exist")), "54P02");
});
