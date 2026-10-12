package jed

// Phase 7: parameterized queries ($N bind parameters) — spec/design/api.md §5. Parameters are a
// host-API surface (not the shared corpus): their type is inferred from context and supplied
// values are coerced two-phase before any row is touched.

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

func TestWherePkEqParamPointLookup(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
	rows := queryRows(t, db, "SELECT v FROM t WHERE id = $1", IntValue(2))
	if len(rows) != 1 || rows[0][0].Int != 20 {
		t.Fatalf("got %v want [[20]]", rows)
	}
	out, err := queryOutcome(db, "SELECT v FROM t WHERE id = $1", []Value{NullValue()})
	if err != nil || len(out.Rows) != 0 || out.Cost != 0 {
		t.Fatalf("NULL point parameter = rows %v cost %d err %v, want empty cost 0", out.Rows, out.Cost, err)
	}
	if c := paramErrCode(t, db, "SELECT v FROM t WHERE id = $1", IntValue(int64(1)<<31)); c != "22003" {
		t.Fatalf("out-of-range point parameter code = %s want 22003", c)
	}
}

func TestCompositePKParamTupleBound(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (a i32, b i16, v i32, PRIMARY KEY (b, a))",
		"INSERT INTO t VALUES (1, 1, 10), (2, 1, 20), (3, 1, 30), (1, 2, 40)")
	rows := queryRows(t, db, "SELECT v FROM t WHERE b = $1 AND a >= $2 ORDER BY a", IntValue(1), IntValue(2))
	if !eqInts(firstInts(rows), 20, 30) {
		t.Fatalf("got %v want [20 30]", firstInts(rows))
	}
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

func TestParamAdoptsNarrowColumnTypeAndTrapsOverflow(t *testing.T) {
	t.Parallel()
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
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, name text)")
	if _, err := queryOutcome(db, "INSERT INTO t VALUES ($1, $2)", []Value{IntValue(7), TextValue("alice")}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id, name FROM t WHERE id = $1", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 || rows[0][1].str() != "alice" {
		t.Fatalf("got %v", rows)
	}
}

func TestInsertParamNullIntoNotNullTraps23502(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, name text NOT NULL)")
	if c := paramErrCode(t, db, "INSERT INTO t VALUES ($1, $2)", IntValue(1), NullValue()); c != "23502" {
		t.Fatalf("code = %s want 23502", c)
	}
}

func TestInsertParamWrongFamilyTraps42804(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)")
	if c := paramErrCode(t, db, "INSERT INTO t VALUES ($1, $2)", IntValue(1), TextValue("x")); c != "42804" {
		t.Fatalf("code = %s want 42804", c)
	}
}

func TestUpdateSetAndWhereParams(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20)")
	if _, err := queryOutcome(db, "UPDATE t SET v = $1 WHERE id = $2", []Value{IntValue(99), IntValue(2)}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT v FROM t WHERE id = $1", IntValue(2))
	if rows[0][0].Int != 99 {
		t.Fatalf("got %v want 99", rows)
	}
}

func TestDeleteWhereParam(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY)",
		"INSERT INTO t VALUES (1), (2), (3)")
	if _, err := queryOutcome(db, "DELETE FROM t WHERE id = $1", []Value{IntValue(2)}); err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id FROM t")
	if !eqInts(firstInts(rows), 1, 3) {
		t.Fatalf("got %v want [1 3]", firstInts(rows))
	}
}

func TestTextParamInference(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, name text)",
		"INSERT INTO t VALUES (1, 'alice'), (2, 'bob')")
	rows := queryRows(t, db, "SELECT id FROM t WHERE name = $1", TextValue("bob"))
	if !eqInts(firstInts(rows), 2) {
		t.Fatalf("got %v want [2]", firstInts(rows))
	}
}

func TestCoalesceInfersParamsFromCommonType(t *testing.T) {
	// COALESCE resolves its arguments before it knows their common type, then feeds that type back
	// to bare parameters before binding (grammar.md §51).
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rows := queryRows(t, db, "SELECT COALESCE($1, 10)", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("first argument: got %v want [[7]]", rows)
	}
	rows = queryRows(t, db, "SELECT COALESCE(NULL, $1, 10)", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("middle argument: got %v want [[7]]", rows)
	}
}

func TestCaseInfersParamsFromCommonResultType(t *testing.T) {
	// CASE result arms use the same common-type feedback as COALESCE (grammar.md §23).
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rows := queryRows(t, db, "SELECT CASE WHEN true THEN $1 ELSE 10 END", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("THEN arm: got %v want [[7]]", rows)
	}
	rows = queryRows(t, db, "SELECT CASE WHEN false THEN 10 ELSE $1 END", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("ELSE arm: got %v want [[7]]", rows)
	}
}

func TestLeastInfersParamFromCommonType(t *testing.T) {
	// GREATEST/LEAST note a bare parameter at their unified scalar type, like a comparison operand
	// (grammar.md §52). Source branch A skipped this, so LEAST($1, 10) failed 42P18. A per-core
	// test because param binding is a host-API surface (not the shared corpus).
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	rows := queryRows(t, db, "SELECT LEAST($1, 10)", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("got %v want [[7]]", rows)
	}
}

func TestBareSelectParamIsIndeterminate42P18(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT $1 FROM t", IntValue(1)); c != "42P18" {
		t.Fatalf("code = %s want 42P18", c)
	}
}

func TestGapInParamIndicesIs42P18(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)")
	c := paramErrCode(t, db, "SELECT a FROM t WHERE a = $1 OR b = $3", IntValue(1), IntValue(2), IntValue(3))
	if c != "42P18" {
		t.Fatalf("code = %s want 42P18", c)
	}
}

func TestConflictingInferenceIs42804(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (a i32 PRIMARY KEY, name text)")
	if c := paramErrCode(t, db, "SELECT a FROM t WHERE a = $1 OR name = $1", IntValue(1)); c != "42804" {
		t.Fatalf("code = %s want 42804", c)
	}
}

func TestCountMismatchIs42601(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	if c := paramErrCode(t, db, "SELECT id FROM t WHERE id = $1"); c != "42601" {
		t.Fatalf("none: code = %s want 42601", c)
	}
	if c := paramErrCode(t, db, "SELECT id FROM t WHERE id = $1", IntValue(1), IntValue(2)); c != "42601" {
		t.Fatalf("two: code = %s want 42601", c)
	}
}

func TestNullParamThreeValued(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10)")
	rows := queryRows(t, db, "SELECT id FROM t WHERE v = $1", NullValue())
	if len(rows) != 0 {
		t.Fatalf("got %v want []", rows)
	}
}

func TestParamInInList(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY)",
		"INSERT INTO t VALUES (1), (2), (3)")
	rows := queryRows(t, db, "SELECT id FROM t WHERE id IN ($1, $2)", IntValue(1), IntValue(3))
	if !eqInts(firstInts(rows), 1, 3) {
		t.Fatalf("got %v want [1 3]", firstInts(rows))
	}
}

func TestDDLWithParamsTraps42601(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if c := paramErrCode(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)", IntValue(1)); c != "42601" {
		t.Fatalf("code = %s want 42601", c)
	}
}

func TestParamTypedByCastOperator(t *testing.T) {
	t.Parallel()
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
	t.Parallel()
	// `$1::smallint` declares `$1` as i16; a bound value out of i16 range traps 22003 at
	// bind, before any scan.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT $1::smallint", IntValue(100000)); c != "22003" {
		t.Fatalf("code = %s want 22003", c)
	}
}

func TestCastOperatorInheritsMatrixAndRejectsLoneColon(t *testing.T) {
	t.Parallel()
	// `::` desugars to CAST, so an integer → interval cast is the same forbidden pair (42804) the
	// CAST spelling reports.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	if c := paramErrCode(t, db, "SELECT 5::interval"); c != "42804" {
		t.Fatalf("5::interval code = %s want 42804", c)
	}
	// A lone `:` is not part of jed's surface — a 42601 syntax error from the lexer.
	if c := paramErrCode(t, db, "SELECT 1 : 2"); c != "42601" {
		t.Fatalf("lone colon code = %s want 42601", c)
	}
}

func TestLexerRejectsBadParamTokens(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for _, sql := range []string{
		"SELECT id FROM t WHERE id = $0",
		"SELECT id FROM t WHERE id = $",
		"SELECT id FROM t WHERE id = $01",
	} {
		if _, err := queryOutcome(db, sql, nil); err == nil {
			t.Fatalf("%q: expected 42601", sql)
		} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "42601" {
			t.Fatalf("%q: code = %v want 42601", sql, err)
		}
	}
}

// LIKE … ESCAPE desugars to like_escape(p, e) (grammar.md §22). A bare $N pattern or escape is typed
// text — the type like_escape declares, as PostgreSQL infers it — so either may be bound, and a bound
// pattern/escape charges exactly like the same literal (a parameter is a constant operand, so the
// growth kernel charges no output bytes).
func TestLikeEscapeBoundParameters(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s text)",
		"INSERT INTO t VALUES (1, 'a%'), (2, 'ab'), (3, NULL)",
	)
	const literal = "SELECT id FROM t WHERE s LIKE 'a#%' ESCAPE '#' ORDER BY id"
	const byPattern = "SELECT id FROM t WHERE s LIKE $1 ESCAPE '#' ORDER BY id"
	const byEscape = "SELECT id FROM t WHERE s LIKE 'a#%' ESCAPE $1 ORDER BY id"
	for _, tc := range []struct {
		sql   string
		param Value
		want  []int64
	}{
		{byPattern, TextValue("a#%"), []int64{1}},
		{byPattern, TextValue("a%"), []int64{1, 2}},
		{byEscape, TextValue("#"), []int64{1}},
		{byEscape, NullValue(), []int64{}},
	} {
		got := firstInts(queryRows(t, db, tc.sql, tc.param))
		if len(got) != len(tc.want) {
			t.Fatalf("%q %v: ids = %v, want %v", tc.sql, tc.param, got, tc.want)
		}
		for i := range got {
			if got[i] != tc.want[i] {
				t.Fatalf("%q %v: ids = %v, want %v", tc.sql, tc.param, got, tc.want)
			}
		}
	}
	if code := paramErrCode(t, db, byEscape, TextValue("xy")); code != "22025" {
		t.Errorf("escape 'xy': code %s, want 22025", code)
	}
	lit := mustCost(t, db, literal)
	for _, tc := range []struct {
		sql   string
		param Value
	}{{byPattern, TextValue("a#%")}, {byEscape, TextValue("#")}} {
		out, err := queryOutcome(db, tc.sql, []Value{tc.param})
		if err != nil {
			t.Fatal(err)
		}
		if out.Cost != lit {
			t.Errorf("%q: parameter cost %d, literal cost %d", tc.sql, out.Cost, lit)
		}
	}
}

// A bare $N operand of `||` with no array operand is typed text (string-functions.md §5), as
// PostgreSQL infers its unknown parameter for textcat / textanycat: it concatenates with a text,
// string-literal, or non-text operand, and a NULL binding is a NULL result.
func TestTextConcatBoundParameters(t *testing.T) {
	t.Parallel()
	db := dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, s text)",
		"INSERT INTO t VALUES (1, 'x')",
	)
	for _, tc := range []struct {
		sql    string
		params []Value
		want   Value
	}{
		{"SELECT $1 || 'b' FROM t", []Value{TextValue("a")}, TextValue("ab")},
		{"SELECT s || $1 FROM t", []Value{TextValue("a")}, TextValue("xa")},
		{"SELECT $1 || id FROM t", []Value{TextValue("a")}, TextValue("a1")},
		{"SELECT $1 || $2 FROM t", []Value{TextValue("a"), TextValue("b")}, TextValue("ab")},
		{"SELECT $1 || s FROM t", []Value{NullValue()}, NullValue()},
	} {
		rows := queryRows(t, db, tc.sql, tc.params...)
		if len(rows) != 1 || rows[0][0].Kind != tc.want.Kind ||
			(tc.want.Kind == ValText && rows[0][0].str() != tc.want.str()) {
			t.Errorf("%q %v: got %v, want %v", tc.sql, tc.params, rows, tc.want)
		}
	}
	// The parameter is declared text, so an integer binding does not coerce.
	if code := paramErrCode(t, db, "SELECT $1 || id FROM t", IntValue(5)); code != "42804" {
		t.Errorf("an integer binding for a text parameter: code %s, want 42804", code)
	}
}
