package abide

// Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
// minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
// connectives, operator precedence, and parentheses. These complement the conformance
// corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

import "testing"

// scalar runs a single-row, single-column query and returns the lone value.
func scalar(t *testing.T, db *Database, sql string) Value {
	t.Helper()
	rows := query(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%q: expected one row of one column, got %v", sql, rows)
	}
	return rows[0][0]
}

func setupExpr(t *testing.T) *Database {
	return dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
		"INSERT INTO t VALUES (1, 6, 4)",
		"INSERT INTO t VALUES (2, 20, 6)",
		"INSERT INTO t VALUES (3, -7, 3)",
	)
}

func TestArithmeticAndPrecedence(t *testing.T) {
	db := setupExpr(t)
	cases := []struct {
		sql  string
		want int64
	}{
		{"SELECT 6 + 4 * 2 FROM t WHERE id = 1", 14},   // * binds tighter than +
		{"SELECT (6 + 4) * 2 FROM t WHERE id = 1", 20}, // parens override
		{"SELECT a + b FROM t WHERE id = 2", 26},
		{"SELECT a * b FROM t WHERE id = 2", 120},
		{"SELECT a / b FROM t WHERE id = 3", -2}, // truncate toward zero
		{"SELECT a % b FROM t WHERE id = 3", -1}, // remainder sign follows dividend
	}
	for _, c := range cases {
		if got := scalar(t, db, c.sql); got.Kind != ValInt || got.Int != c.want {
			t.Errorf("%q = %v, want %d", c.sql, got, c.want)
		}
	}
}

func TestArithmeticInWhere(t *testing.T) {
	db := setupExpr(t)
	if got := queryIDs(t, db, "SELECT id FROM t WHERE a + b = 26 ORDER BY id"); !eqInts(got, 2) {
		t.Errorf("got %v want [2]", got)
	}
}

func TestOverflowTrapsAtResultType(t *testing.T) {
	// int32 + int32 overflows at the int32 boundary even though it fits int64.
	db := dbWith(t,
		"CREATE TABLE e (id int32 PRIMARY KEY, a int32, b int32)",
		"INSERT INTO e VALUES (1, 2147483647, 1)",
	)
	wantErr(t, db, "SELECT a + b FROM e WHERE id = 1", "22003")
	if got := scalar(t, db, "SELECT CAST(a AS int64) + b FROM e WHERE id = 1"); got.Int != 2147483648 {
		t.Errorf("widened sum = %v, want 2147483648", got)
	}
}

func TestDivideAndModuloByZero(t *testing.T) {
	db := setupExpr(t)
	wantErr(t, db, "SELECT a / 0 FROM t WHERE id = 1", "22012")
	wantErr(t, db, "SELECT a % 0 FROM t WHERE id = 1", "22012")
}

func TestUnaryMinusAndInt64Min(t *testing.T) {
	db := setupExpr(t)
	if got := scalar(t, db, "SELECT -a FROM t WHERE id = 1"); got.Int != -6 {
		t.Errorf("-a = %v, want -6", got)
	}
	if got := scalar(t, db, "SELECT - -a FROM t WHERE id = 1"); got.Int != 6 {
		t.Errorf("- -a = %v, want 6", got)
	}
	// int64's minimum is reachable only via unary minus.
	if got := scalar(t, db, "SELECT -9223372036854775808 FROM t WHERE id = 1"); got.Int != -9223372036854775808 {
		t.Errorf("int64 min = %v", got)
	}
	wantErr(t, db, "SELECT 9223372036854775808 FROM t WHERE id = 1", "22003") // bare 2^63
	wantErr(t, db, "SELECT 9223372036854775809 FROM t WHERE id = 1", "42601") // > 2^63 (lex)
}

func TestComparisonsProjectBooleans(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
		"INSERT INTO t VALUES (1, 5, 5)",
		"INSERT INTO t VALUES (2, 5, 9)",
		"INSERT INTO t VALUES (3, 5, NULL)",
	)
	rows := query(t, db, "SELECT a = b FROM t ORDER BY id")
	want := []Value{BoolValue(true), BoolValue(false), NullValue()}
	for i, w := range want {
		if rows[i][0] != w {
			t.Errorf("row %d = %v, want %v", i, rows[i][0], w)
		}
	}
	if got := scalar(t, db, "SELECT TRUE FROM t WHERE id = 1"); got != BoolValue(true) {
		t.Errorf("TRUE = %v", got)
	}
	if BoolValue(true).Render() != "true" || BoolValue(false).Render() != "false" {
		t.Errorf("boolean render mismatch")
	}
}

func TestIsDistinctFrom(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
		"INSERT INTO t VALUES (1, 5, 5)",       // present, equal
		"INSERT INTO t VALUES (2, 5, 9)",       // present, unequal
		"INSERT INTO t VALUES (3, NULL, 5)",    // one NULL
		"INSERT INTO t VALUES (4, NULL, NULL)", // both NULL
	)
	// IS NOT DISTINCT FROM is NULL-safe equality — always a definite boolean: two NULLs
	// are "the same", a NULL vs a present value is not.
	same := []Value{BoolValue(true), BoolValue(false), BoolValue(false), BoolValue(true)}
	nd := query(t, db, "SELECT a IS NOT DISTINCT FROM b FROM t ORDER BY id")
	for i, w := range same {
		if nd[i][0] != w {
			t.Errorf("IS NOT DISTINCT FROM row %d = %v, want %v", i, nd[i][0], w)
		}
	}
	// IS DISTINCT FROM is its exact negation (also always definite, never NULL).
	d := query(t, db, "SELECT a IS DISTINCT FROM b FROM t ORDER BY id")
	for i := range same {
		want := BoolValue(same[i] == BoolValue(false))
		if d[i][0] != want {
			t.Errorf("IS DISTINCT FROM row %d = %v, want %v", i, d[i][0], want)
		}
	}
	// WHERE keeps the "same" rows, including both-NULL — which plain `=` would drop.
	if got := queryIDs(t, db, "SELECT id FROM t WHERE a IS NOT DISTINCT FROM b ORDER BY id"); !eqInts(got, 1, 4) {
		t.Errorf("not-distinct WHERE got %v want [1 4]", got)
	}
	// Distinct-from-NULL coincides with IS NOT NULL (selects the present values).
	if got := queryIDs(t, db, "SELECT id FROM t WHERE a IS DISTINCT FROM NULL ORDER BY id"); !eqInts(got, 1, 2) {
		t.Errorf("distinct-from-NULL WHERE got %v want [1 2]", got)
	}
	// Same operand contract as `=`: non-associative chaining and boolean operands error.
	wantErr(t, db, "SELECT id FROM t WHERE a IS DISTINCT FROM b IS DISTINCT FROM b", "42601")
	wantErr(t, db, "SELECT id FROM t WHERE (a = b) IS NOT DISTINCT FROM (a = b)", "42804")
}

func TestKleeneConnectives(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE tv (id int32 PRIMARY KEY, p int32, q int32)",
		"INSERT INTO tv VALUES (1, 0, 0)", // false, false
		"INSERT INTO tv VALUES (2, 0, 1)", // false, true
	)
	// false AND unknown = false (a dominant FALSE absorbs NULL).
	if got := scalar(t, db, "SELECT (p = 1) AND (q = NULL) FROM tv WHERE id = 1"); got != BoolValue(false) {
		t.Errorf("false AND unknown = %v, want false", got)
	}
	// true OR unknown = true.
	if got := scalar(t, db, "SELECT (q = 1) OR (p = NULL) FROM tv WHERE id = 2"); got != BoolValue(true) {
		t.Errorf("true OR unknown = %v, want true", got)
	}
	// NOT unknown = unknown (genuine propagation).
	if got := scalar(t, db, "SELECT NOT (p = NULL) FROM tv WHERE id = 1"); got != NullValue() {
		t.Errorf("NOT unknown = %v, want NULL", got)
	}
}

func TestTypeErrorsAndBooleanNarrowings(t *testing.T) {
	db := setupExpr(t)
	wantErr(t, db, "SELECT id FROM t WHERE a", "42804")                 // WHERE must be boolean
	wantErr(t, db, "SELECT id FROM t WHERE a AND b", "42804")           // AND needs boolean
	wantErr(t, db, "SELECT (a = b) + 1 FROM t WHERE id = 1", "42804")   // arithmetic on boolean
	wantErr(t, db, "SELECT id FROM t WHERE (a = b) = (a = b)", "42804") // bool = bool
	wantErr(t, db, "CREATE TABLE bt (id int32 PRIMARY KEY, flag boolean)", "0A000")
	wantErr(t, db, "SELECT CAST(a AS boolean) FROM t WHERE id = 1", "0A000")
	wantErr(t, db, "SELECT CAST(a = b AS int32) FROM t WHERE id = 1", "42804") // no bool->int cast
}
