package abide

// Phase B: CREATE TABLE — parse, analyze, register in the catalog. Driven by unit
// tests until the `core` profile is complete and the corpus runs (Phase E).

import "testing"

func mustCreate(t *testing.T, db *Database, sql string) Outcome {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("Execute(%q) error: %v", sql, err)
	}
	return out
}

func wantErr(t *testing.T, db *Database, sql, code string) {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("Execute(%q): expected error %s, got success", sql, code)
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != code {
		t.Fatalf("Execute(%q): expected error %s, got %v", sql, code, err)
	}
}

func TestCreatesTableWithResolvedTypesAndPK(t *testing.T) {
	db := NewDatabase()
	out := mustCreate(t, db, "CREATE TABLE nums (id int32 PRIMARY KEY, small int16, big int64)")
	if out.Kind != OutcomeStatement {
		t.Fatalf("expected statement outcome, got %v", out.Kind)
	}
	tbl, ok := db.Table("nums")
	if !ok {
		t.Fatal("table not registered")
	}
	if len(tbl.Columns) != 3 {
		t.Fatalf("expected 3 columns, got %d", len(tbl.Columns))
	}
	if tbl.Columns[0].Name != "id" || tbl.Columns[0].Type != Int32 ||
		!tbl.Columns[0].PrimaryKey || !tbl.Columns[0].NotNull {
		t.Errorf("col 0 wrong: %+v", tbl.Columns[0])
	}
	if tbl.Columns[1].Type != Int16 || tbl.Columns[1].PrimaryKey || tbl.Columns[1].NotNull {
		t.Errorf("col 1 wrong: %+v", tbl.Columns[1])
	}
	if tbl.Columns[2].Type != Int64 {
		t.Errorf("col 2 wrong: %+v", tbl.Columns[2])
	}
	if tbl.PrimaryKeyIndex() != 0 {
		t.Errorf("PrimaryKeyIndex got %d want 0", tbl.PrimaryKeyIndex())
	}
}

func TestSQLStandardTypeAliasesResolve(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE t (a smallint, b integer, c int, d bigint)")
	tbl, _ := db.Table("t")
	want := []ScalarType{Int16, Int32, Int32, Int64}
	for i, w := range want {
		if tbl.Columns[i].Type != w {
			t.Errorf("col %d: got %v want %v", i, tbl.Columns[i].Type, w)
		}
	}
}

func TestTableAndTypeNamesAreCaseInsensitive(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "create table T (Id INT32 primary key)")
	if _, ok := db.Table("t"); !ok {
		t.Error("lowercase lookup failed")
	}
	if _, ok := db.Table("T"); !ok {
		t.Error("uppercase lookup failed")
	}
}

func TestDuplicateTableIsRejected(t *testing.T) {
	db := NewDatabase()
	mustCreate(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	wantErr(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)", "42P07")
}

func TestDuplicateColumnIsRejected(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "CREATE TABLE t (a int32, a int16)", "42701")
}

func TestUnknownTypeIsRejected(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "CREATE TABLE t (a int128)", "42704")
}

func TestPGInternalTypeNamesAreNotAccepted(t *testing.T) {
	// We own our surface (CLAUDE.md §1): int2/int4/int8 are NOT type names.
	db := NewDatabase()
	wantErr(t, db, "CREATE TABLE t (a int2)", "42704")
}

func TestMultiplePrimaryKeysAreRejected(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "CREATE TABLE t (a int32 PRIMARY KEY, b int32 PRIMARY KEY)", "42P16")
}

func TestSyntaxErrorsAreReported(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "CREATE TABLE t", "42601")
	wantErr(t, db, "CREATE TABLE t (a int32,)", "42601")
}
