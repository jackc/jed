package jed

// Collation host API (spec/design/collation.md §1/§4): db.ImportCollation (1c) plus the slice-1d
// host surface — ExportCollation, SetDefaultCollation / DefaultCollation, Collations, per-database
// default inheritance, and the baked file round-trip (format_version 17, entry_kind 3). These are the
// host-API + persistence behaviors the conformance corpus cannot express (CLAUDE.md §10); the
// in-memory SQL behavior lives in suites/collation/collate.test. Mirrors
// impl/rust/tests/collation_host.rs and impl/ts/tests/collation_host.test.ts.

import (
	"os"
	"testing"
)

func devRoot(t *testing.T) *Collation {
	t.Helper()
	def, err := os.ReadFile(specPath(t, "collation/fixtures/dev-root.allkeys"))
	if err != nil {
		t.Fatalf("read dev-root: %v", err)
	}
	coll, err := CompileCollation("dev-root", string(def))
	if err != nil {
		t.Fatalf("compile dev-root: %v", err)
	}
	return coll
}

// devRootNamedButNordicTable returns a collation under the name "dev-root" but with the dev-nordic
// table (a different content hash) — the conflicting import.
func devRootNamedButNordicTable(t *testing.T) *Collation {
	t.Helper()
	def := collDefinition(t, []string{"collation/fixtures/dev-root.allkeys", "collation/fixtures/dev-nordic.ldml"})
	coll, err := CompileCollation("dev-root", def)
	if err != nil {
		t.Fatalf("compile: %v", err)
	}
	return coll
}

func TestImportCollationThenUseInQuery(t *testing.T) {
	db := NewDatabase()
	if name, err := db.ImportCollation(devRoot(t)); err != nil || name != "dev-root" {
		t.Fatalf("import: name=%q err=%v", name, err)
	}
	// The imported collation is usable by name: 'ä' < 'z' is true under dev-root (ä near a), the
	// opposite of the C byte order where it is false.
	rows := query(t, db, `SELECT 'ä' < 'z' COLLATE "dev-root"`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("got %v, want [[true]]", rows)
	}
}

func TestImportCollationIdempotent(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("first import: %v", err)
	}
	// Re-importing the identical (name, content) collation is a no-op success.
	if name, err := db.ImportCollation(devRoot(t)); err != nil || name != "dev-root" {
		t.Fatalf("idempotent import: name=%q err=%v", name, err)
	}
}

func TestImportCollationConflictIs42710(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	// A DIFFERENT table under a name already in use is a conflict (collation.md §4).
	_, err := db.ImportCollation(devRootNamedButNordicTable(t))
	if err == nil || err.(*EngineError).Code() != "42710" {
		t.Fatalf("want 42710, got %v", err)
	}
}

func TestImportCollationRejectsC(t *testing.T) {
	db := NewDatabase()
	// C is table-free and built in; it is never imported (collation.md §4).
	def, _ := os.ReadFile(specPath(t, "collation/fixtures/dev-root.allkeys"))
	c, err := CompileCollation("C", string(def))
	if err != nil {
		t.Fatalf("compile C: %v", err)
	}
	if _, err := db.ImportCollation(c); err == nil || err.(*EngineError).Code() != "42710" {
		t.Fatalf("want 42710, got %v", err)
	}
}

// ---- slice 1d ----

func devNordic(t *testing.T) *Collation {
	t.Helper()
	def := collDefinition(t, []string{"collation/fixtures/dev-root.allkeys", "collation/fixtures/dev-nordic.ldml"})
	coll, err := CompileCollation("dev-nordic", def)
	if err != nil {
		t.Fatalf("compile dev-nordic: %v", err)
	}
	return coll
}

func texts(t *testing.T, rows [][]Value) []string {
	t.Helper()
	out := make([]string, len(rows))
	for i, r := range rows {
		if r[0].Kind != ValText {
			t.Fatalf("expected text, got %v", r[0])
		}
		out[i] = r[0].Str
	}
	return out
}

func eqStrings(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func TestPerColumnCollationOrdersImplicitly(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root")`)
	run(t, db, `INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')`)
	// No explicit COLLATE on the query: name sorts by its frozen dev-root collation (ä next to a).
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("implicit dev-root order: got %v", got)
	}
	// An explicit COLLATE "C" overrides back to byte order (ä is 2-byte UTF-8 → after z).
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name COLLATE "C"`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("explicit C order: got %v", got)
	}
}

func TestImplicitConflictIs42P22(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import dev-root: %v", err)
	}
	if _, err := db.ImportCollation(devNordic(t)); err != nil {
		t.Fatalf("import dev-nordic: %v", err)
	}
	run(t, db, `CREATE TABLE t (a text COLLATE "dev-root", b text COLLATE "dev-nordic", c text COLLATE "C")`)
	run(t, db, `INSERT INTO t VALUES ('a','z','b')`)
	for _, sql := range []string{`SELECT a < b FROM t`, `SELECT a < c FROM t`} {
		if _, err := Execute(db, sql); err == nil || err.(*EngineError).Code() != "42P22" {
			t.Fatalf("%s: want 42P22, got %v", sql, err)
		}
	}
	// An explicit COLLATE on one side breaks the tie: a='a' < (b='z') = true.
	rows := query(t, db, `SELECT a < b COLLATE "dev-root" FROM t`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("explicit override: got %v", rows)
	}
}

func TestCollateColumnErrors(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	if _, err := Execute(db, `CREATE TABLE t (a i32 COLLATE "dev-root")`); err == nil || err.(*EngineError).Code() != "42804" {
		t.Fatalf("non-text COLLATE: want 42804, got %v", err)
	}
	if _, err := Execute(db, `CREATE TABLE t (a text COLLATE "nope")`); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("unknown name: want 42704, got %v", err)
	}
}

func TestDefaultCollationInheritance(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	if db.DefaultCollation() != "C" {
		t.Fatalf("fresh default: got %q", db.DefaultCollation())
	}
	run(t, db, `CREATE TABLE before (id i32 PRIMARY KEY, name text)`)
	if err := db.SetDefaultCollation("dev-root"); err != nil {
		t.Fatalf("set default: %v", err)
	}
	if db.DefaultCollation() != "dev-root" {
		t.Fatalf("default after set: got %q", db.DefaultCollation())
	}
	run(t, db, `CREATE TABLE after (id i32 PRIMARY KEY, name text)`)
	run(t, db, `INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM after ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("after inherited dev-root: got %v", got)
	}
	run(t, db, `INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM before ORDER BY name`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("before frozen at C: got %v", got)
	}
	// SetDefaultCollation of an unloaded name is 42704; C always resolves.
	if err := db.SetDefaultCollation("nope"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("set default unknown: want 42704, got %v", err)
	}
	if err := db.SetDefaultCollation("C"); err != nil {
		t.Fatalf("set default C: %v", err)
	}
}

func TestExportAndIntrospect(t *testing.T) {
	db := NewDatabase()
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	exported, err := db.ExportCollation("dev-root")
	if err != nil || exported.Name != "dev-root" {
		t.Fatalf("export: name=%v err=%v", exported, err)
	}
	db2 := NewDatabase()
	if name, err := db2.ImportCollation(exported); err != nil || name != "dev-root" {
		t.Fatalf("re-import exported: name=%q err=%v", name, err)
	}
	if _, err := db.ExportCollation("nope"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("export unknown: want 42704, got %v", err)
	}
	if _, err := db.ExportCollation("C"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("export C: want 42704, got %v", err)
	}
	if err := db.SetDefaultCollation("dev-root"); err != nil {
		t.Fatalf("set default: %v", err)
	}
	infos := db.Collations()
	if len(infos) != 1 || infos[0].Name != "dev-root" || !infos[0].IsDefault {
		t.Fatalf("introspect: got %+v", infos)
	}
}

func TestBakedFileRoundTrip(t *testing.T) {
	path := t.TempDir() + "/collation_baked.jed"
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import: %v", err)
	}
	if err := db.SetDefaultCollation("dev-root"); err != nil {
		t.Fatalf("set default: %v", err)
	}
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root", plain text)`)
	run(t, db, `INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')`)
	if err := db.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	re, err := Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if re.DefaultCollation() != "dev-root" {
		t.Fatalf("reopened default: got %q", re.DefaultCollation())
	}
	if len(re.Collations()) != 1 {
		t.Fatalf("reopened collations: got %d", len(re.Collations()))
	}
	if got := texts(t, query(t, re, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened collated column: got %v", got)
	}
	// plain (un-annotated) inherited the default (dev-root) at create.
	if got := texts(t, query(t, re, `SELECT plain FROM t ORDER BY plain`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened inherited column: got %v", got)
	}
	if err := re.Close(); err != nil {
		t.Fatalf("close re: %v", err)
	}
}
