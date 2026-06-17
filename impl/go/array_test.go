package jed

// Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural int32[] column, the
// ARRAY[…] constructor + the '{…}' literal, the compact value codec (S2), btree-NULL element
// comparison / ORDER BY / DISTINCT (S4), and array_out rendering. Mirrors impl/rust/tests/array.rs.

import (
	"reflect"
	"sort"
	"testing"
)

func runArray(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
}

func errArray(t *testing.T, db *Database, sql string) string {
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

func TestArrayColumnRoundtrip(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])")
	runArray(t, db, "INSERT INTO t VALUES (2, '{40,50}', '{}')")
	got := queryRendered(t, db, "SELECT id, xs, tags FROM t ORDER BY id")
	want := [][]string{
		{"1", "{10,20,30}", "{a,b}"},
		{"2", "{40,50}", "{}"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestArrayImageRoundtrip(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])")
	runArray(t, db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3], '{}')")
	runArray(t, db, "INSERT INTO t VALUES (3, NULL, NULL)")
	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	got := queryRendered(t, loaded, "SELECT id, xs, tags FROM t ORDER BY id")
	want := [][]string{
		{"1", "{10,20,30}", "{a,b}"},
		{"2", "{1,NULL,3}", "{}"},
		{"3", "NULL", "NULL"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestArrayNullLevels(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])")
	runArray(t, db, "INSERT INTO t VALUES (2, NULL)")
	runArray(t, db, "INSERT INTO t VALUES (3, '{}')")
	got := queryRendered(t, db, "SELECT xs FROM t ORDER BY id")
	want := [][]string{{"{1,NULL,3}"}, {"NULL"}, {"{}"}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE xs IS NULL ORDER BY id"); !reflect.DeepEqual(ids, []int64{2}) {
		t.Fatalf("IS NULL: got %v", ids)
	}
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE xs IS NOT NULL ORDER BY id"); !reflect.DeepEqual(ids, []int64{1, 3}) {
		t.Fatalf("IS NOT NULL: got %v", ids)
	}
}

func TestArrayEqualityBtreeSemantics(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])")
	runArray(t, db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3])")
	runArray(t, db, "INSERT INTO t VALUES (3, ARRAY[1, 2])")
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE xs = ARRAY[1,2,3]"); !reflect.DeepEqual(ids, []int64{1}) {
		t.Fatalf("exact: %v", ids)
	}
	// {1,NULL,3} = {1,NULL,3} is TRUE (NULLs mutually equal — not UNKNOWN).
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE xs = ARRAY[1,NULL,3]"); !reflect.DeepEqual(ids, []int64{2}) {
		t.Fatalf("null-eq: %v", ids)
	}
	if ids := queryIDs(t, db, "SELECT id FROM t WHERE xs = ARRAY[1,2]"); !reflect.DeepEqual(ids, []int64{3}) {
		t.Fatalf("shorter: %v", ids)
	}
}

func TestArrayOrderBy(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])")
	runArray(t, db, "INSERT INTO t VALUES (2, ARRAY[1, 2])")
	runArray(t, db, "INSERT INTO t VALUES (3, ARRAY[1, 3])")
	runArray(t, db, "INSERT INTO t VALUES (4, ARRAY[1])")
	got := queryRendered(t, db, "SELECT xs FROM t ORDER BY xs")
	want := [][]string{{"{1}"}, {"{1,2}"}, {"{1,2,3}"}, {"{1,3}"}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestArrayDistinct(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1, 2])")
	runArray(t, db, "INSERT INTO t VALUES (2, ARRAY[1, 2])")
	runArray(t, db, "INSERT INTO t VALUES (3, ARRAY[3])")
	got := queryRendered(t, db, "SELECT DISTINCT xs FROM t")
	flat := make([]string, len(got))
	for i, r := range got {
		flat[i] = r[0]
	}
	sort.Strings(flat)
	if !reflect.DeepEqual(flat, []string{"{1,2}", "{3}"}) {
		t.Fatalf("distinct: %v", flat)
	}
}

func TestArrayOutQuoting(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, tags text[])")
	runArray(t, db, `INSERT INTO t VALUES (1, ARRAY['a,b', '', 'NULL', 'x"y'])`)
	got := queryRendered(t, db, "SELECT tags FROM t")
	want := [][]string{{`{"a,b","","NULL","x\"y"}`}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestArrayElementOverflowIs22003(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int16[])")
	if code := errArray(t, db, "INSERT INTO t VALUES (1, ARRAY[100000])"); code != "22003" {
		t.Fatalf("got %s", code)
	}
}

func TestArrayPrimaryKeyIs0A000(t *testing.T) {
	db := NewDatabase()
	if code := errArray(t, db, "CREATE TABLE t (xs int32[] PRIMARY KEY)"); code != "0A000" {
		t.Fatalf("got %s", code)
	}
}

func TestMalformedArrayLiteralIs22P02(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	if code := errArray(t, db, "INSERT INTO t VALUES (1, '{1,2')"); code != "22P02" {
		t.Fatalf("got %s", code)
	}
}

func TestArrayCrossElementCompareIs42804(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], ts text[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1], ARRAY['a'])")
	if code := errArray(t, db, "SELECT id FROM t WHERE xs = ts"); code != "42804" {
		t.Fatalf("got %s", code)
	}
}

// S3: a[i] is 1-based; the element type is the column's element type (spec/design/array.md §6).
func TestSubscriptIsOneBased(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])")
	for _, c := range []struct {
		sql  string
		want string
	}{
		{"SELECT xs[1] FROM t", "10"},
		{"SELECT xs[3] FROM t", "30"},
		{"SELECT tags[2] FROM t", "b"},
	} {
		if got := queryRendered(t, db, c.sql); !reflect.DeepEqual(got, [][]string{{c.want}}) {
			t.Fatalf("%s: got %v, want [[%s]]", c.sql, got, c.want)
		}
	}
}

// S3: an out-of-bounds subscript (0, negative, or past the end) yields NULL — never an error (PG).
func TestSubscriptOutOfBoundsIsNull(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])")
	for _, sql := range []string{"SELECT xs[0] FROM t", "SELECT xs[4] FROM t", "SELECT xs[-1] FROM t"} {
		if got := queryRendered(t, db, sql); !reflect.DeepEqual(got, [][]string{{"NULL"}}) {
			t.Fatalf("%s: got %v, want [[NULL]]", sql, got)
		}
	}
}

// S3: a NULL subscript, a subscript of a NULL array, and a subscript reading a NULL element all
// yield NULL.
func TestSubscriptNullCases(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])")
	runArray(t, db, "INSERT INTO t VALUES (2, NULL)")
	for _, sql := range []string{
		"SELECT xs[NULL] FROM t WHERE id = 1", // NULL index
		"SELECT xs[1] FROM t WHERE id = 2",    // NULL array
		"SELECT xs[2] FROM t WHERE id = 1",    // NULL element
	} {
		if got := queryRendered(t, db, sql); !reflect.DeepEqual(got, [][]string{{"NULL"}}) {
			t.Fatalf("%s: got %v, want [[NULL]]", sql, got)
		}
	}
}

// S3: subscripting a non-array base is 42804 at resolve.
func TestSubscriptNonArrayIs42804(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)")
	runArray(t, db, "INSERT INTO t VALUES (1, 5)")
	if code := errArray(t, db, "SELECT n[1] FROM t"); code != "42804" {
		t.Fatalf("got %s", code)
	}
}

// S3: the index can be an arbitrary integer expression, and an ARRAY[…] constructor subscripts
// directly ((ARRAY[…])[i]).
func TestSubscriptExpressionIndexAndConstructorBase(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])")
	if got := queryRendered(t, db, "SELECT xs[1 + 1] FROM t"); !reflect.DeepEqual(got, [][]string{{"20"}}) {
		t.Fatalf("expression index: got %v", got)
	}
	if got := queryRendered(t, db, "SELECT (ARRAY[100, 200, 300])[3] FROM t"); !reflect.DeepEqual(got, [][]string{{"300"}}) {
		t.Fatalf("constructor base: got %v", got)
	}
}

// --- AC1: array-of-composite element types (spec/design/array.md §12) -----------------------------

// TestArrayOfCompositeRoundtripAndAccess: a composite type is a first-class array element type.
// Construct via the '{…}'::addr[] literal (array_in → record_in per element) AND via the
// ARRAY[ROW(…)] constructor with the column's composite element context (the jed extension PG needs
// ::addr casts for — covered here, not in the PG-oracle corpus). array_out nests the two quoting
// layers; subscript yields the composite, field access reads into it, a slice yields addr[].
func TestArrayOfCompositeRoundtripAndAccess(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, items addr[])")
	runArray(t, db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`)
	runArray(t, db, "INSERT INTO t VALUES (2, ARRAY[ROW('Other, Ln', 12)])")
	runArray(t, db, `INSERT INTO t VALUES (3, '{"(Main,)",NULL}')`)
	got := queryRendered(t, db, "SELECT id, items FROM t ORDER BY id")
	want := [][]string{
		{"1", `{"(Main,90210)","(Side,5)"}`},
		{"2", `{"(\"Other, Ln\",12)"}`},
		{"3", `{"(Main,)",NULL}`},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("array-of-composite render:\n got %v\nwant %v", got, want)
	}
	if got := queryRendered(t, db, "SELECT items[1] FROM t WHERE id = 1"); !reflect.DeepEqual(got, [][]string{{"(Main,90210)"}}) {
		t.Fatalf("subscript: got %v", got)
	}
	if got := queryRendered(t, db, "SELECT (items[2]).street FROM t WHERE id = 1"); !reflect.DeepEqual(got, [][]string{{"Side"}}) {
		t.Fatalf("field access: got %v", got)
	}
	if got := queryRendered(t, db, "SELECT items[1:1] FROM t WHERE id = 1"); !reflect.DeepEqual(got, [][]string{{`{"(Main,90210)"}`}}) {
		t.Fatalf("slice: got %v", got)
	}
}

// TestArrayOfCompositeImageRoundtrip: an addr[] column survives the on-disk image round-trip
// (the recursive value codec — composite element bodies inside the array body; complements the
// cross-core golden).
func TestArrayOfCompositeImageRoundtrip(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, items addr[])")
	runArray(t, db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`)
	runArray(t, db, `INSERT INTO t VALUES (2, '{"(Main,)",NULL}')`)
	runArray(t, db, "INSERT INTO t VALUES (3, NULL)")
	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	got := queryRendered(t, loaded, "SELECT id, items FROM t ORDER BY id")
	want := [][]string{
		{"1", `{"(Main,90210)","(Side,5)"}`},
		{"2", `{"(Main,)",NULL}`},
		{"3", "NULL"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("image round-trip:\n got %v\nwant %v", got, want)
	}
}

// TestArrayOfCompositeNullFieldOrderingOperators: the load-bearing comparison fix — a composite
// element's per-element compare routes through the composite TOTAL ORDER (NULLs-last, definite),
// NOT the 3VL, so the ordering operators < <= > >= are consistent for arrays whose composite
// elements have NULL fields (spec/design/array.md §5, oracle-pinned).
func TestArrayOfCompositeNullFieldOrderingOperators(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	got := queryRendered(t, db, `SELECT '{"(1,)"}'::addr[] <= '{"(1,)"}'::addr[], `+
		`'{"(1,)"}'::addr[] >= '{"(1,)"}'::addr[], `+
		`'{"(1,)"}'::addr[] < '{"(1,)"}'::addr[]`)
	if !reflect.DeepEqual(got, [][]string{{"true", "true", "false"}}) {
		t.Fatalf("equal-with-NULL-field ordering: got %v", got)
	}
	got = queryRendered(t, db, `SELECT '{"(a,)"}'::addr[] > '{"(a,1)"}'::addr[], `+
		`'{"(a,1)"}'::addr[] < '{"(a,)"}'::addr[]`)
	if !reflect.DeepEqual(got, [][]string{{"true", "true"}}) {
		t.Fatalf("NULL field sorts last: got %v", got)
	}
}

// TestArrayOfCompositePrimaryKeyIs0A000: a composite-element array is still never keyable (§8) —
// the new element type does not relax the key gate.
func TestArrayOfCompositePrimaryKeyIs0A000(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TYPE addr AS (street text, zip int32)")
	if code := errArray(t, db, "CREATE TABLE t (items addr[] PRIMARY KEY)"); code != "0A000" {
		t.Fatalf("composite-array PK: got %s, want 0A000", code)
	}
}
