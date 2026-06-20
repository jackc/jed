package jed

// S3 session privileges — the host-API surface (spec/design/session.md §5.3). The SQL-observable
// 42501 behavior (every table/function/DDL gate) is corpus-tested across all three cores
// (suites/session/privileges.test); these per-core tests cover what the single-statement corpus
// cannot CALL: configuring the envelope through the Go host API directly, the value-level
// Privilege/PrivilegeSet surface, the per-session independence of an additional session, and the
// introspection accessors (CLAUDE.md §10). Mirrors impl/rust/tests/privileges.rs.

import "testing"

func privCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	return sessCode(t, err)
}

func TestDefaultSessionIsFullyPermissive(t *testing.T) {
	db := NewDatabase()
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
	db := NewDatabase()
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

func TestGrantAddsAndRevokeWins(t *testing.T) {
	db := NewDatabase()
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
	db := NewDatabase()
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
	db := NewDatabase()
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
	db := NewDatabase()
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")

	readOnly := PrivSetEmpty.With(PrivSelect)
	restricted := db.NewSession(SessionOptions{DefaultPrivileges: &readOnly})
	if _, err := restricted.ExecuteSQL(db, "SELECT * FROM t", nil); err != nil {
		t.Fatalf("read should be allowed on the restricted session: %v", err)
	}
	if _, err := restricted.ExecuteSQL(db, "INSERT INTO t VALUES (1, 10)", nil); sessCode(t, err) != "42501" {
		t.Fatalf("write should be 42501 on the restricted session")
	}

	// The default session is unaffected — it still writes.
	sessExec(t, db, "INSERT INTO t VALUES (1, 10)")

	// A grant on the additional session lifts the restriction for it alone.
	restricted.Grant(PrivSetEmpty.With(PrivInsert), "t")
	if _, err := restricted.ExecuteSQL(db, "INSERT INTO t VALUES (2, 20)", nil); err != nil {
		t.Fatalf("insert should be allowed after grant on the restricted session: %v", err)
	}
}

func TestMissingObjectIs42P01NotAuthorization(t *testing.T) {
	db := NewDatabase()
	db.SetDefaultPrivileges(PrivSetEmpty)
	if got := privCode(t, db, "SELECT * FROM does_not_exist"); got != "42P01" {
		t.Fatalf("want 42P01 (existence before authorization), got %s", got)
	}
}
