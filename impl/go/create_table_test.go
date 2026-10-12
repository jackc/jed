package jed

// Phase B: CREATE TABLE — parse, analyze, register in the catalog. Driven by unit
// tests until the `core` profile is complete and the corpus runs (Phase E).

import (
	"bytes"
	"slices"
	"testing"
)

func mustCreate(t *testing.T, db dbHandle, sql string) outcome {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("Execute(%q) error: %v", sql, err)
	}
	return out
}

func wantErr(t *testing.T, db dbHandle, sql, code string) {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("Execute(%q): expected error %s, got success", sql, code)
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != code {
		t.Fatalf("Execute(%q): expected error %s, got %v", sql, code, err)
	}
}

func TestCreatesTableWithResolvedTypesAndPK(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	out := mustCreate(t, db, "CREATE TABLE nums (id i32 PRIMARY KEY, small i16, big i64)")
	if out.Kind != outcomeStatement {
		t.Fatalf("expected statement outcome, got %v", out.Kind)
	}
	tbl, ok := db.Table("nums")
	if !ok {
		t.Fatal("table not registered")
	}
	if len(tbl.Columns) != 3 {
		t.Fatalf("expected 3 columns, got %d", len(tbl.Columns))
	}
	if tbl.Columns[0].Name != "id" || tbl.Columns[0].Type.ScalarTy() != scalarInt32 ||
		!tbl.Columns[0].PrimaryKey || !tbl.Columns[0].NotNull {
		t.Errorf("col 0 wrong: %+v", tbl.Columns[0])
	}
	if tbl.Columns[1].Type.ScalarTy() != scalarInt16 || tbl.Columns[1].PrimaryKey || tbl.Columns[1].NotNull {
		t.Errorf("col 1 wrong: %+v", tbl.Columns[1])
	}
	if tbl.Columns[2].Type.ScalarTy() != scalarInt64 {
		t.Errorf("col 2 wrong: %+v", tbl.Columns[2])
	}
	if tbl.PrimaryKeyIndex() != 0 {
		t.Errorf("PrimaryKeyIndex got %d want 0", tbl.PrimaryKeyIndex())
	}
}

func TestSQLStandardTypeAliasesResolve(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE t (a smallint, b integer, c int, d bigint)")
	tbl, _ := db.Table("t")
	want := []scalarType{scalarInt16, scalarInt32, scalarInt32, scalarInt64}
	for i, w := range want {
		if tbl.Columns[i].Type.ScalarTy() != w {
			t.Errorf("col %d: got %v want %v", i, tbl.Columns[i].Type.ScalarTy(), w)
		}
	}
}

// A bare name folds to lowercase; the host API looks a name up exactly (grammar.md §3).
func TestBareNamesFoldAndHostLookupIsExact(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustCreate(t, db, "create table T (Id I32 primary key)")
	tbl, ok := db.Table("t")
	if !ok {
		t.Fatal("lowercase lookup failed")
	}
	if tbl.Columns[0].Name != "id" {
		t.Errorf("column name = %q, want id", tbl.Columns[0].Name)
	}
	if _, ok := db.Table("T"); ok {
		t.Error("uppercase lookup must not reach the folded table t")
	}
	mustCreate(t, db, `create table "T" ("Id" i32 primary key)`)
	up, ok := db.Table("T")
	if !ok || up.Columns[0].Name != "Id" {
		t.Fatalf("quoted table T missing or wrong column: %v", up)
	}
}

// The on-disk catalog emits each name group in ascending BYTE order of the exact name
// (format_version 37): uppercase ASCII sorts before lowercase, so B < C < a, and the reload keeps
// every name exact (a bare lookup never reaches a mixed-case name).
func TestCatalogOrderIsExactNameBytesAcrossReload(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		"CREATE TABLE a (id i32 PRIMARY KEY)",
		`CREATE TABLE "C" (id i32 PRIMARY KEY)`,
		`CREATE TABLE "B" (id i32 PRIMARY KEY, "V" i32, v i32)`,
		`CREATE INDEX "Zed" ON "B" (v)`,
		`CREATE INDEX alpha ON "B" ("V")`,
		`CREATE SEQUENCE "Seq"`,
		"CREATE SEQUENCE aseq",
	} {
		mustCreate(t, db, sql)
	}
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	loaded, err := loadEngine(image)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for name := range loaded.readSnap().tables {
		got = append(got, name)
	}
	slices.Sort(got)
	if !slices.Equal(got, []string{"B", "C", "a"}) {
		t.Fatalf("table names = %v", got)
	}
	b, ok := loaded.Table("B")
	if !ok {
		t.Fatal("table B missing")
	}
	var idx []string
	for _, ix := range b.Indexes {
		idx = append(idx, ix.Name)
	}
	if !slices.Equal(idx, []string{"Zed", "alpha"}) {
		t.Fatalf("index order = %v", idx)
	}
	if b.Columns[1].Name != "V" || b.Columns[2].Name != "v" {
		t.Fatalf("columns = %q, %q", b.Columns[1].Name, b.Columns[2].Name)
	}
	if _, ok := loaded.Table("b"); ok {
		t.Fatal("bare b must not reach B")
	}
	if _, err := queryOutcome(loaded, `SELECT nextval('"Seq"')`, nil); err != nil {
		t.Fatalf("nextval Seq: %v", err)
	}
	wantErr(t, loaded, "SELECT nextval('Seq')", "42P01") // the bare text folds to seq
	// Re-serializing the catalog is byte-identical (the reader trusts the emitted order); the
	// nextval above advanced "Seq", so compare against a fresh reload of the original image.
	fresh, err := loadEngine(image)
	if err != nil {
		t.Fatal(err)
	}
	again, err := fresh.ToImage(8192, 1)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(again, image) {
		t.Fatal("re-serialized image differs")
	}
}

func TestDuplicateTableIsRejected(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	wantErr(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)", "42P07")
}

func TestDuplicateColumnIsRejected(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	wantErr(t, db, "CREATE TABLE t (a i32, a i16)", "42701")
}

func TestUnknownTypeIsRejected(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	wantErr(t, db, "CREATE TABLE t (a int128)", "42704")
	// The old jed bit-names are a CLEAN BREAK — replaced by the i/f prefix, no longer
	// accepted (CLAUDE.md §4; types.md §11).
	wantErr(t, db, "CREATE TABLE t (a int32)", "42704")
	wantErr(t, db, "CREATE TABLE t (a float64)", "42704")
}

func TestPGByteShorthandTypeNamesAreAccepted(t *testing.T) {
	t.Parallel()
	// The i/f prefix makes jed's bit-namespace (i8…i64) lexically disjoint from PG's
	// byte-namespace, so PG's byte-shorthand is accepted as aliases (CLAUDE.md §1/§4;
	// types.md §11): int2→i16, int4→i32, int8→i64, float4→f32, float8→f64. There is no
	// int8-means-8-bit collision, and a future 8-bit i8 stays free.
	db := memDB().Session(SessionOptions{})
	mustCreate(t, db, "CREATE TABLE t (a int2, b int4, c int8, d float4, e float8)")
	tbl, _ := db.Table("t")
	want := []scalarType{scalarInt16, scalarInt32, scalarInt64, scalarFloat32, scalarFloat64}
	for i, w := range want {
		if got := tbl.Columns[i].Type.ScalarTy(); got != w {
			t.Errorf("col %d: got %v want %v", i, got, w)
		}
	}
}

func TestMultiplePrimaryKeysAreRejected(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	wantErr(t, db, "CREATE TABLE t (a i32 PRIMARY KEY, b i32 PRIMARY KEY)", "42P16")
}

func TestSyntaxErrorsAreReported(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	wantErr(t, db, "CREATE TABLE t", "42601")
	wantErr(t, db, "CREATE TABLE t (a i32,)", "42601")
}
