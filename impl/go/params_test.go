package jed

// Phase 7: parameterized queries ($N bind parameters) — spec/design/api.md §5. Parameters are a
// host-API surface (not the shared corpus): their type is inferred from context and supplied
// values are coerced two-phase before any row is touched.

import "testing"

// queryRows runs a parameterized query and returns its rows; t.Fatal on error.
func queryRows(t *testing.T, db *Engine, sql string, params ...Value) [][]Value {
	t.Helper()
	out, err := ExecuteParams(db, sql, params)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("%q: expected a query result", sql)
	}
	return out.Rows
}

// paramErrCode runs a parameterized statement expected to fail and returns its SQLSTATE.
func paramErrCode(t *testing.T, db *Engine, sql string, params ...Value) string {
	t.Helper()
	_, err := ExecuteParams(db, sql, params)
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

func TestWherePkEqParamPointLookup(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
	rows := queryRows(t, db, "SELECT v FROM t WHERE id = $1", IntValue(2))
	if len(rows) != 1 || rows[0][0].Int != 20 {
		t.Fatalf("got %v want [[20]]", rows)
	}
}

func TestParamAdoptsNarrowColumnTypeAndTrapsOverflow(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, s i16)",
		"INSERT INTO t VALUES (1, 100)")
	if c := paramErrCode(t, db, "SELECT id FROM t WHERE s = $1", IntValue(100000)); c != "22003" {
		t.Fatalf("overflow code = %s want 22003", c)
	}
	rows := queryRows(t, db, "SELECT id FROM t WHERE s = $1", IntValue(100))
	if !eqInts(firstInts(rows), 1) {
		t.Fatalf("got %v want [1]", firstInts(rows))
	}
}

func TestInsertValuesParamsRoundTrip(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, name text)")
	if _, err := ExecuteParams(db, "INSERT INTO t VALUES ($1, $2)",
		[]Value{IntValue(7), TextValue("alice")}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id, name FROM t WHERE id = $1", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 || rows[0][1].Str != "alice" {
		t.Fatalf("got %v", rows)
	}
}

func TestInsertParamNullIntoNotNullTraps23502(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, name text NOT NULL)")
	if c := paramErrCode(t, db, "INSERT INTO t VALUES ($1, $2)", IntValue(1), NullValue()); c != "23502" {
		t.Fatalf("code = %s want 23502", c)
	}
}

func TestInsertParamWrongFamilyTraps42804(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)")
	if c := paramErrCode(t, db, "INSERT INTO t VALUES ($1, $2)", IntValue(1), TextValue("x")); c != "42804" {
		t.Fatalf("code = %s want 42804", c)
	}
}

func TestUpdateSetAndWhereParams(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20)")
	if _, err := ExecuteParams(db, "UPDATE t SET v = $1 WHERE id = $2",
		[]Value{IntValue(99), IntValue(2)}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT v FROM t WHERE id = $1", IntValue(2))
	if rows[0][0].Int != 99 {
		t.Fatalf("got %v want 99", rows)
	}
}

func TestDeleteWhereParam(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY)",
		"INSERT INTO t VALUES (1), (2), (3)")
	if _, err := ExecuteParams(db, "DELETE FROM t WHERE id = $1", []Value{IntValue(2)}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id FROM t")
	if !eqInts(firstInts(rows), 1, 3) {
		t.Fatalf("got %v want [1 3]", firstInts(rows))
	}
}

func TestTextParamInference(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, name text)",
		"INSERT INTO t VALUES (1, 'alice'), (2, 'bob')")
	rows := queryRows(t, db, "SELECT id FROM t WHERE name = $1", TextValue("bob"))
	if !eqInts(firstInts(rows), 2) {
		t.Fatalf("got %v want [2]", firstInts(rows))
	}
}

func TestBareSelectParamIsIndeterminate42P18(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT $1 FROM t", IntValue(1)); c != "42P18" {
		t.Fatalf("code = %s want 42P18", c)
	}
}

func TestGapInParamIndicesIs42P18(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)")
	c := paramErrCode(t, db, "SELECT a FROM t WHERE a = $1 OR b = $3", IntValue(1), IntValue(2), IntValue(3))
	if c != "42P18" {
		t.Fatalf("code = %s want 42P18", c)
	}
}

func TestConflictingInferenceIs42804(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (a i32 PRIMARY KEY, name text)")
	if c := paramErrCode(t, db, "SELECT a FROM t WHERE a = $1 OR name = $1", IntValue(1)); c != "42804" {
		t.Fatalf("code = %s want 42804", c)
	}
}

func TestCountMismatchIs42601(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	if c := paramErrCode(t, db, "SELECT id FROM t WHERE id = $1"); c != "42601" {
		t.Fatalf("none: code = %s want 42601", c)
	}
	if c := paramErrCode(t, db, "SELECT id FROM t WHERE id = $1", IntValue(1), IntValue(2)); c != "42601" {
		t.Fatalf("two: code = %s want 42601", c)
	}
}

func TestNullParamThreeValued(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10)")
	rows := queryRows(t, db, "SELECT id FROM t WHERE v = $1", NullValue())
	if len(rows) != 0 {
		t.Fatalf("got %v want []", rows)
	}
}

func TestParamInInList(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY)",
		"INSERT INTO t VALUES (1), (2), (3)")
	rows := queryRows(t, db, "SELECT id FROM t WHERE id IN ($1, $2)", IntValue(1), IntValue(3))
	if !eqInts(firstInts(rows), 1, 3) {
		t.Fatalf("got %v want [1 3]", firstInts(rows))
	}
}

func TestDDLWithParamsTraps42601(t *testing.T) {
	db := NewEngine()
	if c := paramErrCode(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)", IntValue(1)); c != "42601" {
		t.Fatalf("code = %s want 42601", c)
	}
}

func TestParamTypedByCastOperator(t *testing.T) {
	// `$1::int` declares `$1` as int — PostgreSQL types a parameter by its cast target
	// (api.md §5, grammar.md §37). No surrounding context is needed, so this is NOT 42P18.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rows := queryRows(t, db, "SELECT $1::int", IntValue(42))
	if len(rows) != 1 || rows[0][0].Int != 42 {
		t.Fatalf("got %v want [[42]]", rows)
	}
	// The CAST(... AS ...) spelling infers the parameter's type identically.
	rows = queryRows(t, db, "SELECT CAST($1 AS int)", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("got %v want [[7]]", rows)
	}
}

func TestParamCastOperatorNarrowsAndTraps22003(t *testing.T) {
	// `$1::smallint` declares `$1` as i16; a bound value out of i16 range traps 22003 at
	// bind, before any scan.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT $1::smallint", IntValue(100000)); c != "22003" {
		t.Fatalf("code = %s want 22003", c)
	}
}

func TestParamCastToDeferredTargetIs0A000(t *testing.T) {
	// Casting a parameter to a deferred target (text) is 0A000, like any non-string-literal
	// cast to text.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT $1::text", IntValue(1)); c != "0A000" {
		t.Fatalf("code = %s want 0A000", c)
	}
}

func TestCastOperatorInheritsDeferralsAndRejectsLoneColon(t *testing.T) {
	// `::` desugars to CAST, so casting a non-string-literal value to text is the same deferred
	// 0A000 narrowing the CAST spelling carries. The boolean cast has since landed — `5::boolean`
	// is now valid (→ true; cast_bool_int_test.go).
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT 5::text"); c != "0A000" {
		t.Fatalf("5::text code = %s want 0A000", c)
	}
	// A lone `:` is not part of jed's surface — a 42601 syntax error from the lexer.
	if c := paramErrCode(t, db, "SELECT 1 : 2"); c != "42601" {
		t.Fatalf("lone colon code = %s want 42601", c)
	}
}

func TestLexerRejectsBadParamTokens(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for _, sql := range []string{
		"SELECT id FROM t WHERE id = $0",
		"SELECT id FROM t WHERE id = $",
		"SELECT id FROM t WHERE id = $01",
	} {
		if _, err := Execute(db, sql); err == nil {
			t.Fatalf("%q: expected 42601", sql)
		} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "42601" {
			t.Fatalf("%q: code = %v want 42601", sql, err)
		}
	}
}
