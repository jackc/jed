package jed

// Composite PRIMARY KEY — the table-level `PRIMARY KEY (a, b, …)` constraint
// (spec/design/constraints.md §3, grammar.md §28). Covers what the corpus suite
// (ddl/composite_pk.test) cannot: catalog flag introspection, the stored key order
// (the concatenated encoding of encoding.md §2.3), and the on-disk round-trip (a
// composite-PK table reloads as a KEYED table, not a rowid table). Mirrors
// impl/rust/tests/composite_pk.rs and impl/ts/tests/composite_pk.test.ts.

import (
	"slices"
	"testing"
)

func compositeErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

// The constraint flags every member primary_key + NOT NULL, and the stored order is the
// tuple's lexicographic order (the concatenated key — first component, then the second
// breaking its ties), independent of insertion order.
func TestCompositeKeyOrdersByTuple(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))")
	tab, _ := db.Table("t")
	if got := tab.PKIndices(); !slices.Equal(got, []int{0, 1}) {
		t.Fatalf("PKIndices = %v, want [0 1]", got)
	}
	if !tab.Columns[0].PrimaryKey || !tab.Columns[0].NotNull ||
		!tab.Columns[1].PrimaryKey || !tab.Columns[1].NotNull {
		t.Fatal("both members must be primary_key + NOT NULL")
	}
	if tab.Columns[2].PrimaryKey {
		t.Fatal("non-member column must not be flagged")
	}
	// Single-column pushdown accessor must NOT see a composite key.
	if got := tab.PrimaryKeyIndex(); got != -1 {
		t.Fatalf("PrimaryKeyIndex = %d, want -1 for a composite key", got)
	}

	// Insert out of tuple order; include a negative first component (sign-flip) and ties
	// on the first component broken by the second.
	for _, stmt := range []string{
		"INSERT INTO t VALUES (2, 1, 50)",
		"INSERT INTO t VALUES (1, 2, 30)",
		"INSERT INTO t VALUES (-1, 9, 10)",
		"INSERT INTO t VALUES (1, 1, 20)",
		"INSERT INTO t VALUES (2, 0, 40)",
	} {
		if _, err := queryOutcome(db, stmt, nil); err != nil {
			t.Fatalf("%q: %v", stmt, err)
		}
	}
	want := [][2]int64{{-1, 9}, {1, 1}, {1, 2}, {2, 0}, {2, 1}}
	rows := db.RowsInKeyOrder("t")
	if len(rows) != len(want) {
		t.Fatalf("got %d rows, want %d", len(rows), len(want))
	}
	for i, r := range rows {
		if r[0].Int != want[i][0] || r[1].Int != want[i][1] {
			t.Fatalf("row %d = (%d,%d), want (%d,%d)", i, r[0].Int, r[1].Int, want[i][0], want[i][1])
		}
	}
}

// Uniqueness is over the WHOLE tuple: a shared prefix is fine, a duplicate tuple traps
// 23505 — both against the store and within one INSERT's batch (two-phase, nothing stored).
func TestCompositeUniquenessIsTheWholeTuple(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, b))",
		"INSERT INTO t VALUES (1, 1)",
	)
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (1, 2)", nil); err != nil {
		t.Fatalf("shared prefix must be a distinct row: %v", err)
	}
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (1, 1)"); code != "23505" {
		t.Fatalf("duplicate tuple: got %s, want 23505", code)
	}
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (5, 5), (5, 5)"); code != "23505" {
		t.Fatalf("in-batch duplicate: got %s, want 23505", code)
	}
	// The failed batch stored nothing (all-or-nothing).
	if n := len(db.RowsInKeyOrder("t")); n != 2 {
		t.Fatalf("got %d rows, want 2", n)
	}
}

// DDL errors mirror PostgreSQL (oracle-probed): unknown member 42703, repeated member
// 42701, more than one primary key across both forms 42P16 — plus the jed narrowings
// (0A000): out-of-declaration-order list, non-keyable member type.
func TestCompositeDDLErrorsMatchPostgresAndNarrowings(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, "CREATE TYPE addr AS (street text, zip i32)", nil); err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		sql, want string
	}{
		{"CREATE TABLE t (a i32, PRIMARY KEY (a, nosuch))", "42703"},
		{"CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, a))", "42701"},
		{"CREATE TABLE t (a i32 PRIMARY KEY, b i32, PRIMARY KEY (b))", "42P16"},
		{"CREATE TABLE t (a i32, b i32, PRIMARY KEY (a), PRIMARY KEY (b))", "42P16"},
		// 42P16 fires BEFORE the second constraint's members resolve (PostgreSQL's order).
		{"CREATE TABLE t (a i32 PRIMARY KEY, PRIMARY KEY (nosuch))", "42P16"},
		// Narrowing: every member must be key-encodable. f64 IS now keyable (encoding.md §2.8);
		// the recursive composite container is NOT (composite.md §6), so a composite member is 0A000.
		{"CREATE TABLE t (a i32, s addr, PRIMARY KEY (a, s))", "0A000"},
	}
	for _, c := range cases {
		if code := compositeErrCode(t, db, c.sql); code != c.want {
			t.Fatalf("%q: got %s, want %s", c.sql, code, c.want)
		}
	}
	// f64 IS now a key-encodable PK member (the float-order-preserving key lifted the narrowing,
	// encoding.md §2.8): a composite PK with a float member succeeds.
	if _, err := queryOutcome(db, "CREATE TABLE fpk (a i32, s f64, PRIMARY KEY (a, s))", nil); err != nil {
		t.Fatalf("composite PK with f64 member: %v", err)
	}
	// The list order is the KEY order — it may differ from declaration order (the original
	// 0A000 narrowing was lifted by the v5 catalog reshape, constraints.md §3): the table
	// keys by (b, a), so the stored scan order is b-major.
	if _, err := queryOutcome(db, "CREATE TABLE rev (a i32, b i32, PRIMARY KEY (b, a))", nil); err != nil {
		t.Fatalf("out-of-declaration-order PK: %v", err)
	}
	if revTab, _ := db.Table("rev"); !slices.Equal(revTab.PKIndices(), []int{1, 0}) {
		t.Fatalf("PKIndices = %v, want [1 0]", func() []int { rt, _ := db.Table("rev"); return rt.PKIndices() }())
	}
	if _, err := queryOutcome(db, "INSERT INTO rev VALUES (1, 20), (2, 10), (3, 15)", nil); err != nil {
		t.Fatalf("insert: %v", err)
	}
	var bs []int64
	for _, row := range db.RowsInKeyOrder("rev") {
		bs = append(bs, row[1].Int)
	}
	if !slices.Equal(bs, []int64{10, 15, 20}) {
		t.Fatalf("stored order = %v, want the (b, a) tuple order [10 15 20]", bs)
	}

	// A single-column table constraint is the column-level form's equivalent.
	if _, err := queryOutcome(db, "CREATE TABLE ok (a i32, PRIMARY KEY (a))", nil); err != nil {
		t.Fatalf("single-column table constraint: %v", err)
	}
	tab, _ := db.Table("ok")
	if tab.PrimaryKeyIndex() != 0 || !tab.Columns[0].NotNull {
		t.Fatal("single-member constraint must behave as the column-level form")
	}
}

// Every member is a key column: NULL into any member traps 23502. Assigning a member now
// re-keys the row (§11 step 6 — the narrowing is lifted) instead of trapping 0A000; a
// non-member updates in place.
func TestCompositeMembersNotNullAndRekey(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
		"INSERT INTO t VALUES (1, 1, 10)",
	)
	if code := compositeErrCode(t, db, "INSERT INTO t VALUES (1, NULL, 5)"); code != "23502" {
		t.Fatalf("NULL member: got %s, want 23502", code)
	}
	if code := compositeErrCode(t, db, "INSERT INTO t (a, v) VALUES (2, 5)"); code != "23502" {
		t.Fatalf("omitted member: got %s, want 23502", code)
	}
	// Assigning a key member re-keys the row: (1,1) → (9,1) → (9,9); a non-member is in place.
	for _, stmt := range []string{"UPDATE t SET a = 9", "UPDATE t SET b = 9", "UPDATE t SET v = 11"} {
		if _, err := queryOutcome(db, stmt, nil); err != nil {
			t.Fatalf("%q: %v", stmt, err)
		}
	}
	rows := db.RowsInKeyOrder("t")
	if len(rows) != 1 || rows[0][0].Int != 9 || rows[0][1].Int != 9 || rows[0][2].Int != 11 {
		t.Fatalf("after re-key: got %v, want one row (9,9,11)", rows)
	}
}

// Mixed fixed-width components (uuid first, i32 second) concatenate per encoding.md
// §2.3 and iterate in tuple order — uuid bytes compare first, the int breaks ties.
func TestCompositeMixedUuidIntComponentsOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (u uuid, n i32, PRIMARY KEY (u, n))")
	for _, stmt := range []string{
		"INSERT INTO t VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff', -5)",
		"INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', 7)",
		"INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', -2)",
	} {
		if _, err := queryOutcome(db, stmt, nil); err != nil {
			t.Fatalf("%q: %v", stmt, err)
		}
	}
	rows := db.RowsInKeyOrder("t")
	want := []int64{-2, 7, -5}
	for i, r := range rows {
		if r[1].Int != want[i] {
			t.Fatalf("row %d second component = %d, want %d", i, r[1].Int, want[i])
		}
	}
}

// The on-disk round-trip: a composite-PK table reloads as a KEYED table (both flag bits
// survive in the catalog), key order is preserved, and a duplicate tuple still traps
// 23505 after the reload. Guards the format.go hasPK seam — a composite-PK table must
// not be mistaken for a rowid table on load.
func TestCompositeRoundTripsThroughTheOnDiskImage(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
		"INSERT INTO t VALUES (2, 1, 40), (1, 2, 20), (1, 1, 10)",
	)
	image, err := db.ToImage(256, 1)
	if err != nil {
		t.Fatalf("ToImage: %v", err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatalf("LoadEngine: %v", err)
	}

	tab, _ := loaded.Table("t")
	if got := tab.PKIndices(); !slices.Equal(got, []int{0, 1}) {
		t.Fatalf("PKIndices after reload = %v, want [0 1]", got)
	}
	if !tab.Columns[0].NotNull || !tab.Columns[1].NotNull {
		t.Fatal("members must reload NOT NULL")
	}

	want := [][2]int64{{1, 1}, {1, 2}, {2, 1}}
	rows := loaded.RowsInKeyOrder("t")
	if len(rows) != len(want) {
		t.Fatalf("got %d rows, want %d", len(rows), len(want))
	}
	for i, r := range rows {
		if r[0].Int != want[i][0] || r[1].Int != want[i][1] {
			t.Fatalf("row %d = (%d,%d), want (%d,%d)", i, r[0].Int, r[1].Int, want[i][0], want[i][1])
		}
	}

	if code := compositeErrCode(t, loaded, "INSERT INTO t VALUES (1, 2, 99)"); code != "23505" {
		t.Fatalf("duplicate after reload: got %s, want 23505", code)
	}
	if _, err := queryOutcome(loaded, "INSERT INTO t VALUES (2, 2, 50)", nil); err != nil {
		t.Fatalf("fresh insert after reload: %v", err)
	}
	if n := len(loaded.RowsInKeyOrder("t")); n != 4 {
		t.Fatalf("got %d rows, want 4", n)
	}
}
