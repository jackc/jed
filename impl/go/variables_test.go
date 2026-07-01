package jed

// S5 session variables — the host-API surface (spec/design/session.md §6.1). The SQL-observable
// current_setting behavior (a set variable read back, the 42704-on-unset, missing_ok → NULL, the
// per-record reset) is corpus-tested across all three cores (suites/session/variables.test); these
// per-core tests cover what the directive-driven corpus cannot CALL or OBSERVE: the host setters and
// getter (SetVar/ResetVar/Var), the 42704 rejection of a non-dotted name, case folding at the host
// API, NULL propagation through a text-typed NULL value, that variables are SESSION state not snapshot
// state (they do not roll back with a transaction), an additional session's independent variables, and
// ResetVars (PG RESET ALL). Mirrors impl/rust/tests/variables.rs.

import "testing"

// varScalar runs a single-row, single-column query and returns the lone value.
func varScalar(t *testing.T, db dbHandle, sql string) Value {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if len(out.Rows) != 1 || len(out.Rows[0]) != 1 {
		t.Fatalf("%q: want one row/column, got %v", sql, out.Rows)
	}
	return out.Rows[0][0]
}

func varErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := db.Execute(sql, nil)
	if err == nil {
		t.Fatalf("%q: expected an error", sql)
	}
	return err.(*EngineError).Code()
}

func mustText(t *testing.T, v Value, want string) {
	t.Helper()
	if v.Kind != ValText || v.Str != want {
		t.Fatalf("want text %q, got %v", want, v)
	}
}

func TestVarHostSetAndReadRoundTrip(t *testing.T) {
	// SetVar stores; Var reads it back through the host API; current_setting reads it in SQL.
	db := NewDatabase().Session(SessionOptions{})
	if _, ok := db.Var("myapp.tenant"); ok {
		t.Fatal("fresh var should be unset")
	}
	if err := db.SetVar("myapp.tenant", "acme"); err != nil {
		t.Fatalf("SetVar: %v", err)
	}
	if v, ok := db.Var("myapp.tenant"); !ok || v != "acme" {
		t.Fatalf("Var: want acme/true, got %q/%v", v, ok)
	}
	mustText(t, varScalar(t, db, "SELECT current_setting('myapp.tenant')"), "acme")
}

func TestVarSetAndResetRejectANonDottedName(t *testing.T) {
	// A variable must be namespaced (dotted) — a non-dotted name is a built-in setting name, and v1
	// exposes none through this map (the time_zone built-in is its own slice), so it is 42704.
	db := NewDatabase().Session(SessionOptions{})
	if err := db.SetVar("bogus", "x"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("SetVar non-dotted: want 42704, got %v", err)
	}
	if err := db.ResetVar("bogus"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("ResetVar non-dotted: want 42704, got %v", err)
	}
	// The host getter never errors — a non-dotted (or any unset) name simply reads as unset.
	if _, ok := db.Var("bogus"); ok {
		t.Fatal("non-dotted name should read as unset")
	}
}

func TestVarResetRemovesAndIsIdempotent(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	if err := db.SetVar("myapp.k", "v"); err != nil {
		t.Fatal(err)
	}
	if err := db.ResetVar("myapp.k"); err != nil {
		t.Fatal(err)
	}
	if _, ok := db.Var("myapp.k"); ok {
		t.Fatal("var should be gone after reset")
	}
	if got := varErrCode(t, db, "SELECT current_setting('myapp.k')"); got != "42704" {
		t.Fatalf("current_setting after reset: want 42704, got %s", got)
	}
	// Resetting an unset variable is a no-op success (PG RESET of an unset custom variable).
	if err := db.ResetVar("myapp.k"); err != nil {
		t.Fatalf("idempotent reset: %v", err)
	}
}

func TestVarNamesAreCaseInsensitiveButValuesAreVerbatim(t *testing.T) {
	// The NAME folds to lowercase (PG GUC names are case-insensitive); the VALUE is preserved exactly.
	db := NewDatabase().Session(SessionOptions{})
	if err := db.SetVar("myApp.Tenant", "AcmeCorp"); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"myapp.tenant", "MYAPP.TENANT"} {
		if v, ok := db.Var(name); !ok || v != "AcmeCorp" {
			t.Fatalf("Var(%q): want AcmeCorp, got %q/%v", name, v, ok)
		}
	}
	mustText(t, varScalar(t, db, "SELECT current_setting('MyApp.TENANT')"), "AcmeCorp")
}

func TestVarMissingOkTurnsTheUnsetErrorIntoNull(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	if got := varErrCode(t, db, "SELECT current_setting('myapp.unset')"); got != "42704" {
		t.Fatalf("one-arg unset: want 42704, got %s", got)
	}
	if v := varScalar(t, db, "SELECT current_setting('myapp.unset', true)"); v.Kind != ValNull {
		t.Fatalf("missing_ok=true: want NULL, got %v", v)
	}
	// false behaves like the one-arg form.
	if got := varErrCode(t, db, "SELECT current_setting('myapp.unset', false)"); got != "42704" {
		t.Fatalf("missing_ok=false: want 42704, got %s", got)
	}
}

func TestVarNullNamePropagatesToNull(t *testing.T) {
	// null = "propagates": a NULL name short-circuits to NULL before the lookup. A text column holding
	// a NULL is the typed-NULL the corpus cannot write (jed defers text casts, so no NULL::text yet).
	db := NewDatabase().Session(SessionOptions{})
	sessExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, n text)")
	sessExec(t, db, "INSERT INTO t VALUES (1, NULL)")
	if err := db.SetVar("myapp.x", "set"); err != nil {
		t.Fatal(err)
	}
	if v := varScalar(t, db, "SELECT current_setting(n) FROM t WHERE id = 1"); v.Kind != ValNull {
		t.Fatalf("NULL name: want NULL, got %v", v)
	}
}

func TestVarsAreSessionStateNotSnapshotState(t *testing.T) {
	// Variables are SESSION state, not snapshot state (§6.1): a ROLLBACK undoes DATA but never a
	// session variable (PG SET SESSION). Set one outside, one inside a block, roll back — both survive.
	db := NewDatabase().Session(SessionOptions{})
	if err := db.SetVar("myapp.outer", "a"); err != nil {
		t.Fatal(err)
	}
	sessExec(t, db, "BEGIN")
	if err := db.SetVar("myapp.inner", "b"); err != nil {
		t.Fatal(err)
	}
	sessExec(t, db, "ROLLBACK")
	if v, ok := db.Var("myapp.outer"); !ok || v != "a" {
		t.Fatalf("outer after rollback: want a, got %q/%v", v, ok)
	}
	if v, ok := db.Var("myapp.inner"); !ok || v != "b" {
		t.Fatalf("inner after rollback: want b, got %q/%v", v, ok)
	}
	mustText(t, varScalar(t, db, "SELECT current_setting('myapp.inner')"), "b")
}

func TestVarAdditionalSessionHasIndependentVariables(t *testing.T) {
	// db.Session(opts) mints an independent session over a shared core (§2.1/§2.4): its variable map
	// is its own — a variable set on it is invisible to another session and vice versa.
	db := NewDatabase()
	a := db.Session(SessionOptions{})
	if err := a.SetVar("myapp.who", "a"); err != nil {
		t.Fatal(err)
	}
	other := db.Session(SessionOptions{})
	if err := other.SetVar("myapp.who", "other"); err != nil {
		t.Fatal(err)
	}
	out, err := other.Execute("SELECT current_setting('myapp.who')", nil)
	if err != nil {
		t.Fatalf("other execute: %v", err)
	}
	mustText(t, out.Rows[0][0], "other")
	if v, _ := a.Var("myapp.who"); v != "a" {
		t.Fatalf("session a: want a, got %q", v)
	}
	if v, _ := other.Var("myapp.who"); v != "other" {
		t.Fatalf("other session: want other, got %q", v)
	}
	// A variable only on one session is not visible to the other at all.
	if err := other.SetVar("myapp.only", "x"); err != nil {
		t.Fatal(err)
	}
	if _, ok := a.Var("myapp.only"); ok {
		t.Fatal("additional session's var leaked to the other")
	}
}

func TestVarResetVarsClearsEveryVariable(t *testing.T) {
	// ResetVars is PG RESET ALL for the variable map.
	db := NewDatabase().Session(SessionOptions{})
	if err := db.SetVar("myapp.a", "1"); err != nil {
		t.Fatal(err)
	}
	if err := db.SetVar("myapp.b", "2"); err != nil {
		t.Fatal(err)
	}
	db.ResetVars()
	if _, ok := db.Var("myapp.a"); ok {
		t.Fatal("myapp.a should be cleared")
	}
	if _, ok := db.Var("myapp.b"); ok {
		t.Fatal("myapp.b should be cleared")
	}
	if got := varErrCode(t, db, "SELECT current_setting('myapp.a')"); got != "42704" {
		t.Fatalf("after RESET ALL: want 42704, got %s", got)
	}
}
