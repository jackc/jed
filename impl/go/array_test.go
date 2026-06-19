package jed

// Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural i32[] column, the
// ARRAY[…] constructor + the '{…}' literal, the compact value codec (S2), btree-NULL element
// comparison / ORDER BY / DISTINCT (S4), and array_out rendering. Mirrors impl/rust/tests/array.rs.

import (
	"reflect"
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

func TestArrayImageRoundtrip(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])")
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

// --- AC1: array-of-composite element types (spec/design/array.md §12) -----------------------------

// TestArrayOfCompositeRoundtripAndAccess: a composite type is a first-class array element type.
// Construct via the '{…}'::addr[] literal (array_in → record_in per element) AND via the
// ARRAY[ROW(…)] constructor with the column's composite element context (the jed extension PG needs
// ::addr casts for — covered here, not in the PG-oracle corpus). array_out nests the two quoting
// layers; subscript yields the composite, field access reads into it, a slice yields addr[].
func TestArrayOfCompositeRoundtripAndAccess(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runArray(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])")
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
	runArray(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	runArray(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])")
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
	runArray(t, db, "CREATE TYPE addr AS (street text, zip i32)")
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
	runArray(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	if code := errArray(t, db, "CREATE TABLE t (items addr[] PRIMARY KEY)"); code != "0A000" {
		t.Fatalf("composite-array PK: got %s, want 0A000", code)
	}
}
