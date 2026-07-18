package jed

// Composite TYPE as a key — a column whose type is a `CREATE TYPE … AS (…)` row type used as a
// PRIMARY KEY / ordered secondary index / UNIQUE column (the third container key,
// `composite-field-slots`, spec/design/encoding.md §2.15 / composite.md §6). Distinct from the
// multi-column composite PRIMARY KEY in composite_pk_test.go (a flat tuple of scalar columns).
// Covers what the corpus cannot: the stored key ORDER (the recursive per-field encoding), catalog
// introspection, the on-disk round-trip, and the array-of-composite 0A000 narrowing. Mirrors
// impl/rust/tests/composite_key.rs and impl/ts/tests/composite_key.test.ts.

import (
	"slices"
	"testing"
)

func idsInKeyOrder(db *Session, table string) []int64 {
	var ids []int64
	for _, r := range db.RowsInKeyOrder(table) {
		ids = append(ids, r[0].Int)
	}
	return ids
}

// A composite-typed column is a valid sole PRIMARY KEY, and rows iterate in the composite sort key's
// order — lexicographic over fields (text then the tie-breaking i32), reproducing the in-memory
// comparator (§5) under the §2.15 memcmp key.
func TestCompositePKOrdersByFieldLexicographic(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TYPE addr AS (street text, zip i32)",
		"CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
		"INSERT INTO t VALUES (1, ROW('Main', 90210))",
		"INSERT INTO t VALUES (2, ROW('Elm', 100))",
		"INSERT INTO t VALUES (3, ROW('Main', 5))",
		"INSERT INTO t VALUES (4, ROW('', -1))",
	)
	// '' < 'Elm' < 'Main'; within 'Main', zip 5 < 90210  => ids 4, 2, 3, 1
	if got := idsInKeyOrder(db, "t"); !slices.Equal(got, []int64{4, 2, 3, 1}) {
		t.Fatalf("composite key order = %v, want [4 2 3 1]", got)
	}
	tab, _ := db.Table("t")
	if got := tab.PKIndices(); !slices.Equal(got, []int{1}) {
		t.Fatalf("PKIndices = %v, want [1]", got)
	}
	if !tab.Columns[1].NotNull {
		t.Fatal("composite PK column must be NOT NULL")
	}
}

// Uniqueness is over the whole composite value: a duplicate composite traps 23505, a value that
// differs in ANY field is distinct.
func TestCompositePKUniquenessIsWholeValue(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TYPE addr AS (street text, zip i32)",
		"CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
		"INSERT INTO t VALUES (1, ROW('Main', 5))",
	)
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (2, ROW('Main', 6))", nil); err != nil {
		t.Fatalf("distinct zip: %v", err)
	}
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (9, ROW('Main', 5))"); code != "23505" {
		t.Fatalf("duplicate composite: got %s, want 23505", code)
	}
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (7, ROW('X', 1)), (8, ROW('X', 1))"); code != "23505" {
		t.Fatalf("in-batch duplicate: got %s, want 23505", code)
	}
	if n := len(db.RowsInKeyOrder("t")); n != 2 {
		t.Fatalf("got %d rows, want 2 (batch all-or-nothing)", n)
	}
}

// A composite UNIQUE constraint recurses through a NESTED composite field.
func TestCompositeUniqueAndNested(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TYPE addr AS (street text, zip i32)",
		"CREATE TYPE line AS (a addr, b addr)",
		"CREATE TABLE t (id i32, seg line, UNIQUE (seg))",
		"INSERT INTO t VALUES (1, ROW(ROW('Main',1), ROW('Elm',2)))",
	)
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (4, ROW(ROW('Main',1), ROW('Elm',2)))"); code != "23505" {
		t.Fatalf("duplicate nested composite: got %s, want 23505", code)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (5, ROW(ROW('Main',1), ROW('Elm',3)))", nil); err != nil {
		t.Fatalf("distinct deeply-nested field: %v", err)
	}
	if n := len(db.RowsInKeyOrder("t")); n != 2 {
		t.Fatalf("got %d rows, want 2", n)
	}
}

// A secondary index over a composite column supports maintenance (INSERT/DELETE) and the composite
// value round-trips through the on-disk image.
func TestCompositeSecondaryIndexAndImageRoundtrip(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TYPE addr AS (street text, zip i32)",
		"CREATE TABLE t (id i32 PRIMARY KEY, home addr)",
		"CREATE INDEX t_home ON t (home)",
		"INSERT INTO t VALUES (1, ROW('Main', 90210))",
		"INSERT INTO t VALUES (2, ROW('Elm', 100))",
		"INSERT INTO t VALUES (3, ROW('Main', 5))",
	)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("ToImage: %v", err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatalf("LoadEngine: %v", err)
	}
	if _, err := queryOutcome(loaded, "DELETE FROM t WHERE id = 2", nil); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if _, err := queryOutcome(loaded, "INSERT INTO t VALUES (4, ROW('Elm', 100))", nil); err != nil {
		t.Fatalf("reinsert (index maintenance): %v", err)
	}
	if n := len(loaded.RowsInKeyOrder("t")); n != 3 {
		t.Fatalf("got %d rows, want 3", n)
	}
	tab, _ := loaded.Table("t")
	found := false
	for _, ix := range tab.Indexes {
		if ix.Name == "t_home" {
			found = true
		}
	}
	if !found {
		t.Fatal("index t_home must survive the reload")
	}
}

// A composite transitively containing an array-of-composite field is NOT keyable (the array key
// admits only scalar elements, §2.14) — the lone remaining 0A000 key case. A composite with a
// scalar-array field IS keyable.
func TestArrayOfCompositeFieldNotKeyable(t *testing.T) {
	t.Parallel()
	db := dbWith(
		t,
		"CREATE TYPE addr AS (street text, zip i32)",
		"CREATE TYPE tags AS (name text, nums i32[])",
		"CREATE TYPE poly AS (name text, spots addr[])",
	)
	if _, err := queryOutcome(db, "CREATE TABLE ok (id i32, t tags, PRIMARY KEY (t))", nil); err != nil {
		t.Fatalf("scalar-array field composite PK: %v", err)
	}
	if code := compositeErrCode(t, db, "CREATE TABLE t (id i32, p poly, PRIMARY KEY (p))"); code != "0A000" {
		t.Fatalf("array-of-composite PK: got %s, want 0A000", code)
	}
	if code := compositeErrCode(t, db, "CREATE TABLE t (id i32, p poly, UNIQUE (p))"); code != "0A000" {
		t.Fatalf("array-of-composite UNIQUE: got %s, want 0A000", code)
	}
	if _, err := queryOutcome(db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)", nil); err != nil {
		t.Fatalf("array-of-composite column: %v", err)
	}
	if code := compositeErrCode(t, db, "CREATE INDEX t_p ON t (p)"); code != "0A000" {
		t.Fatalf("array-of-composite INDEX: got %s, want 0A000", code)
	}
}
