package jed

// CHECK constraints — `[CONSTRAINT name] CHECK ( expr )` in both positions
// (spec/design/constraints.md §4, grammar.md §29). Covers what the corpus suite
// (ddl/check.test) cannot: catalog introspection (names, evaluation order, persisted
// expression text), the on-disk round-trip (v4 catalog check list), a corrupted stored
// expression (XX001), and the metered evaluation cost. Mirrors
// impl/rust/tests/check_constraint.rs and impl/ts/tests/check_constraint.test.ts.

import (
	"bytes"
	"slices"
	"strings"
	"testing"
)

func checkErr(t *testing.T, db *Database, sql string) (string, string) {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	ee := err.(*EngineError)
	return ee.Code(), ee.Message
}

func checkNames(t *testing.T, db *Database, table string) []string {
	t.Helper()
	tab, ok := db.Table(table)
	if !ok {
		t.Fatalf("table %s not found", table)
	}
	names := make([]string, len(tab.Checks))
	for i, c := range tab.Checks {
		names[i] = c.Name
	}
	return names
}

// PG's auto-naming, oracle-probed: exactly one distinct referenced column →
// <table>_<col>_check, else <table>_check; the smallest free numeric suffix on a
// collision; names assigned in textual definition order, then the catalog holds them in
// evaluation (name) order.
func TestCheckAutoNamingMatchesPostgres(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a), CHECK (1 < 2), CHECK (b < 100))",
		// Two same-column checks on one column, then a table-level one on it.
		"CREATE TABLE t2 (a int CHECK (a > 0) CHECK (a < 10), CHECK (a = 5))",
		// A table-level check FIRST gets the unsuffixed name (textual order).
		"CREATE TABLE t3 (CHECK (a > 0), a int CHECK (a < 5))",
		// An explicit name occupying a would-be auto name: the auto skips to the next free.
		"CREATE TABLE t9 (a int CONSTRAINT t9_a_check CHECK (a > 0) CHECK (a < 5))",
	)
	if got := checkNames(t, db, "t"); !slices.Equal(got, []string{"t_a_check", "t_b_check", "t_check", "t_check1"}) {
		t.Fatalf("t names = %v", got)
	}
	if got := checkNames(t, db, "t2"); !slices.Equal(got, []string{"t2_a_check", "t2_a_check1", "t2_a_check2"}) {
		t.Fatalf("t2 names = %v", got)
	}
	if got := checkNames(t, db, "t3"); !slices.Equal(got, []string{"t3_a_check", "t3_a_check1"}) {
		t.Fatalf("t3 names = %v", got)
	}
	if got := checkNames(t, db, "t9"); !slices.Equal(got, []string{"t9_a_check", "t9_a_check1"}) {
		t.Fatalf("t9 names = %v", got)
	}
	// The persisted expression text is the re-rendered token sequence.
	tab, _ := db.Table("t")
	texts := make([]string, len(tab.Checks))
	for i, c := range tab.Checks {
		texts[i] = c.ExprText
	}
	if !slices.Equal(texts, []string{"a > 0", "b < 100", "b > a", "1 < 2"}) {
		t.Fatalf("t texts = %v", texts)
	}
}

// The DDL-time rejections, codes and check order all oracle-probed against PostgreSQL.
func TestCheckDDLErrorsMatchPostgres(t *testing.T) {
	db := dbWith(t)
	if code, _ := checkErr(t, db, "CREATE TABLE x (a int CHECK (a + 1))"); code != "42804" {
		t.Fatalf("non-boolean = %s, want 42804", code)
	}
	// Subqueries — scalar, EXISTS, IN — are rejected structurally, before any resolution
	// (the inner table need not exist).
	for _, sql := range []string{
		"CREATE TABLE x (a int CHECK (a > (SELECT v FROM nowhere)))",
		"CREATE TABLE x (a int CHECK (EXISTS (SELECT v FROM nowhere)))",
		"CREATE TABLE x (a int CHECK (a IN (SELECT v FROM nowhere)))",
	} {
		code, msg := checkErr(t, db, sql)
		if code != "0A000" || msg != "cannot use subquery in check constraint" {
			t.Fatalf("%q = %s %q", sql, code, msg)
		}
	}
	code, msg := checkErr(t, db, "CREATE TABLE x (a int CHECK (sum(a) > 0))")
	if code != "42803" || msg != "aggregate functions are not allowed in check constraints" {
		t.Fatalf("aggregate = %s %q", code, msg)
	}
	code, msg = checkErr(t, db, "CREATE TABLE x (a int CHECK (a > $1))")
	if code != "42P02" || msg != "there is no parameter $1" {
		t.Fatalf("param = %s %q", code, msg)
	}
	if code, _ := checkErr(t, db, "CREATE TABLE x (a int CHECK (nope > 0))"); code != "42703" {
		t.Fatalf("unknown column = %s, want 42703", code)
	}
	if code, _ := checkErr(t, db, "CREATE TABLE x (a int CHECK (other.a > 0))"); code != "42P01" {
		t.Fatalf("bad qualifier = %s, want 42P01", code)
	}
	// A forward reference is fine (checks resolve after all columns are known); so is a
	// reference qualified by this table's name.
	mustExecCheck(t, db, "CREATE TABLE fwd (CHECK (b > 0), b int)")
	mustExecCheck(t, db, "CREATE TABLE q (a int CHECK (q.a > 0))")
	// Duplicate explicit name.
	code, msg = checkErr(t, db,
		"CREATE TABLE x (a int CONSTRAINT cc CHECK (a > 0) CONSTRAINT cc CHECK (a < 5))")
	if code != "42710" || msg != "constraint cc for relation x already exists" {
		t.Fatalf("dup name = %s %q", code, msg)
	}
	// An explicit name colliding with an EARLIER auto name (derived names never yield).
	if code, _ := checkErr(t, db,
		"CREATE TABLE tb (a int CHECK (a > 0), CONSTRAINT tb_a_check CHECK (a < 5))"); code != "42710" {
		t.Fatalf("explicit-after-auto = %s, want 42710", code)
	}
	// PRIMARY KEY constraints resolve before any check expression (PG's order).
	code, msg = checkErr(t, db, "CREATE TABLE tc (a int CHECK (nope > 0), PRIMARY KEY (alsonope))")
	if code != "42703" || !strings.Contains(msg, "named in key") {
		t.Fatalf("pk-before-check = %s %q", code, msg)
	}
	// ALL validation precedes ALL naming: a 42703 in a later check beats a 42710 between
	// earlier ones.
	if code, _ := checkErr(t, db,
		"CREATE TABLE td (a int CONSTRAINT cc CHECK (a > 0), CONSTRAINT cc CHECK (nope > 0))"); code != "42703" {
		t.Fatalf("validate-before-name = %s, want 42703", code)
	}
	// The DEFAULT is NOT checked against CHECK at CREATE TABLE.
	mustExecCheck(t, db, "CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0))")
	// CHECK () is a syntax error.
	if code, _ := checkErr(t, db, "CREATE TABLE x (a int, CHECK ())"); code != "42601" {
		t.Fatalf("empty check = %s, want 42601", code)
	}
	// Columns may be NAMED check / constraint (the keywords stay non-reserved).
	mustExecCheck(t, db, "CREATE TABLE odd (check int, constraint int16)")
	mustExecCheck(t, db, "INSERT INTO odd VALUES (1, 2)")
}

func mustExecCheck(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

// Enforcement: FALSE traps 23514 with PG's message; TRUE and NULL pass; checks evaluate
// in NAME order (not definition order); NOT NULL fires before CHECK; CHECK fires before
// the duplicate-key check.
func TestCheckViolationsMatchPostgresOrder(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a))",
		// zz is defined first but aa evaluates first (name order, oracle-probed).
		"CREATE TABLE t5 (a int, CONSTRAINT zz CHECK (a > 0), CONSTRAINT aa CHECK (a > 5))",
		"CREATE TABLE tn (a int NOT NULL CHECK (a > 0))",
		"CREATE TABLE tu (k int PRIMARY KEY, v int CHECK (v > 0))",
	)
	code, msg := checkErr(t, db, "INSERT INTO t VALUES (-1, 5)")
	if code != "23514" || msg != "new row for relation t violates check constraint t_a_check" {
		t.Fatalf("violation = %s %q", code, msg)
	}
	// Violating both: the first in name order reports.
	if _, msg := checkErr(t, db, "INSERT INTO t VALUES (-1, -5)"); !strings.HasSuffix(msg, "t_a_check") {
		t.Fatalf("both violated = %q", msg)
	}
	if _, msg := checkErr(t, db, "INSERT INTO t VALUES (5, 1)"); !strings.HasSuffix(msg, "t_check") {
		t.Fatalf("t_check violated = %q", msg)
	}
	if _, msg := checkErr(t, db, "INSERT INTO t5 VALUES (-1)"); !strings.HasSuffix(msg, "violates check constraint aa") {
		t.Fatalf("name order = %q", msg)
	}
	// NULL passes a check (UNKNOWN is not FALSE).
	mustExecCheck(t, db, "INSERT INTO t VALUES (NULL, NULL)")
	// NOT NULL fires before CHECK on the same row.
	if code, _ := checkErr(t, db, "INSERT INTO tn VALUES (NULL)"); code != "23502" {
		t.Fatalf("not-null-first = %s, want 23502", code)
	}
	// CHECK fires before the duplicate-key check.
	mustExecCheck(t, db, "INSERT INTO tu VALUES (1, 5)")
	if code, _ := checkErr(t, db, "INSERT INTO tu VALUES (1, -1)"); code != "23514" {
		t.Fatalf("check-before-dup = %s, want 23514", code)
	}
	// A runtime error inside a check propagates as itself.
	mustExecCheck(t, db, "CREATE TABLE dz (a int CHECK (10 / a > 0))")
	if code, _ := checkErr(t, db, "INSERT INTO dz VALUES (0)"); code != "22012" {
		t.Fatalf("div-by-zero = %s, want 22012", code)
	}
}

// The two-phase / all-or-nothing pass covers checks: a violating row anywhere in the
// batch (INSERT multi-row, INSERT ... SELECT, UPDATE) leaves the table untouched, and a
// defaulted value goes through the same per-row evaluation.
func TestCheckTwoPhaseAndDefaults(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a int CHECK (a > 0))",
		"CREATE TABLE src (v int)",
		"INSERT INTO src VALUES (3), (-3)",
		"CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0), b int)",
	)
	// Multi-row INSERT: the second row violates → nothing stored.
	if code, _ := checkErr(t, db, "INSERT INTO t VALUES (1), (-1)"); code != "23514" {
		t.Fatalf("multi-row = %s, want 23514", code)
	}
	if rows := db.RowsInKeyOrder("t"); len(rows) != 0 {
		t.Fatalf("two-phase INSERT stored %d rows, want 0", len(rows))
	}
	// INSERT ... SELECT flows through the same per-row checks.
	if code, _ := checkErr(t, db, "INSERT INTO t SELECT v FROM src"); code != "23514" {
		t.Fatalf("insert-select = %s, want 23514", code)
	}
	// UPDATE: a later row violates → no row changes.
	mustExecCheck(t, db, "INSERT INTO t VALUES (1), (2)")
	if code, _ := checkErr(t, db, "UPDATE t SET a = a - 1"); code != "23514" {
		t.Fatalf("update = %s, want 23514", code)
	}
	rows := db.RowsInKeyOrder("t")
	if len(rows) != 2 || rows[0][0].Int != 1 || rows[1][0].Int != 2 {
		t.Fatalf("two-phase UPDATE changed rows: %v", rows)
	}
	// An UPDATE that passes every check applies.
	mustExecCheck(t, db, "UPDATE t SET a = a + 10")
	// The stored default is evaluated per row like any value: a check-violating default
	// traps 23514 at INSERT, not CREATE.
	if code, _ := checkErr(t, db, "INSERT INTO t7 VALUES (DEFAULT, 1)"); code != "23514" {
		t.Fatalf("default slot = %s, want 23514", code)
	}
	if code, _ := checkErr(t, db, "INSERT INTO t7 (b) VALUES (1)"); code != "23514" {
		t.Fatalf("omitted column = %s, want 23514", code)
	}
	mustExecCheck(t, db, "INSERT INTO t7 VALUES (2, 1)")
}

// The full expression surface works inside a check: CASE, BETWEEN, IN, LIKE, IS NULL,
// scalar functions, casts, booleans, decimals, text.
func TestCheckExpressionSurface(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE e (n int, flag boolean, note text, price numeric(8,2), "+
			"CHECK (CASE WHEN n IS NULL THEN TRUE ELSE n BETWEEN 0 AND 100 END), "+
			"CHECK (flag), "+
			"CHECK (note LIKE 'ok%' OR note IN ('a', 'b')), "+
			"CHECK (abs(n) <= CAST(100 AS int)), "+
			"CONSTRAINT price_pos CHECK (price >= 0.50))",
	)
	mustExecCheck(t, db, "INSERT INTO e VALUES (50, TRUE, 'ok then', 1.00), (NULL, TRUE, 'a', 0.50)")
	for _, sql := range []string{
		"INSERT INTO e VALUES (101, TRUE, 'a', 1.00)",
		"INSERT INTO e VALUES (1, FALSE, 'a', 1.00)",
		"INSERT INTO e VALUES (1, TRUE, 'c', 1.00)",
	} {
		if code, _ := checkErr(t, db, sql); code != "23514" {
			t.Fatalf("%q = %s, want 23514", sql, code)
		}
	}
	if _, msg := checkErr(t, db, "INSERT INTO e VALUES (1, TRUE, 'a', 0.49)"); !strings.HasSuffix(msg, "price_pos") {
		t.Fatalf("price_pos = %q", msg)
	}
}

// Check evaluation is metered expression work: each interior node charges operator_eval
// per candidate row (constraints.md §4.4) — the documented exception to "VALUES inserts
// cost zero".
func TestCheckEvaluationIsMetered(t *testing.T) {
	db := dbWith(t, "CREATE TABLE c (a int CHECK (a > 0))")
	// One interior node (>) × one row.
	out, err := Execute(db, "INSERT INTO c VALUES (1)")
	if err != nil || out.Cost != 1 {
		t.Fatalf("1-row insert cost = %d (%v), want 1", out.Cost, err)
	}
	// Two rows × one node.
	out, err = Execute(db, "INSERT INTO c VALUES (2), (3)")
	if err != nil || out.Cost != 2 {
		t.Fatalf("2-row insert cost = %d (%v), want 2", out.Cost, err)
	}
	// UPDATE: page_read(1) + 3×storage_row_read + 3×(a + 1) + 3×(a > 0) = 10.
	out, err = Execute(db, "UPDATE c SET a = a + 1")
	if err != nil || out.Cost != 10 {
		t.Fatalf("update cost = %d (%v), want 10", out.Cost, err)
	}
	// The ceiling aborts mid-validation deterministically.
	db.SetMaxCost(2)
	if _, err := Execute(db, "INSERT INTO c VALUES (4), (5), (6)"); err == nil ||
		err.(*EngineError).Code() != "54P01" {
		t.Fatalf("ceiling = %v, want 54P01", err)
	}
	db.SetMaxCost(0)
	if rows := db.RowsInKeyOrder("c"); len(rows) != 3 {
		t.Fatalf("aborted insert stored rows: %d, want 3", len(rows))
	}
}

// Round-trip: the v4 catalog persists (name, expression text) in evaluation order; a
// reloaded table enforces its checks identically, and a corrupted stored expression is
// XX001 at open.
func TestCheckRoundTripsThroughOnDiskImage(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), "+
			"CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, "+
			"CHECK (note = 'ok' OR note = 'a''b'))",
		"INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), (3, 100, 0.50, 'ok')",
	)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("ToImage: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("LoadDatabase: %v", err)
	}
	tab, _ := loaded.Table("t")
	wantNames := []string{"price_range", "t_b_check", "t_note_check"}
	wantTexts := []string{"price >= 0.50 AND price <= 9999.99", "b > 0", "note = 'ok' OR note = 'a''b'"}
	for i, c := range tab.Checks {
		if c.Name != wantNames[i] || c.ExprText != wantTexts[i] {
			t.Fatalf("check %d = (%q, %q), want (%q, %q)", i, c.Name, c.ExprText, wantNames[i], wantTexts[i])
		}
	}
	if len(tab.Checks) != 3 {
		t.Fatalf("checks = %d, want 3", len(tab.Checks))
	}
	// Still enforced, with the same message.
	code, msg := checkErr(t, loaded, "INSERT INTO t VALUES (4, -1, 1.00, 'ok')")
	if code != "23514" || msg != "new row for relation t violates check constraint t_b_check" {
		t.Fatalf("after reload = %s %q", code, msg)
	}
	if code, _ := checkErr(t, loaded, "INSERT INTO t VALUES (4, 1, 0.10, 'ok')"); code != "23514" {
		t.Fatalf("price after reload = %s, want 23514", code)
	}
	if code, _ := checkErr(t, loaded, "INSERT INTO t VALUES (4, 1, 1.00, 'nope')"); code != "23514" {
		t.Fatalf("note after reload = %s, want 23514", code)
	}
	mustExecCheck(t, loaded, "INSERT INTO t VALUES (4, 1, 1.00, 'a''b')")
	// A second generation (load → image → load) is byte-stable: the text is written back
	// verbatim.
	image2, err := loaded.ToImage(256, 1)
	if err != nil {
		t.Fatalf("ToImage 2: %v", err)
	}
	reloaded, err := LoadDatabase(image2)
	if err != nil {
		t.Fatalf("LoadDatabase 2: %v", err)
	}
	if got := checkNames(t, reloaded, "t"); !slices.Equal(got, wantNames) {
		t.Fatalf("second generation names = %v", got)
	}

	// A stored expression that no longer parses is XX001 (the file lied): patch the text
	// `b > 0` to the same-length garbage `b > (`.
	at := bytes.Index(image, []byte("b > 0"))
	if at < 0 {
		t.Fatal("stored check text not found in image")
	}
	corrupt := bytes.Clone(image)
	corrupt[at+4] = '('
	if _, err := LoadDatabase(corrupt); err == nil || err.(*EngineError).Code() != "XX001" {
		t.Fatalf("corrupt check text = %v, want XX001", err)
	}
}
