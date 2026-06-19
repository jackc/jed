package jed

// Range storage (spec/design/ranges.md, R2) — the divergences + introspection the oracle corpus
// cannot express (CLAUDE.md §10): the deliberate 0A000 narrowings PostgreSQL does NOT share (a range
// PRIMARY KEY / DEFAULT / index — PG allows them via its btree/GiST opclasses), the jed-canonical
// i32range spelling (PG reports int4range), INSERT…SELECT deferral, and the whole-image store/load
// round-trip of a range column (the byte layout is pinned cross-core by range_table.jed; this is the
// behavioral check). The agreeing behavior — render, canonicalization, IS NULL, 22000/22P02/22003/
// 42704 — lives in types/range.test (oracle-clean), not here. Mirrors impl/rust/tests/range_storage.rs.

import (
	"reflect"
	"strings"
	"testing"
)

// errRange executes sql expecting an error and returns its SQLSTATE code.
func errRange(t *testing.T, db *Database, sql string) string {
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

// TestRangeImageRoundtrip: a range column survives a whole-image serialize + reload (ToImage →
// LoadDatabase), exercising encodeRangeBody / readRangeBody (the empty range, infinite bounds, a NULL
// range, the canonical [) storage). The on-disk byte layout is pinned cross-core by range_table.jed;
// this is the behavioral round-trip.
func TestRangeImageRoundtrip(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')")
	run(t, db, "INSERT INTO t VALUES (2, '[1,5]', NULL)") // canonical [1,6)
	run(t, db, "INSERT INTO t VALUES (3, 'empty', '(,100)')")
	run(t, db, "INSERT INTO t VALUES (4, '(,)', '(5,)')") // canonical [6,)
	run(t, db, "INSERT INTO t VALUES (5, NULL, '[1,1]')") // canonical [1,2)

	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load image: %v", err)
	}
	got := queryRendered(t, loaded, "SELECT id, r, br FROM t ORDER BY id")
	want := [][]string{
		{"1", "[1,5)", "[10,20)"},
		{"2", "[1,6)", "NULL"},
		{"3", "empty", "(,100)"},
		{"4", "(,)", "[6,)"},
		{"5", "NULL", "[1,2)"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("rows differ\n  got:  %v\n  want: %v", got, want)
	}
}

// TestRangeCanonicalNameAndAliases: the jed-canonical name is i32range (PG reports int4range), and
// int4range/int8range are accepted as aliases (the i/f-prefix rename — CLAUDE.md §4). The PG alias
// declares a column whose stored value renders identically to the canonical spelling, and the
// canonical name (not the PG int4range) appears in a jed message.
func TestRangeCanonicalNameAndAliases(t *testing.T) {
	// The PG alias is accepted on the column; the value renders the same as the canonical spelling.
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r int4range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)')")
	got := queryRendered(t, db, "SELECT r FROM t")
	if want := [][]string{{"[1,5)"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("rows differ\n  got:  %v\n  want: %v", got, want)
	}

	// The canonical name appears in the 0A000 PK-narrowing message (CanonicalName), even though the
	// column was declared with the PG alias int4range.
	db2 := NewDatabase()
	_, err := Execute(db2, "CREATE TABLE u (r int4range PRIMARY KEY)")
	if err == nil {
		t.Fatal("a range primary key should be rejected")
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an *EngineError, got %T", err)
	}
	if !strings.Contains(ee.Message, "i32range") {
		t.Errorf("message should name i32range: %q", ee.Message)
	}
}

// TestRangeNarrowingsAre0A000: the staged 0A000 narrowings PostgreSQL does NOT share: a range PRIMARY
// KEY, a range DEFAULT, a range index, and INSERT…SELECT into a range column (PG accepts a range key
// via its default btree opclass and a range DEFAULT outright — spec/design/ranges.md §8). These are
// jed-stricter, so they cannot live in the oracle-clean corpus.
func TestRangeNarrowingsAre0A000(t *testing.T) {
	db := NewDatabase()
	if got := errRange(t, db, "CREATE TABLE a (r i32range PRIMARY KEY)"); got != "0A000" {
		t.Errorf("range PRIMARY KEY: got %s, want 0A000", got)
	}
	if got := errRange(t, db, "CREATE TABLE b (id i32 PRIMARY KEY, r i32range DEFAULT '[1,5)')"); got != "0A000" {
		t.Errorf("range DEFAULT: got %s, want 0A000", got)
	}
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	// A range index needs a GiST opclass jed does not ship (§8/§10).
	if got := errRange(t, db, "CREATE INDEX ri ON t (r)"); got != "0A000" {
		t.Errorf("range index: got %s, want 0A000", got)
	}
	// INSERT … SELECT into a range column is deferred (the VALUES + literal path is the input).
	run(t, db, "CREATE TABLE src (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "INSERT INTO src VALUES (1, '[1,5)')")
	if got := errRange(t, db, "INSERT INTO t SELECT id, r FROM src"); got != "0A000" {
		t.Errorf("INSERT ... SELECT into range column: got %s, want 0A000", got)
	}
}

// TestRangeCompositeFieldIs0A000: a range-typed composite field is deferred (0A000) — only range
// *columns* are storable this slice. The type name IS known, so it is 0A000, not the 42704 an unknown
// type would give.
func TestRangeCompositeFieldIs0A000(t *testing.T) {
	db := NewDatabase()
	if got := errRange(t, db, "CREATE TYPE rec AS (lo i32, span i32range)"); got != "0A000" {
		t.Errorf("range composite field: got %s, want 0A000", got)
	}
}
