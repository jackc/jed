package jed

// Bound parameters ($N) — spec/design/api.md §5. Typing, binding, and the errors they raise are pinned
// once for every core by the shared corpus (suites/params, the `# params:` directive —
// conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a binary
// float, raw bytes); the helpers are shared by other tests in the package.

import "testing"

// queryRows runs a parameterized query and returns its rows; t.Fatal on error.
func queryRows(t *testing.T, db dbHandle, sql string, params ...Value) [][]Value {
	t.Helper()
	out, err := queryOutcome(db, sql, params)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != outcomeQuery {
		t.Fatalf("%q: expected a query result", sql)
	}
	return out.Rows
}

// paramErrCode runs a parameterized statement expected to fail and returns its SQLSTATE.
func paramErrCode(t *testing.T, db dbHandle, sql string, params ...Value) string {
	t.Helper()
	_, err := queryOutcome(db, sql, params)
	if err == nil {
		t.Fatalf("%q: expected an error", sql)
	}
	var ee *EngineError
	if !asEngineError(err, &ee) {
		t.Fatalf("%q: not an EngineError: %v", sql, err)
	}
	return ee.Code()
}

func asEngineError(err error, target **EngineError) bool {
	if ee, ok := err.(*EngineError); ok {
		*target = ee
		return true
	}
	return false
}

func firstInts(rows [][]Value) []int64 {
	out := make([]int64, len(rows))
	for i, r := range rows {
		out[i] = r[0].Int
	}
	return out
}

func TestCompositePKFloatParamWidensSoundly(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (f f64, a i32, v i32, PRIMARY KEY (f, a))",
		"INSERT INTO t VALUES (1.5, 1, 10), (2.5, 1, 20)")
	rows := queryRows(t, db, "SELECT v FROM t WHERE f = $1 AND a = $2", Float64Value(1.5), IntValue(1))
	if !eqInts(firstInts(rows), 10) {
		t.Fatalf("got %v want [10]", firstInts(rows))
	}
}

// A bytea host value binds to a bytea slot and is the wrong family for a text one.
func TestByteaHostValueBindsToAdoptedSlot(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	rows := queryRows(t, db, "SELECT encode($1, 'hex')", ByteaValue([]byte{0xab}))
	if got := rows[0][0].Render(); got != "ab" {
		t.Fatalf("got %s want ab", got)
	}
	if code := paramErrCode(t, db, "SELECT length($1)", ByteaValue([]byte{1, 2})); code != "42804" {
		t.Fatalf("length(bytea value): code %s, want 42804", code)
	}
}
