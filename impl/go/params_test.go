package jed

// Bound parameters ($N) — spec/design/api.md §5. Typing, binding, and the errors they raise are pinned
// once for every core by the shared corpus (suites/params, the `# params:` directive —
// conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a binary
// float, raw bytes, a range value) and the Go-native slice mapping; the helpers are shared by other
// tests in the package.

import (
	"context"
	"testing"
)

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

// A range host value binds canonicalized, as a range constructor's result is (api.md §5): an
// inclusive discrete upper bound moves to the canonical `[)` form, and crossed bounds are 22000.
func TestRangeHostValueBindsCanonicalized(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	three, one := IntValue(3), IntValue(1)
	inclusive := RangeValue(&RangeVal{Lower: &one, Upper: &three, LowerInc: true, UpperInc: true})
	rows := queryRows(t, db, "SELECT $1::i32range", inclusive)
	if got := rows[0][0].Render(); got != "[1,4)" {
		t.Fatalf("got %s want [1,4)", got)
	}
	crossed := RangeValue(&RangeVal{Lower: &three, Upper: &one, LowerInc: true})
	if code := paramErrCode(t, db, "SELECT $1::i32range", crossed); code != "22000" {
		t.Fatalf("crossed bounds: code %s, want 22000", code)
	}
}

// A Go slice binds as an array parameter, and a nil slice as NULL (api.md §11.2).
func TestSliceArgBindsAsArray(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, tags text[])")
	mustExec(t, s, "INSERT INTO t VALUES (1, '{a}'), (2, '{b}'), (3, '{a,b}')")
	rows, err := s.Query(context.Background(), "SELECT id FROM t WHERE id = ANY($1) ORDER BY id", []int64{1, 3})
	if err != nil {
		t.Fatal(err)
	}
	var ids []int64
	for rows.Next() {
		var id int64
		if err := rows.Scan(&id); err != nil {
			t.Fatal(err)
		}
		ids = append(ids, id)
	}
	if err := rows.Err(); err != nil || !eqInts(ids, 1, 3) {
		t.Fatalf("ids %v err %v, want [1 3]", ids, err)
	}
	var n int64
	if err := s.QueryRow(context.Background(), "SELECT count(*) FROM t WHERE tags @> $1", []string{"a"}).Scan(&n); err != nil || n != 2 {
		t.Fatalf("tags @> [a]: %d %v, want 2", n, err)
	}
	var none []int64
	if err := s.QueryRow(context.Background(), "SELECT count(*) FROM t WHERE id = ANY($1)", none).Scan(&n); err != nil || n != 0 {
		t.Fatalf("nil slice: %d %v, want 0", n, err)
	}
}
