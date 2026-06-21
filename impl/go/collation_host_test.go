package jed

// Collation host API + persistence (spec/design/collation.md §1/§4.2): the reference-only surface —
// SetDefaultCollation / DefaultCollation, the per-file db.Collations (what the database REFERENCES)
// vs the build-global VendoredCollations (what the engine VENDORS), per-column / per-database default
// inheritance, collated keys, and the reference-only FILE ROUND-TRIP (format_version 18, entry_kind 3
// metadata entries). These are the host-API + persistence behaviors the conformance corpus cannot
// express (CLAUDE.md §10); the in-memory SQL behavior a collation drives (COLLATE / ORDER BY /
// derivation / 42P21 / 42P22) lives in suites/collation/collate.test, which runs on every core. There
// is NO ImportCollation: a collation is vendored into the binary and used by name (the reference-only
// pivot, §4.2). Mirrors impl/rust/tests/collation_host.rs and impl/ts/tests/collation_host.test.ts.

import "testing"

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

// ---- the vendored set (the engine-global build property) ----

func TestVendoredCollationsIsTheDevFixtures(t *testing.T) {
	// VendoredCollations reports what THIS BUILD provides — the dev fixture set, ascending by name, no
	// IsDefault (a build property, not a per-db one). C is built in and never listed.
	v := VendoredCollations()
	names := make([]string, len(v))
	for i, c := range v {
		names[i] = c.Name
		if c.IsDefault {
			t.Fatalf("vendored %q must not be IsDefault", c.Name)
		}
	}
	if !eqStrings(names, []string{"dev-nordic", "dev-root"}) {
		t.Fatalf("vendored set: got %v, want [dev-nordic dev-root]", names)
	}
	if v[1].Name != "dev-root" || v[1].UnicodeVersion != "0.0.0-dev" {
		t.Fatalf("dev-root entry: got %+v", v[1])
	}
	if VendoredCollation("dev-root") == nil {
		t.Fatalf("dev-root should be vendored")
	}
	if VendoredCollation("C") != nil {
		t.Fatalf("C must never be vendored")
	}
}

// ---- using a vendored collation needs NO import ----

func TestVendoredCollationUsedInAnExpression(t *testing.T) {
	// COLLATE "dev-root" resolves from the binary's vendored set with no import: 'ä' < 'z' is true under
	// dev-root (ä near a), the opposite of the C byte order where it is false. A transient query COLLATE
	// does not make the database REFERENCE the collation, so db.Collations() stays empty.
	db := NewDatabase()
	if n := len(db.Collations()); n != 0 {
		t.Fatalf("fresh db references %d collations, want 0", n)
	}
	rows := query(t, db, `SELECT 'ä' < 'z' COLLATE "dev-root"`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("got %v, want [[true]]", rows)
	}
}

func TestUnknownCollationIs42704(t *testing.T) {
	// A collation neither vendored nor referenced is 42704 (the vendored fallback must not mask it).
	db := NewDatabase()
	if _, err := Execute(db, `SELECT 'x' COLLATE "no-such-collation"`); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("want 42704, got %v", err)
	}
}

func TestPerColumnCollationOrdersImplicitlyAndIsReferenced(t *testing.T) {
	// A column declared COLLATE "dev-root" (vendored, no import) sorts by that collation with no explicit
	// COLLATE on the query — dev-root puts ä next to a. Because the SCHEMA now references dev-root,
	// db.Collations() (the per-file view) lists exactly it.
	db := NewDatabase()
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root")`)
	run(t, db, `INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("implicit dev-root order: got %v", got)
	}
	refs := db.Collations()
	if len(refs) != 1 || refs[0].Name != "dev-root" || refs[0].IsDefault {
		t.Fatalf("referenced set: got %+v", refs)
	}
	// An explicit COLLATE "C" on the query overrides back to byte order (ä is 2-byte UTF-8 → after z).
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name COLLATE "C"`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("explicit C order: got %v", got)
	}
}

func TestImplicitConflictIs42P22(t *testing.T) {
	// Two columns with DIFFERENT implicit (vendored) collations compared with no explicit COLLATE →
	// 42P22 (PG-matching). C counts as a distinct implicit collation, so dev-root vs C also conflicts.
	db := NewDatabase()
	run(t, db, `CREATE TABLE t (a text COLLATE "dev-root", b text COLLATE "dev-nordic", c text COLLATE "C")`)
	run(t, db, `INSERT INTO t VALUES ('a','z','b')`)
	for _, sql := range []string{`SELECT a < b FROM t`, `SELECT a < c FROM t`} {
		if _, err := Execute(db, sql); err == nil || err.(*EngineError).Code() != "42P22" {
			t.Fatalf("%s: want 42P22, got %v", sql, err)
		}
	}
	// An explicit COLLATE on one side breaks the tie (no error): a='a' < (b='z') = true.
	rows := query(t, db, `SELECT a < b COLLATE "dev-root" FROM t`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("explicit override: got %v", rows)
	}
	// The table references both vendored collations → db.Collations() lists them (sorted).
	var names []string
	for _, c := range db.Collations() {
		names = append(names, c.Name)
	}
	if !eqStrings(names, []string{"dev-nordic", "dev-root"}) {
		t.Fatalf("referenced names: got %v", names)
	}
}

func TestNonTextCollateIs42804UnknownName42704(t *testing.T) {
	db := NewDatabase()
	if _, err := Execute(db, `CREATE TABLE t (a i32 COLLATE "dev-root")`); err == nil || err.(*EngineError).Code() != "42804" {
		t.Fatalf("non-text COLLATE: want 42804, got %v", err)
	}
	if _, err := Execute(db, `CREATE TABLE t (a text COLLATE "nope")`); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("unknown name: want 42704, got %v", err)
	}
}

// ---- the per-database default (over the vendored set, no import) ----

func TestDefaultCollationInheritedByUnannotatedColumn(t *testing.T) {
	// SetDefaultCollation moves the per-database default to a VENDORED collation (no import); an
	// un-annotated text column created AFTER inherits it (frozen), one created BEFORE keeps C.
	db := NewDatabase()
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
	// after.name inherited dev-root → ä sorts next to a even with no COLLATE clause.
	if got := texts(t, query(t, db, `SELECT name FROM after ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("after inherited dev-root: got %v", got)
	}
	// before.name was frozen at C → byte order.
	run(t, db, `INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM before ORDER BY name`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("before frozen at C: got %v", got)
	}
	// The default makes dev-root referenced (IsDefault true).
	refs := db.Collations()
	if len(refs) != 1 || !refs[0].IsDefault {
		t.Fatalf("referenced set: got %+v", refs)
	}
}

func TestSetDefaultUnknownIs42704(t *testing.T) {
	db := NewDatabase()
	if err := db.SetDefaultCollation("nope"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("set default unknown: want 42704, got %v", err)
	}
	if err := db.SetDefaultCollation("C"); err != nil { // C always resolves (resets to byte order)
		t.Fatalf("set default C: %v", err)
	}
}

// ---- collated keys (slice 1e, on-disk/internal — the corpus cannot express it) ----

func TestCollatedPrimaryKeyStoredInCollationOrder(t *testing.T) {
	// A collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so the B-tree
	// physically iterates in COLLATION order. dev-root (vendored, no import): a < A < b < Z; C bytes:
	// A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
	db := NewDatabase()
	run(t, db, `CREATE TABLE t (name text COLLATE "dev-root" PRIMARY KEY)`)
	run(t, db, `INSERT INTO t VALUES ('Z'),('a'),('b'),('A')`)
	if got := texts(t, query(t, db, `SELECT name FROM t`)); !eqStrings(got, []string{"a", "A", "b", "Z"}) {
		t.Fatalf("collated PK stored order: got %v", got)
	}
	run(t, db, `CREATE TABLE c (name text PRIMARY KEY)`)
	run(t, db, `INSERT INTO c VALUES ('Z'),('a'),('b'),('A')`)
	if got := texts(t, query(t, db, `SELECT name FROM c`)); !eqStrings(got, []string{"A", "Z", "a", "b"}) {
		t.Fatalf("C PK stored order: got %v", got)
	}
}

func TestCollatedUniqueDedupsByByteIdentity(t *testing.T) {
	// A collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A' are
	// DISTINCT, both admitted — collation.md §7), like a C unique key; only a byte-duplicate violates.
	db := NewDatabase()
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root" UNIQUE)`)
	run(t, db, `INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')`)
	if _, err := Execute(db, `INSERT INTO t VALUES (4,'a')`); err == nil || err.(*EngineError).Code() != "23505" {
		t.Fatalf("collated UNIQUE duplicate: want 23505, got %v", err)
	}
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "A", "b"}) {
		t.Fatalf("collated UNIQUE order: got %v", got)
	}
}

// ---- reference-only file round-trip (format_version 18) ----

func TestReferenceOnlyFileRoundTrip(t *testing.T) {
	// A collated table + the per-database default survive a close + paged reopen. The file stores only a
	// metadata REFERENCE entry (no table); on reopen the table is resolved from the vendored set.
	path := t.TempDir() + "/collation_refonly_roundtrip.jed"
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := db.SetDefaultCollation("dev-root"); err != nil { // vendored — no import
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
	// The database still references dev-root (per-file view) — resolved from the vendored set.
	refs := re.Collations()
	if len(refs) != 1 || refs[0].Name != "dev-root" || refs[0].UnicodeVersion != "0.0.0-dev" || !refs[0].IsDefault {
		t.Fatalf("reopened referenced set: got %+v", refs)
	}
	if got := texts(t, query(t, re, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened collated column: got %v", got)
	}
	// plain (un-annotated) inherited the default (dev-root) at create → also dev-root order.
	if got := texts(t, query(t, re, `SELECT plain FROM t ORDER BY plain`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened inherited column: got %v", got)
	}
	if err := re.Close(); err != nil {
		t.Fatalf("close re: %v", err)
	}
}
