package jed

// Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
// array-elements-terminated rule). The 1-D PRIMARY KEY surface agrees with PostgreSQL and is
// oracle-checked in types/array_key.test; this file covers only what that corpus cannot:
//   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent array_cmp order
//       deliberately differs from PostgreSQL's single-column ORDER BY (an abbreviated-key artifact);
//   (b) the keyable-element gate — a float-element or composite-element array PRIMARY KEY is rejected
//       0A000, where PostgreSQL allows it.
// Mirrors impl/rust/tests/array_key.rs.

import "testing"

// TestMultidimAndLowerBoundKeyOrder pins jed's array_cmp PK order for multidim / custom-lower-bound
// values, which diverges from PG's ORDER BY (so it is not oracle-checked).
func TestMultidimAndLowerBoundKeyOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE m (k i32[] PRIMARY KEY)")
	for _, v := range []string{"{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"} {
		if _, err := Execute(db, "INSERT INTO m VALUES ('"+v+"')"); err != nil {
			t.Fatalf("insert %q: %v", v, err)
		}
	}
	out, err := Execute(db, "SELECT k FROM m ORDER BY k")
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

// TestNonKeyableElementArrayKeysRejected: a float / composite element array is NOT keyable (0A000),
// where PostgreSQL allows it.
func TestNonKeyableElementArrayKeysRejected(t *testing.T) {
	db := NewDatabase()
	if got := castErrCode(t, db, "CREATE TABLE bad (k f64[] PRIMARY KEY)"); got != "0A000" {
		t.Fatalf("f64[] PK: want 0A000, got %s", got)
	}
	if _, err := Execute(db, "CREATE TYPE addr AS (street text, zip i32)"); err != nil {
		t.Fatal(err)
	}
	if got := castErrCode(t, db, "CREATE TABLE bad2 (k addr[] PRIMARY KEY)"); got != "0A000" {
		t.Fatalf("addr[] PK: want 0A000, got %s", got)
	}
	if got := castErrCode(t, db, "CREATE TABLE bad3 (id i32 PRIMARY KEY, k f64[] UNIQUE)"); got != "0A000" {
		t.Fatalf("f64[] UNIQUE: want 0A000, got %s", got)
	}
	if _, err := Execute(db, "CREATE TABLE ok (id i32 PRIMARY KEY, k f64[])"); err != nil {
		t.Fatal(err)
	}
	if got := castErrCode(t, db, "CREATE INDEX ix ON ok (k)"); got != "0A000" {
		t.Fatalf("f64[] index: want 0A000, got %s", got)
	}
}
