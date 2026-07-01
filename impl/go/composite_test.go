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
func runComposite(t *testing.T, db dbHandle, sql string) {
	t.Helper()
	if _, err := db.Execute(sql, nil); err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
}

// errComposite executes sql expecting an error and returns its SQLSTATE code.
func errComposite(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := db.Execute(sql, nil)
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
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)")
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
	if ct.Fields[0].Name != "street" || ct.Fields[0].Type.ScalarTy() != scalarText || !ct.Fields[0].NotNull {
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

func TestDropTypeRemovesIt(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (a i32)")
	runComposite(t, db, "DROP TYPE addr")
	if db.CompositeType("addr") != nil {
		t.Error("type addr should be gone")
	}
}

// queryRendered runs a query and renders its rows as [][]string (each value via Render), mirroring
// the Rust composite test's `query` helper.
func queryRendered(t *testing.T, db dbHandle, sql string) [][]string {
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

// TestNestedCompositeValueRoundtrip: a nested composite value round-trips and renders with the inner
// record quoted.
func TestNestedCompositeValueRoundtrip(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE point AS (x i32, y i32)")
	runComposite(t, db, "CREATE TYPE seg AS (a point, b point)")
	runComposite(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, s seg)")
	runComposite(t, db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))")
	got := queryRendered(t, db, "SELECT s FROM t")
	want := [][]string{{`("(1,2)","(3,4)")`}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestCompositeValuesPersistThroughImage: composite values survive a serialize → load round-trip
// (the v9 recursive value codec).
func TestCompositeValuesPersistThroughImage(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)")
	runComposite(t, db, "INSERT INTO p VALUES (1, ROW('Main', 90210))")
	runComposite(t, db, "INSERT INTO p VALUES (2, ROW('Oak', NULL))")
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := loadEngine(image)
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
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TABLE person (id i32 PRIMARY KEY, home addr)")
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

func TestNestedTypeSelfOrForwardReferenceIs42704(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
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
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)")
	runComposite(t, db, "CREATE TYPE line AS (a point, b point)")
	runComposite(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, n i32)")
	runComposite(t, db, "INSERT INTO t VALUES (1, 10)")

	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := loadEngine(image)
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
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE rec AS (a i32, b i32)")
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

// composite_column_compare_and_order (S5): a composite column compares against a ROW(…) value in
// WHERE (element-wise), and ORDER BY over the composite column sorts lexicographically.
func TestCompositeColumnCompareAndOrder(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TABLE p (id i32 PRIMARY KEY, home addr)")
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

// composite_is_null_non_recursive (S5): the all-fields IS NULL rule is ONE LEVEL DEEP, not recursive
// (the empirically-probed PG behavior). A composite-valued field is a non-NULL value, so it counts
// as PRESENT: a nested all-NULL row is therefore IS NULL = FALSE and IS NOT NULL = TRUE.
func TestCompositeIsNullNonRecursive(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE point AS (x i32, y i32)")
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

// --- a composite type with an array-typed field (spec/design/array.md §12 — the mirror of an
// array-of-composite element). The catalog persists the array field as type_code 15 + the inline
// element descriptor; the value codec / comparison / text-I/O all recurse. Mirrors the Rust tests. ---

// TestCreateTypeWithArrayFieldRegisters: `CREATE TYPE t AS (xs i32[])` registers an array field.
func TestCreateTypeWithArrayFieldRegisters(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE poly AS (name text, pts i32[])")
	ct := db.CompositeType("poly")
	if ct == nil || len(ct.Fields) != 2 {
		t.Fatalf("poly wrong: %+v", ct)
	}
	if ct.Fields[1].Name != "pts" || ct.Fields[1].Type.Array == nil ||
		ct.Fields[1].Type.Array.ScalarTy() != scalarInt32 {
		t.Errorf("field 1 should be i32[], got %+v", ct.Fields[1].Type)
	}
}

// TestCompositeWithArrayFieldImageRoundtrip: the array field survives the on-disk image round-trip
// (the catalog code-15 field entry + the recursive value codec).
func TestCompositeWithArrayFieldImageRoundtrip(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE poly AS (name text, pts i32[])")
	runComposite(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)")
	runComposite(t, db, "INSERT INTO t VALUES (1, ROW('a', ARRAY[1, 2, 3]))")
	runComposite(t, db, "INSERT INTO t VALUES (2, ROW('b', NULL))")
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	ct := loaded.CompositeType("poly")
	if ct == nil || ct.Fields[1].Type.Array == nil || ct.Fields[1].Type.Array.ScalarTy() != scalarInt32 {
		t.Fatalf("poly array field did not persist: %+v", ct)
	}
	got := queryRendered(t, loaded, "SELECT id, p FROM t ORDER BY id")
	want := [][]string{{"1", `(a,"{1,2,3}")`}, {"2", "(b,)"}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows = %v, want %v", got, want)
	}
}

// TestCompositeWithArrayOfCompositeField: the doubly-nested case (homes addr[]) — the field carries
// element code 14 + name; the value codec nests array-over-composite; it survives the image round-trip.
func TestCompositeWithArrayOfCompositeField(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TYPE person AS (name text, homes addr[])")
	runComposite(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, who person)")
	runComposite(t, db, `INSERT INTO t VALUES (1, ROW('jo', '{"(Main,1)","(Oak,2)"}'))`)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	if got, want := queryRendered(t, loaded, "SELECT (who).homes[1] FROM t WHERE id = 1"), [][]string{{"(Main,1)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("array-of-composite field access: got %v, want %v", got, want)
	}
}

// TestDropTypeBlockedByArrayFieldDependent: DROP TYPE addr is 2BP01 while a composite type has an
// addr[] field; dropping the dependent first frees it.
func TestDropTypeBlockedByArrayFieldDependent(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TYPE person AS (name text, homes addr[])")
	if got := errComposite(t, db, "DROP TYPE addr"); got != "2BP01" {
		t.Errorf("DROP TYPE addr: got %q, want 2BP01", got)
	}
	runComposite(t, db, "DROP TYPE person")
	runComposite(t, db, "DROP TYPE addr")
}

// TestDropTypeBlockedByArrayColumnDependent: DROP TYPE addr is 2BP01 while a table column is addr[].
func TestDropTypeBlockedByArrayColumnDependent(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	runComposite(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runComposite(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])")
	if got := errComposite(t, db, "DROP TYPE addr"); got != "2BP01" {
		t.Errorf("DROP TYPE addr: got %q, want 2BP01", got)
	}
}

// TestArrayFieldTypeModifierIs0A000 / unknown element 42704: the array field's element gates match
// an array column's.
func TestArrayFieldErrors(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	if got := errComposite(t, db, "CREATE TYPE t AS (xs decimal(10,2)[])"); got != "0A000" {
		t.Errorf("array field typmod: got %q, want 0A000", got)
	}
	if got := errComposite(t, db, "CREATE TYPE t2 AS (xs nope[])"); got != "42704" {
		t.Errorf("unknown array element: got %q, want 42704", got)
	}
}
