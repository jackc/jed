package jed

// S3 session privileges — the host-API surface (spec/design/session.md §5.3). The SQL-observable
// 42501 behavior (every table/function/DDL gate) is corpus-tested across all three cores
// (suites/session/privileges.test); these per-core tests cover what the single-statement corpus
// cannot CALL: configuring the envelope through the Go host API directly, the value-level
// Privilege/PrivilegeSet surface, the per-session independence of an additional session, and the
// introspection accessors (CLAUDE.md §10). Mirrors impl/rust/tests/privileges.rs.

import "testing"

func privCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	return sessCode(t, err)
}

func TestDefaultSessionIsFullyPermissive(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if !db.AllowDDL() {
		t.Fatal("default session should allow DDL")
	}
	if !db.Privileges().IsPermissive() {
		t.Fatal("default session should be fully permissive")
	}
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	sessExec(t, db, "UPDATE t SET v = 20 WHERE id = 1")
	sessExec(t, db, "DELETE FROM t WHERE id = 1")
}

func TestSetDefaultPrivilegesMakesAReadOnlySession(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	db.SetDefaultPrivileges(PrivSetEmpty.With(PrivSelect))
	sessExec(t, db, "SELECT v FROM t WHERE id = 1")
	if got := privCode(t, db, "INSERT INTO t VALUES (2, 20)"); got != "42501" {
		t.Fatalf("insert: want 42501, got %s", got)
	}
	if got := privCode(t, db, "UPDATE t SET v = 0 WHERE id = 1"); got != "42501" {
		t.Fatalf("update: want 42501, got %s", got)
	}
	if got := privCode(t, db, "DELETE FROM t WHERE id = 1"); got != "42501" {
		t.Fatalf("delete: want 42501, got %s", got)
	}
}

// TestQueryPathEnforcesSelectPrivilege locks the §13 safety fix that made Query a total-AND-safe seam
// (gateReadLanes). A SELECT served by the lazy streaming lane (here a PK point lookup) used to bypass
// the privilege envelope entirely — it never reached the materialized dispatch where checkPrivileges
// lives — so a restricted session could read a table it held no SELECT on through the ergonomic Query
// path. The corpus drives this via the harness's Query re-plumb; this pins it at the Go host surface.
func TestQueryPathEnforcesSelectPrivilege(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")
	db.SetDefaultPrivileges(PrivSetEmpty) // no SELECT
	rows, err := db.queryValues("SELECT v FROM t WHERE id = 1", nil)
	if err == nil {
		_ = rows.Close()
		t.Fatal("SELECT via the streaming Query path without SELECT privilege should be 42501, got rows")
	}
	if got := sessCode(t, err); got != "42501" {
		t.Fatalf("want 42501, got %s", got)
	}
}

func TestGrantAddsAndRevokeWins(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")

	db.SetDefaultPrivileges(PrivSetEmpty)
	db.Grant(PrivSetEmpty.With(PrivInsert), "t")
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)") // bare INSERT needs only INSERT

	// Revoking what was granted denies it (deny wins regardless of the grant).
	db.Revoke(PrivSetEmpty.With(PrivInsert), "t")
	if got := privCode(t, db, "INSERT INTO t VALUES (2, 20)"); got != "42501" {
		t.Fatalf("want 42501, got %s", got)
	}
	if db.Privileges().AllowsTable("t", PrivInsert) {
		t.Fatal("revoke should win over grant")
	}
}

func TestAllowDDLGateIsIndependentOfTablePrivileges(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	db.SetAllowDDL(false)
	if got := privCode(t, db, "CREATE TABLE u (id i32 PRIMARY KEY)"); got != "42501" {
		t.Fatalf("create: want 42501, got %s", got)
	}
	if got := privCode(t, db, "DROP TABLE t"); got != "42501" {
		t.Fatalf("drop: want 42501, got %s", got)
	}
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)") // DML untouched
}

func TestFunctionExecuteIsRevocable(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if !db.Privileges().AllowsFunction("abs") {
		t.Fatal("functions should default to EXECUTE on all")
	}
	sessExec(t, db, "SELECT abs(-5)")
	db.Revoke(PrivSetEmpty.With(PrivExecute), "abs")
	if db.Privileges().AllowsFunction("abs") {
		t.Fatal("EXECUTE on abs should be revoked")
	}
	if got := privCode(t, db, "SELECT abs(-5)"); got != "42501" {
		t.Fatalf("want 42501, got %s", got)
	}
	sessExec(t, db, "SELECT 1 + 2") // the + operator is not a named function — never gated
}

func TestAnAdditionalSessionCarriesItsOwnEnvelope(t *testing.T) {
	t.Parallel()
	// db.Session(opts) mints an independent session over a shared Database core (§2.4): a restricted
	// one rejects a write a permissive session still allows, and they share committed storage through
	// the core (§2.1/§5.3) — each owns its envelope, no swap.
	db := memDB()
	a := db.Session(SessionOptions{})
	if _, err := queryOutcome(a, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", nil); err != nil {
		t.Fatal(err)
	}

	readOnly := PrivSetEmpty.With(PrivSelect)
	restricted := db.Session(SessionOptions{DefaultPrivileges: &readOnly})
	if _, err := queryOutcome(restricted, "SELECT * FROM t", nil); err != nil {
		t.Fatalf("read should be allowed on the restricted session: %v", err)
	}
	if _, err := queryOutcome(restricted, "INSERT INTO t VALUES (1, 10)", nil); sessCode(t, err) != "42501" {
		t.Fatalf("write should be 42501 on the restricted session")
	}

	// The permissive session is unaffected — it still writes.
	if _, err := queryOutcome(a, "INSERT INTO t VALUES (1, 10)", nil); err != nil {
		t.Fatal(err)
	}

	// A grant on the additional session lifts the restriction for it alone.
	restricted.Grant(PrivSetEmpty.With(PrivInsert), "t")
	if _, err := queryOutcome(restricted, "INSERT INTO t VALUES (2, 20)", nil); err != nil {
		t.Fatalf("insert should be allowed after grant on the restricted session: %v", err)
	}
}

func TestMissingObjectIs42P01NotAuthorization(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	db.SetDefaultPrivileges(PrivSetEmpty)
	if got := privCode(t, db, "SELECT * FROM does_not_exist"); got != "42P01" {
		t.Fatalf("want 42P01 (existence before authorization), got %s", got)
	}
}
