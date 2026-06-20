package jed

// Collation slice 1c — the host db.ImportCollation API (spec/design/collation.md §4). These are the
// host-API behaviors the conformance corpus cannot express (CLAUDE.md §10): the import call itself,
// its idempotency, the same-name conflict, and the C rejection. The SQL behavior a loaded collation
// drives (COLLATE / ORDER BY / errors) lives in suites/collation/collate.test, which runs on every
// core. Mirrors impl/rust/tests/collation_host.rs and impl/ts/tests/collation_host.test.ts.

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
