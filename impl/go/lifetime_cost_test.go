package jed

// S4 session lifetime cost budget — the host-API surface (spec/design/session.md §5.4). The
// SQL-observable 54P02 schedule (in-flight abort + admission rejection) is corpus-tested across all
// three cores (suites/session/lifetime_cost.test); these per-core tests cover what the single-session
// corpus cannot CALL or OBSERVE: the cumulative-cost gauge (LifetimeCost), the budget setters, that
// the cumulative is SESSION state not snapshot state (it does not roll back with a transaction), the
// exact partial cost an aborted statement leaves, the precise 54P01-vs-54P02 precedence (and its
// exact tie), and an additional session's independent budget. Mirrors impl/rust/tests/lifetime_cost.rs.

import "testing"

// cost5 — "SELECT 1 + 1 + 1 + 1 + 1" — five 1s, four +, costs 5 (4 operator_eval + 1 row_produced).
const cost5 = "SELECT 1 + 1 + 1 + 1 + 1"

func lifeCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	return sessCode(t, err)
}

func TestDefaultSessionHasNoBudgetButTracksTheCumulative(t *testing.T) {
	// A fresh session is unlimited (budget 0) yet still TRACKS the cumulative cost — the gauge is
	// always readable (§5.4), it just never aborts.
	db := NewDatabase()
	if db.LifetimeMaxCost() != 0 {
		t.Fatalf("fresh budget: want 0, got %d", db.LifetimeMaxCost())
	}
	if db.LifetimeCost() != 0 {
		t.Fatalf("fresh cumulative: want 0, got %d", db.LifetimeCost())
	}
	sessExec(t, db, "SELECT 1") // cost 1
	if db.LifetimeCost() != 1 {
		t.Fatalf("after SELECT 1: want 1, got %d", db.LifetimeCost())
	}
	sessExec(t, db, cost5) // cost 5
	if db.LifetimeCost() != 6 {
		t.Fatalf("after cost-5: want 6, got %d", db.LifetimeCost())
	}
}

func TestBudgetAbortsInFlightThenRejectsAtAdmission(t *testing.T) {
	// Set a budget of 3. The cumulative builds across statements; the one that drives it to the budget
	// aborts 54P02 mid-flight, and every further statement is then rejected 54P02 at admission.
	db := NewDatabase()
	db.SetLifetimeMaxCost(3)
	if db.LifetimeMaxCost() != 3 {
		t.Fatalf("budget: want 3, got %d", db.LifetimeMaxCost())
	}
	sessExec(t, db, "SELECT 1") // cumulative 1
	sessExec(t, db, "SELECT 1") // cumulative 2
	// The third SELECT 1 drives the cumulative to 3 and aborts 54P02; its partial cost counts, so the
	// cumulative is now exactly the budget.
	if got := lifeCode(t, db, "SELECT 1"); got != "54P02" {
		t.Fatalf("crossing: want 54P02, got %s", got)
	}
	if db.LifetimeCost() != 3 {
		t.Fatalf("after abort: want 3, got %d", db.LifetimeCost())
	}
	// Spent: every further statement is rejected at admission — even a trivial one, even a write.
	if got := lifeCode(t, db, "SELECT 1"); got != "54P02" {
		t.Fatalf("admission: want 54P02, got %s", got)
	}
	if got := lifeCode(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)"); got != "54P02" {
		t.Fatalf("admission DDL: want 54P02, got %s", got)
	}
}

func TestPartialCostOfAnAbortedStatementCounts(t *testing.T) {
	// A single statement larger than the whole budget aborts mid-flight, and the partial work it did
	// (up to the budget) still counts — the cumulative lands exactly at the budget (unit charges are 1).
	db := NewDatabase()
	db.SetLifetimeMaxCost(3)
	if got := lifeCode(t, db, cost5); got != "54P02" { // would cost 5; aborts at 3
		t.Fatalf("want 54P02, got %s", got)
	}
	if db.LifetimeCost() != 3 {
		t.Fatalf("partial: want 3, got %d", db.LifetimeCost())
	}
}

func TestTheCumulativeIsSessionStateAndDoesNotRollBack(t *testing.T) {
	// The cumulative is SESSION state, not snapshot state (§5.4): a ROLLBACK undoes a statement's DATA
	// effects but NOT the compute it spent. Run work inside an explicit block, roll it back, and the
	// cumulative still reflects every statement's cost.
	db := NewDatabase()
	sessExec(t, db, "BEGIN")
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	sessExec(t, db, cost5) // cost 5
	beforeRollback := db.LifetimeCost()
	if beforeRollback < 5 {
		t.Fatalf("cumulative should include the block's cost, got %d", beforeRollback)
	}
	sessExec(t, db, "ROLLBACK")
	// The table is gone (data rolled back) but the cumulative is unchanged (compute was spent).
	if db.LifetimeCost() != beforeRollback {
		t.Fatalf("after rollback: want %d, got %d", beforeRollback, db.LifetimeCost())
	}
	if got := lifeCode(t, db, "SELECT v FROM t"); got != "42P01" {
		t.Fatalf("table should have rolled back, got %s", got)
	}
	// And the cumulative keeps building from there.
	sessExec(t, db, "SELECT 1")
	if db.LifetimeCost() != beforeRollback+1 {
		t.Fatalf("after SELECT 1: want %d, got %d", beforeRollback+1, db.LifetimeCost())
	}
}

func TestAStatementAbortsAtWhicheverCeilingItReachesFirst(t *testing.T) {
	// max_cost (54P01) and lifetime_max_cost (54P02) compose: a statement aborts at whichever it
	// reaches first. With the per-statement ceiling tight and the budget far, the per-statement ceiling
	// wins (54P01) — and its partial cost still counts toward the session budget.
	db := NewDatabase()
	db.SetLifetimeMaxCost(1000)
	db.SetMaxCost(3)
	if got := lifeCode(t, db, cost5); got != "54P01" { // max_cost 3 before the far budget
		t.Fatalf("want 54P01, got %s", got)
	}
	if db.LifetimeCost() != 3 {
		t.Fatalf("54P01 partial counted: want 3, got %d", db.LifetimeCost())
	}

	// Now the session budget is the nearer ceiling: a tight budget, the per-statement ceiling far.
	db2 := NewDatabase()
	db2.SetLifetimeMaxCost(3)
	db2.SetMaxCost(1000)
	if got := lifeCode(t, db2, cost5); got != "54P02" { // the budget is reached first
		t.Fatalf("want 54P02, got %s", got)
	}
}

func TestAnExactTieBreaksToThePerStatementCeiling(t *testing.T) {
	// When both ceilings are reached at the very same accrued value, the inner per-statement ceiling
	// wins the tie (54P01) — the documented, deterministic, cross-core tie rule (§5.4, cost.go Guard).
	db := NewDatabase()
	db.SetLifetimeMaxCost(3)
	db.SetMaxCost(3)
	if got := lifeCode(t, db, cost5); got != "54P01" {
		t.Fatalf("tie: want 54P01, got %s", got)
	}
}

func TestAnAdditionalSessionCarriesItsOwnBudget(t *testing.T) {
	// db.NewSession(opts) mints an independent session with its own cumulative + budget (§2.1/§5.4): a
	// restricted additional session aborts at its budget while the permissive default keeps running,
	// and the two cumulatives are independent.
	db := NewDatabase()
	sessExec(t, db, "SELECT 1") // default cumulative 1

	budgeted := db.NewSession(SessionOptions{LifetimeMaxCost: 2})
	if _, err := budgeted.ExecuteSQL(db, "SELECT 1", nil); err != nil { // its cumulative 1
		t.Fatalf("first: %v", err)
	}
	// Its second statement drives its own budget to 2 and aborts 54P02 — independent of the default.
	if _, err := budgeted.ExecuteSQL(db, "SELECT 1", nil); err == nil || err.(*EngineError).Code() != "54P02" {
		t.Fatalf("second: want 54P02, got %v", err)
	}
	if budgeted.LifetimeCost() != 2 {
		t.Fatalf("additional cumulative: want 2, got %d", budgeted.LifetimeCost())
	}

	// The default session is untouched by the additional session's budget — it still runs, and its
	// cumulative reflects only its own statements.
	if db.LifetimeCost() != 1 {
		t.Fatalf("default cumulative: want 1, got %d", db.LifetimeCost())
	}
	sessExec(t, db, "SELECT 1")
	if db.LifetimeCost() != 2 {
		t.Fatalf("default after SELECT 1: want 2, got %d", db.LifetimeCost())
	}
}

func TestAdmissionIsCheckedBeforeExistenceAndPrivileges(t *testing.T) {
	// The budget admission check runs ahead of privileges AND existence (§5.4): once a session is
	// exhausted, even a query naming a missing table is 54P02, not 42P01 — nothing runs.
	db := NewDatabase()
	db.SetLifetimeMaxCost(1)
	// SELECT 1 costs 1, reaching the budget — it aborts 54P02 (and spends the budget).
	if got := lifeCode(t, db, "SELECT 1"); got != "54P02" {
		t.Fatalf("spend: want 54P02, got %s", got)
	}
	// Now exhausted: a missing table is rejected at admission (54P02) before the 42P01 existence check,
	// and likewise a restricted privilege envelope is never consulted.
	db.SetDefaultPrivileges(PrivSetEmpty.With(PrivSelect))
	if got := lifeCode(t, db, "SELECT * FROM does_not_exist"); got != "54P02" {
		t.Fatalf("admission before existence: want 54P02, got %s", got)
	}
}
