package jed

// Composite (row) types — CREATE/DROP TYPE, the catalog type registry, on-disk persistence, and
// (S3) storable composite columns: the ROW(...) constructor, the recursive value codec, the
// INSERT/SELECT round-trip, and record_out rendering (spec/design/composite.md). Mirrors
// impl/rust/tests/composite.rs.

import (
	"reflect"
	"testing"
)

// runComposite executes sql, failing the test on error.
func runComposite(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
}

// errComposite executes sql expecting an error and returns its SQLSTATE code.
func errComposite(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("%s: expected an *EngineError, got %T", sql, err)
	}
	return ee.Code()
}

func TestCreateTypeRegistersFields(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text NOT NULL, zip int32)")
	ct := db.CompositeType("addr")
	if ct == nil {
		t.Fatal("type addr should exist")
	}
	if ct.Name != "addr" {
		t.Errorf("name = %q, want addr", ct.Name)
	}
	if len(ct.Fields) != 2 {
		t.Fatalf("fields = %d, want 2", len(ct.Fields))
	}
	if ct.Fields[0].Name != "street" || ct.Fields[0].Type.ScalarTy() != Text || !ct.Fields[0].NotNull {
		t.Errorf("field 0 wrong: %+v", ct.Fields[0])
	}
	if ct.Fields[1].Name != "zip" || ct.Fields[1].NotNull {
		t.Errorf("field 1 wrong: %+v", ct.Fields[1])
	}
	// Case-insensitive lookup.
	if db.CompositeType("ADDR") == nil {
		t.Error("ADDR should resolve case-insensitively")
	}
}

func TestDuplicateTypeNameIs42710(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (a int32)")
	if code := errComposite(t, db, "CREATE TYPE addr AS (b int32)"); code != "42710" {
		t.Errorf("code = %s, want 42710", code)
	}
}

func TestUnknownFieldTypeIs42704(t *testing.T) {
	db := NewDatabase()
	if code := errComposite(t, db, "CREATE TYPE t AS (a nosuchtype)"); code != "42704" {
		t.Errorf("code = %s, want 42704", code)
	}
}

func TestDuplicateFieldNameIs42701(t *testing.T) {
	db := NewDatabase()
	if code := errComposite(t, db, "CREATE TYPE t AS (a int32, a int64)"); code != "42701" {
		t.Errorf("code = %s, want 42701", code)
	}
}

func TestDropTypeRemovesIt(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (a int32)")
	runComposite(t, db, "DROP TYPE addr")
	if db.CompositeType("addr") != nil {
		t.Error("type addr should be gone")
	}
}

func TestDropMissingTypeIs42704UnlessIfExists(t *testing.T) {
	db := NewDatabase()
	if code := errComposite(t, db, "DROP TYPE nope"); code != "42704" {
		t.Errorf("code = %s, want 42704", code)
	}
	runComposite(t, db, "DROP TYPE IF EXISTS nope") // no-op success
}

func TestDropTypeWithDependentFieldIs2BP01(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE point AS (x int32, y int32)")
	runComposite(t, db, "CREATE TYPE line AS (a point, b point)")
	// point is referenced by line's fields.
	if code := errComposite(t, db, "DROP TYPE point"); code != "2BP01" {
		t.Errorf("code = %s, want 2BP01", code)
	}
	// Dropping the dependent first frees it.
	runComposite(t, db, "DROP TYPE line")
	runComposite(t, db, "DROP TYPE point")
}

// queryRendered runs a query and renders its rows as [][]string (each value via Render), mirroring
// the Rust composite test's `query` helper.
func queryRendered(t *testing.T, db *Database, sql string) [][]string {
	t.Helper()
	rows := query(t, db, sql)
	out := make([][]string, len(rows))
	for i, r := range rows {
		out[i] = make([]string, len(r))
		for j, v := range r {
			out[i][j] = v.Render()
		}
	}
	return out
}

// TestCompositeColumnRowRoundtrip (S3): a composite column is storable. ROW(...) INSERT then SELECT
// round-trips the value and record_out renders it (Main,90210).
func TestCompositeColumnRowRoundtrip(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO person VALUES (1, ROW('Main', 90210))")
	got := queryRendered(t, db, "SELECT id, home FROM person")
	want := [][]string{{"1", "(Main,90210)"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestCompositePrimaryKeyIs0A000: a composite PRIMARY KEY stays rejected (the key encoding is
// authored but unexercised — spec/design/composite.md §6).
func TestCompositePrimaryKeyIs0A000(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (a int32)")
	if code := errComposite(t, db, "CREATE TABLE t (home addr PRIMARY KEY)"); code != "0A000" {
		t.Errorf("code = %s, want 0A000", code)
	}
}

// TestRecordOutQuotingAndNulls: record_out field quoting (spec/design/composite.md §8, PG-exact) — a
// field containing a delimiter / quote / whitespace is double-quoted; inside the quotes PostgreSQL
// **doubles** an embedded `"` → `""` and `\` → `\\` (NOT backslash-escaping). A NULL field is empty;
// the empty string is `""`.
func TestRecordOutQuotingAndNulls(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE rec AS (a text, b int32)")
	runComposite(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, r rec)")
	runComposite(t, db, "INSERT INTO t VALUES (1, ROW('a b', 1))")      // space → quoted
	runComposite(t, db, "INSERT INTO t VALUES (2, ROW('x,y', 2))")      // comma → quoted
	runComposite(t, db, "INSERT INTO t VALUES (3, ROW('', 3))")         // empty string → quoted ""
	runComposite(t, db, `INSERT INTO t VALUES (4, ROW('q"s', 4))`)      // embedded quote → doubled
	runComposite(t, db, "INSERT INTO t VALUES (5, ROW('plain', NULL))") // NULL field → empty
	runComposite(t, db, `INSERT INTO t VALUES (6, ROW('a\b', 7))`)      // embedded backslash → doubled
	rows := queryRendered(t, db, "SELECT r FROM t ORDER BY id")
	want := []string{`("a b",1)`, `("x,y",2)`, `("",3)`, `("q""s",4)`, "(plain,)", `("a\\b",7)`}
	for i, w := range want {
		if rows[i][0] != w {
			t.Errorf("row %d = %q, want %q", i, rows[i][0], w)
		}
	}
}

// TestRecordInRoundtrip (S6): record_in round-trips record_out. A `'(…)'::type` cast and the
// `type '(…)'` typed literal parse a composite text literal back into the value (the inverse of
// record_out).
func TestRecordInRoundtrip(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	// The cast spelling and the typed-literal spelling are equivalent.
	got := queryRendered(t, db, "SELECT '(Main,90210)'::addr")
	if want := [][]string{{"(Main,90210)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("cast rows = %v, want %v", got, want)
	}
	got = queryRendered(t, db, "SELECT addr '(Main,90210)'")
	if want := [][]string{{"(Main,90210)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("typed-literal rows = %v, want %v", got, want)
	}
	// Quoted field with comma; unquoted-empty → NULL; quoted-empty → empty string; doubled quote.
	got = queryRendered(t, db, `SELECT '("x,y",2)'::addr`)
	if want := [][]string{{`("x,y",2)`}}; !reflect.DeepEqual(got, want) {
		t.Errorf("quoted-comma rows = %v, want %v", got, want)
	}
	got = queryRendered(t, db, "SELECT ('(,5)'::addr).street IS NULL")
	if want := [][]string{{"true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("unquoted-empty-NULL rows = %v, want %v", got, want)
	}
	// Field access on a parsed literal pulls the coerced field value.
	got = queryRendered(t, db, "SELECT ('(Main,90210)'::addr).zip")
	if want := [][]string{{"90210"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("field-access rows = %v, want %v", got, want)
	}
}

// TestRecordInNested (S6): a nested composite text literal parses recursively (the inner record is a
// quoted token).
func TestRecordInNested(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE point AS (x int32, y int32)")
	runComposite(t, db, "CREATE TYPE seg AS (a point, b point)")
	got := queryRendered(t, db, `SELECT '("(1,2)","(3,4)")'::seg`)
	if want := [][]string{{`("(1,2)","(3,4)")`}}; !reflect.DeepEqual(got, want) {
		t.Errorf("nested rows = %v, want %v", got, want)
	}
}

// TestRecordInErrors (S6): a malformed composite literal / wrong field count is 22P02; a bad field
// value surfaces that field's parse error (e.g. 22P02 for a non-integer zip).
func TestRecordInErrors(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	if code := errComposite(t, db, "SELECT '(Main)'::addr"); code != "22P02" { // too few fields
		t.Errorf("too-few code = %s, want 22P02", code)
	}
	if code := errComposite(t, db, "SELECT '(a,b,c)'::addr"); code != "22P02" { // too many fields
		t.Errorf("too-many code = %s, want 22P02", code)
	}
	if code := errComposite(t, db, "SELECT 'not a record'::addr"); code != "22P02" { // no parens
		t.Errorf("no-parens code = %s, want 22P02", code)
	}
	if code := errComposite(t, db, "SELECT '(Main,notanint)'::addr"); code != "22P02" { // bad field
		t.Errorf("bad-field code = %s, want 22P02", code)
	}
}

// TestNestedCompositeValueRoundtrip: a nested composite value round-trips and renders with the inner
// record quoted.
func TestNestedCompositeValueRoundtrip(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE point AS (x int32, y int32)")
	runComposite(t, db, "CREATE TYPE seg AS (a point, b point)")
	runComposite(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, s seg)")
	runComposite(t, db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))")
	got := queryRendered(t, db, "SELECT s FROM t")
	want := [][]string{{`("(1,2)","(3,4)")`}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestWholeCompositeNull: a whole-value-NULL composite column stores and renders as NULL.
func TestWholeCompositeNull(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO t (id) VALUES (1)") // home omitted → NULL
	got := queryRendered(t, db, "SELECT home FROM t")
	want := [][]string{{"NULL"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestCompositeValuesPersistThroughImage: composite values survive a serialize → load round-trip
// (the v9 recursive value codec).
func TestCompositeValuesPersistThroughImage(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO p VALUES (1, ROW('Main', 90210))")
	runComposite(t, db, "INSERT INTO p VALUES (2, ROW('Oak', NULL))")
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	got := queryRendered(t, loaded, "SELECT id, home FROM p ORDER BY id")
	want := [][]string{{"1", "(Main,90210)"}, {"2", "(Oak,)"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestFieldAccessSelectsField (S4): `(expr).field` selects one field; the output column is named
// after the field. Works on a parenthesized column and a ROW(...) literal.
func TestFieldAccessSelectsField(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO person VALUES (1, ROW('Main', 90210))")
	// Parenthesized-column field access.
	got := queryRendered(t, db, "SELECT (home).zip, (home).street FROM person")
	want := [][]string{{"90210", "Main"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
	// Field access on an anonymous ROW(...) literal (fields named f1, f2, ...), no FROM.
	got = queryRendered(t, db, "SELECT (ROW('x', 7)).f2")
	want = [][]string{{"7"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestFieldAccessRequiresParens (S4): field access on a column is **parens-required** (PostgreSQL):
// `(home).zip` and `(t.home).zip` work; the unparenthesized `home.zip` / `t.home.zip` are NOT field
// access — they resolve as (multi-part) column references and fail (`home` is no relation → 42P01). A
// bare qualified column `person.home` (no field) reads the whole composite column.
func TestFieldAccessRequiresParens(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO person VALUES (1, ROW('Main', 90210))")
	// `(home).zip`: parenthesized base → field access.
	got := queryRendered(t, db, "SELECT (home).zip FROM person")
	if want := [][]string{{"90210"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("(home).zip rows = %v, want %v", got, want)
	}
	// `person.home`: `person` IS the relation → reads the whole composite column.
	got = queryRendered(t, db, "SELECT person.home FROM person")
	if want := [][]string{{"(Main,90210)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("person.home rows = %v, want %v", got, want)
	}
	// `(t.home).zip`: parenthesized qualified column → field access.
	got = queryRendered(t, db, "SELECT (t.home).zip FROM person t")
	if want := [][]string{{"90210"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("(t.home).zip rows = %v, want %v", got, want)
	}
	// Unparenthesized `home.zip`: `home` is no relation → 42P01 (NOT field access — PG-exact).
	if code := errComposite(t, db, "SELECT home.zip FROM person"); code != "42P01" {
		t.Errorf("home.zip code = %s, want 42P01", code)
	}
}

// TestFieldStarExpandsAllFields (S4): `(expr).*` expands a composite into one output column per
// field, in declaration order.
func TestFieldStarExpandsAllFields(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO person VALUES (1, ROW('Main', 90210))")
	got := queryRendered(t, db, "SELECT id, (home).* FROM person")
	want := [][]string{{"1", "Main", "90210"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestFieldAccessErrors (S4): an unknown field is 42703; field access on a non-composite is 42809;
// a bare qualifier that is neither a relation nor a column is still a missing-FROM-entry (42P01).
func TestFieldAccessErrors(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE person (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO person VALUES (1, ROW('Main', 90210))")
	if code := errComposite(t, db, "SELECT (home).nope FROM person"); code != "42703" {
		t.Errorf("unknown field code = %s, want 42703", code)
	}
	if code := errComposite(t, db, "SELECT (id).zip FROM person"); code != "42809" {
		t.Errorf("non-composite base code = %s, want 42809", code)
	}
	if code := errComposite(t, db, "SELECT nosuch.col FROM person"); code != "42P01" {
		t.Errorf("missing qualifier code = %s, want 42P01", code)
	}
}

func TestCascadeIs0A000(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (a int32)")
	if code := errComposite(t, db, "DROP TYPE addr CASCADE"); code != "0A000" {
		t.Errorf("code = %s, want 0A000", code)
	}
}

func TestNestedTypeSelfOrForwardReferenceIs42704(t *testing.T) {
	db := NewDatabase()
	// Forward reference (point not yet defined) — and self-reference — are unknown types.
	if code := errComposite(t, db, "CREATE TYPE line AS (a point)"); code != "42704" {
		t.Errorf("forward ref code = %s, want 42704", code)
	}
	if code := errComposite(t, db, "CREATE TYPE t AS (a t)"); code != "42704" {
		t.Errorf("self ref code = %s, want 42704", code)
	}
}

// TestTypesPersistThroughImage round-trips a composite type (and a nested one) through the on-disk
// image: it survives serialize → load, byte-backed by the v9 catalog type-definition section.
func TestTypesPersistThroughImage(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE point AS (x int32 NOT NULL, y int32 NOT NULL)")
	runComposite(t, db, "CREATE TYPE line AS (a point, b point)")
	runComposite(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)")
	runComposite(t, db, "INSERT INTO t VALUES (1, 10)")

	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}

	point := loaded.CompositeType("point")
	if point == nil {
		t.Fatal("point should persist")
	}
	if len(point.Fields) != 2 || !point.Fields[0].NotNull {
		t.Errorf("point wrong: %+v", point)
	}

	line := loaded.CompositeType("line")
	if line == nil {
		t.Fatal("line should persist")
	}
	if len(line.Fields) != 2 {
		t.Fatalf("line fields = %d, want 2", len(line.Fields))
	}
	// A nested field references its composite by name.
	if line.Fields[0].Type.Comp == nil || line.Fields[0].Type.Comp.Name != "point" {
		t.Errorf("line field 0 should reference point, got %+v", line.Fields[0].Type)
	}
	// The table and its row survive too.
	tbl, ok := loaded.Table("t")
	if !ok || len(tbl.Columns) != 2 {
		t.Errorf("table t wrong: %+v", tbl)
	}
}

// composite_equality_3vl (S5): composite equality is element-wise 3VL (PG row comparison). `=` is
// FALSE if any field is FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE.
func TestCompositeEquality3VL(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE rec AS (a int32, b int32)")
	// Equal rows.
	if got, want := queryRendered(t, db, "SELECT ROW(1, 2) = ROW(1, 2)"), [][]string{{"true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("equal rows: got %v, want %v", got, want)
	}
	// A NULL field with all-else-equal → UNKNOWN (renders NULL).
	if got, want := queryRendered(t, db, "SELECT ROW(1, NULL) = ROW(1, 2)"), [][]string{{"NULL"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("null field: got %v, want %v", got, want)
	}
	// A FALSE field dominates a NULL field → FALSE.
	if got, want := queryRendered(t, db, "SELECT ROW(1, NULL) = ROW(2, 2)"), [][]string{{"false"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("false dominates null: got %v, want %v", got, want)
	}
	// The 3VL negation via NOT (jed has no `<>` operator).
	if got, want := queryRendered(t, db, "SELECT NOT (ROW(1, 2) = ROW(1, 3))"), [][]string{{"true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("not-eq: got %v, want %v", got, want)
	}
}

// composite_ordering_lexicographic (S5): `< <= > >=` is lexicographic — the first non-equal field
// decides.
func TestCompositeOrderingLexicographic(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE rec AS (a int32, b int32)")
	if got, want := queryRendered(t, db, "SELECT ROW(1, 2) < ROW(1, 3)"), [][]string{{"true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("(1,2)<(1,3): got %v, want %v", got, want)
	}
	if got, want := queryRendered(t, db, "SELECT ROW(2, 1) < ROW(1, 9)"), [][]string{{"false"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("(2,1)<(1,9): got %v, want %v", got, want)
	}
	if got, want := queryRendered(t, db, "SELECT ROW(1, 2) >= ROW(1, 2)"), [][]string{{"true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("(1,2)>=(1,2): got %v, want %v", got, want)
	}
}

// composite_column_compare_and_order (S5): a composite column compares against a ROW(…) value in
// WHERE (element-wise), and ORDER BY over the composite column sorts lexicographically.
func TestCompositeColumnCompareAndOrder(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO p VALUES (1, ROW('Oak', 30))")
	runComposite(t, db, "INSERT INTO p VALUES (2, ROW('Oak', 10))")
	runComposite(t, db, "INSERT INTO p VALUES (3, ROW('Elm', 99))")
	// WHERE composite = ROW(...).
	if got, want := queryRendered(t, db, "SELECT id FROM p WHERE home = ROW('Oak', 10)"), [][]string{{"2"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("where composite =: got %v, want %v", got, want)
	}
	// ORDER BY composite column — lexicographic: Elm/99, Oak/10, Oak/30.
	if got, want := queryRendered(t, db, "SELECT id FROM p ORDER BY home"), [][]string{{"3"}, {"2"}, {"1"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("order by composite: got %v, want %v", got, want)
	}
}

// composite_is_null_all_fields (S5): PG's all-fields IS NULL / IS NOT NULL rule — they are NOT
// negations. A partially-NULL row is FALSE for both; an all-NULL row IS NULL; a whole-value NULL IS
// NULL.
func TestCompositeIsNullAllFields(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE rec AS (a int32, b int32)")
	// All fields present → IS NOT NULL true, IS NULL false.
	if got, want := queryRendered(t, db, "SELECT ROW(1, 2) IS NULL, ROW(1, 2) IS NOT NULL"), [][]string{{"false", "true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("all present: got %v, want %v", got, want)
	}
	// Partially NULL → FALSE for both (the PG gotcha).
	if got, want := queryRendered(t, db, "SELECT ROW(1, NULL) IS NULL, ROW(1, NULL) IS NOT NULL"), [][]string{{"false", "false"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("partial null: got %v, want %v", got, want)
	}
	// All fields NULL → IS NULL true, IS NOT NULL false.
	if got, want := queryRendered(t, db, "SELECT ROW(NULL, NULL) IS NULL, ROW(NULL, NULL) IS NOT NULL"), [][]string{{"true", "false"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("all null: got %v, want %v", got, want)
	}
}

// composite_is_null_non_recursive (S5): the all-fields IS NULL rule is ONE LEVEL DEEP, not recursive
// (the empirically-probed PG behavior). A composite-valued field is a non-NULL value, so it counts
// as PRESENT: a nested all-NULL row is therefore IS NULL = FALSE and IS NOT NULL = TRUE.
func TestCompositeIsNullNonRecursive(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE point AS (x int32, y int32)")
	runComposite(t, db, "CREATE TYPE seg AS (a point, b point)")
	// The two inner rows are non-null values → the outer row is NOT all-(SQL-)null → IS NULL false,
	// IS NOT NULL true. PG does NOT recurse into the inner all-NULL rows.
	if got, want := queryRendered(t, db,
		"SELECT ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NULL, ROW(ROW(NULL, NULL), ROW(NULL, NULL)) IS NOT NULL"),
		[][]string{{"false", "true"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("nested all-null: got %v, want %v", got, want)
	}
	// A SQL-NULL field + a composite field → IS NULL false (not all null), IS NOT NULL false (the
	// NULL field is not present).
	if got, want := queryRendered(t, db,
		"SELECT ROW(NULL, ROW(1, 2)) IS NULL, ROW(NULL, ROW(1, 2)) IS NOT NULL"),
		[][]string{{"false", "false"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("null + composite: got %v, want %v", got, want)
	}
}

// composite_distinct_and_group_by (S5): DISTINCT and GROUP BY over a composite column use the
// recursive value key (NULL-safe).
func TestCompositeDistinctAndGroupBy(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runComposite(t, db, "CREATE TABLE p (id int32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO p VALUES (1, ROW('Oak', 10))")
	runComposite(t, db, "INSERT INTO p VALUES (2, ROW('Oak', 10))")
	runComposite(t, db, "INSERT INTO p VALUES (3, ROW('Elm', 20))")
	// DISTINCT collapses the two identical Oak/10 rows → 2 distinct composites.
	if got, want := queryRendered(t, db, "SELECT DISTINCT home FROM p ORDER BY home"), [][]string{{"(Elm,20)"}, {"(Oak,10)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("distinct: got %v, want %v", got, want)
	}
	// GROUP BY the composite column → count per group.
	if got, want := queryRendered(t, db, "SELECT home, count(*) FROM p GROUP BY home ORDER BY home"),
		[][]string{{"(Elm,20)", "1"}, {"(Oak,10)", "2"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("group by: got %v, want %v", got, want)
	}
}

// composite_comparison_type_errors (S5): a composite compared with a non-composite, or with a
// different-arity row, is 42804.
func TestCompositeComparisonTypeErrors(t *testing.T) {
	db := NewDatabase()
	runComposite(t, db, "CREATE TYPE rec AS (a int32, b int32)")
	runComposite(t, db, "CREATE TABLE p (id int32 PRIMARY KEY, r rec)")
	runComposite(t, db, "INSERT INTO p VALUES (1, ROW(1, 2))")
	// Composite vs scalar.
	if got := errComposite(t, db, "SELECT r = 1 FROM p"); got != "42804" {
		t.Errorf("composite vs scalar: got %q, want 42804", got)
	}
	// Different row sizes.
	if got := errComposite(t, db, "SELECT ROW(1, 2) = ROW(1, 2, 3)"); got != "42804" {
		t.Errorf("row size mismatch: got %q, want 42804", got)
	}
}
