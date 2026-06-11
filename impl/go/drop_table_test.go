package jed

// DROP TABLE — remove a table (its definition + all its rows) from the catalog. The
// inverse of CREATE TABLE: a missing table is 42P01 and there is no IF EXISTS this
// slice; single table, no CASCADE/RESTRICT (spec/design/grammar.md §13).
//
// mustCreate / wantErr are shared helpers from create_table_test.go (same package).

import "testing"

func TestDropRemovesTableAndRows(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
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

func TestAccessAfterDropIsUndefinedTable(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	mustCreate(t, db, "DROP TABLE t")
	// Every access path shares the same catalog lookup, so all four trap 42P01.
	for _, sql := range []string{
		"SELECT id FROM t",
		"INSERT INTO t VALUES (1, 1)",
		"UPDATE t SET v = 0",
		"DELETE FROM t",
	} {
		wantErr(t, db, sql, "42P01")
	}
}

func TestDroppingMissingTableTraps42P01(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "DROP TABLE nope", "42P01")
	// No IF EXISTS this slice: a second drop of the same name also errors.
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustCreate(t, db, "DROP TABLE t")
	wantErr(t, db, "DROP TABLE t", "42P01")
}

func TestNameIsFreeToRecreateAfterDrop(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 10)")
	mustCreate(t, db, "DROP TABLE t")
	// Re-create the freed name with a different shape; the new table starts empty.
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, w int64)")
	if rows := db.RowsInKeyOrder("t"); len(rows) != 0 {
		t.Errorf("re-created table not empty: %v", rows)
	}
	tbl, _ := db.Table("t")
	if tbl.Columns[1].Name != "w" {
		t.Errorf("re-created table shape wrong: %+v", tbl.Columns)
	}
}

func TestDropIsCaseInsensitive(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "create table T (id int32 primary key)")
	mustCreate(t, db, "DROP TABLE t")
	if _, ok := db.Table("t"); ok {
		t.Error("case-insensitive drop failed")
	}
}

func TestDropLeavesOtherTablesIntact(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE a (id int32 PRIMARY KEY)")
	mustCreate(t, db, "CREATE TABLE b (id int32 PRIMARY KEY)")
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
	db := NewDatabase()
	wantErr(t, db, "DROP TABLE", "42601") // no table name
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	wantErr(t, db, "DROP TABLE t extra", "42601") // trailing input
	// DROP INDEX is its own statement now (spec/design/indexes.md §2): a missing index
	// is 42704, not a syntax error; DROP of any other object kind is still unparsed.
	wantErr(t, db, "DROP INDEX x", "42704")
	wantErr(t, db, "DROP VIEW v", "42601")
}
