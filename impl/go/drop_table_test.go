package jed

// DROP TABLE — remove a table (its definition + all its rows) from the catalog. The
// inverse of CREATE TABLE: a missing table is 42P01 (or a no-op under IF EXISTS)
// (spec/design/grammar.md §13). These cover the single-table internals (catalog/row-store
// removal, re-create-after-drop, case-insensitivity); the IF EXISTS, multi-table
// (DROP TABLE a, b), and CASCADE/RESTRICT behaviors all agree with PostgreSQL and live in
// the corpus (suites/ddl/drop_table.test).
//
// mustCreate / wantErr are shared helpers from create_table_test.go (same package).

import "testing"

func TestDropRemovesTableAndRows(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 10), (2, 20)")
	out := mustCreate(t, db, "DROP TABLE t")
	if out.Kind != OutcomeStatement || out.Cost != 0 {
		t.Fatalf("DROP TABLE outcome = %+v, want statement cost 0", out)
	}
	if _, ok := db.Table("t"); ok {
		t.Error("catalog entry still present after drop")
	}
	if rows := db.RowsInKeyOrder("t"); rows != nil {
		t.Errorf("row store still present after drop: %v", rows)
	}
}

func TestNameIsFreeToRecreateAfterDrop(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 10)")
	mustCreate(t, db, "DROP TABLE t")
	// Re-create the freed name with a different shape; the new table starts empty.
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, w i64)")
	if rows := db.RowsInKeyOrder("t"); len(rows) != 0 {
		t.Errorf("re-created table not empty: %v", rows)
	}
	tbl, _ := db.Table("t")
	if tbl.Columns[1].Name != "w" {
		t.Errorf("re-created table shape wrong: %+v", tbl.Columns)
	}
}

func TestDropIsCaseInsensitive(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	mustCreate(t, db, "create table T (id i32 primary key)")
	mustCreate(t, db, "DROP TABLE t")
	if _, ok := db.Table("t"); ok {
		t.Error("case-insensitive drop failed")
	}
}

func TestDropLeavesOtherTablesIntact(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE a (id i32 PRIMARY KEY)")
	mustCreate(t, db, "CREATE TABLE b (id i32 PRIMARY KEY)")
	mustCreate(t, db, "INSERT INTO b VALUES (2)")
	mustCreate(t, db, "DROP TABLE a")
	if _, ok := db.Table("a"); ok {
		t.Error("table a should be gone")
	}
	if _, ok := db.Table("b"); !ok {
		t.Error("table b should remain")
	}
	if rows := db.RowsInKeyOrder("b"); len(rows) != 1 {
		t.Errorf("table b rows = %d, want 1", len(rows))
	}
}

func TestDropTableSyntaxErrors(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	wantErr(t, db, "DROP TABLE", "42601") // no table name
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	wantErr(t, db, "DROP TABLE t extra", "42601") // trailing input
	// DROP INDEX is its own statement now (spec/design/indexes.md §2): a missing index
	// is 42704, not a syntax error; DROP of any other object kind is still unparsed.
	wantErr(t, db, "DROP INDEX x", "42704")
	wantErr(t, db, "DROP VIEW v", "42601")
}
