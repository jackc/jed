package jed

// Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
// array-elements-terminated rule). The 1-D PRIMARY KEY surface agrees with PostgreSQL and is
// oracle-checked in types/array_key.test; this file covers only what that corpus cannot:
//   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent array_cmp order
//       deliberately differs from PostgreSQL's single-column ORDER BY (an abbreviated-key artifact);
//   (b) the keyable-element gate — a float-element array PRIMARY KEY IS keyable (the §2.8 lift —
//       f64[]/f32[]); a composite-element array key is still rejected 0A000 (composite not yet keyable).
// Mirrors impl/rust/tests/array_key.rs.

import "testing"

// TestMultidimAndLowerBoundKeyOrder pins jed's array_cmp PK order for multidim / custom-lower-bound
// values, which diverges from PG's ORDER BY (so it is not oracle-checked).
func TestMultidimAndLowerBoundKeyOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE m (k i32[] PRIMARY KEY)")
	for _, v := range []string{"{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"} {
		if _, err := db.Execute("INSERT INTO m VALUES ('"+v+"')", nil); err != nil {
			t.Fatalf("insert %q: %v", v, err)
		}
	}
	out, err := db.Execute("SELECT k FROM m ORDER BY k", nil)
	if err != nil {
		t.Fatal(err)
	}
	got := make([]string, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Render()
	}
	want := []string{"{1,2,3}", "[2:4]={1,2,3}", "{1,2,3,4}", "{{1,2},{3,4}}"}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("array PK order: got %v, want %v", got, want)
		}
	}
}

// firstColRows runs a SELECT and returns the rendered first-column values.
func firstColRows(t *testing.T, db dbHandle, sql string) []string {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
	got := make([]string, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Render()
	}
	return got
}

// TestFloatElementArrayKeyIsKeyable: a f64[] PRIMARY KEY is now allowed (the §2.8 float-key lift) and
// the store iterates in array_cmp order over the float total order (-0=+0, NaN largest, shorter-prefix
// first). The '{…}' literal coerces the specials (NaN/Infinity) without an INSERT ... SELECT.
func TestFloatElementArrayKeyIsKeyable(t *testing.T) {
	db := dbWith(t, "CREATE TABLE m (k f64[] PRIMARY KEY)")
	for _, v := range []string{"{1.5,2.5}", "{1.5}", "{-Infinity}", "{NaN}", "{1.5,2.0}"} {
		if _, err := db.Execute("INSERT INTO m VALUES ('"+v+"')", nil); err != nil {
			t.Fatalf("insert %q: %v", v, err)
		}
	}
	got := firstColRows(t, db, "SELECT k FROM m ORDER BY k")
	want := []string{"{-Infinity}", "{1.5}", "{1.5,2}", "{1.5,2.5}", "{NaN}"}
	for i := range want {
		if i >= len(got) || got[i] != want[i] {
			t.Fatalf("float[] PK order: got %v, want %v", got, want)
		}
	}
}

// TestFloatElementArrayMultidimKeyOrder pins the multidim / lower-bound float-element array key order
// (jed's array_cmp, NOT PG's ORDER BY — the abbreviated-key artifact §2.14), the float analogue of
// TestMultidimAndLowerBoundKeyOrder.
func TestFloatElementArrayMultidimKeyOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE m (k f64[] PRIMARY KEY)")
	for _, v := range []string{"{1.5,2.5,3.5,4.5}", "{{1.5,2.5},{3.5,4.5}}", "{1.5,2.5,3.5}", "[2:4]={1.5,2.5,3.5}"} {
		if _, err := db.Execute("INSERT INTO m VALUES ('"+v+"')", nil); err != nil {
			t.Fatalf("insert %q: %v", v, err)
		}
	}
	got := firstColRows(t, db, "SELECT k FROM m ORDER BY k")
	want := []string{"{1.5,2.5,3.5}", "[2:4]={1.5,2.5,3.5}", "{1.5,2.5,3.5,4.5}", "{{1.5,2.5},{3.5,4.5}}"}
	for i := range want {
		if i >= len(got) || got[i] != want[i] {
			t.Fatalf("float[] multidim PK order: got %v, want %v", got, want)
		}
	}
}

// TestCompositeElementArrayKeysRejected: a composite-element array key is still 0A000 (composite not
// yet keyable), while float-element arrays are accepted everywhere a key is taken.
func TestCompositeElementArrayKeysRejected(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if _, err := db.Execute("CREATE TYPE addr AS (street text, zip i32)", nil); err != nil {
		t.Fatal(err)
	}
	if got := castErrCode(t, db, "CREATE TABLE bad (k addr[] PRIMARY KEY)"); got != "0A000" {
		t.Fatalf("addr[] PK: want 0A000, got %s", got)
	}
	if _, err := db.Execute("CREATE TABLE ok (id i32 PRIMARY KEY, k f32[] UNIQUE)", nil); err != nil {
		t.Fatalf("f32[] UNIQUE: %v", err)
	}
	if _, err := db.Execute("CREATE TABLE ok2 (id i32 PRIMARY KEY, k f64[])", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Execute("CREATE INDEX ix ON ok2 (k)", nil); err != nil {
		t.Fatalf("f64[] index: %v", err)
	}
}
