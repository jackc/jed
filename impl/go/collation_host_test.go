package jed

// Collation host API + persistence (spec/design/collation.md §1/§4.2): the host-loaded surface —
// LoadUnicodeData (the JUCD bundle load seam), SetDefaultCollation / DefaultCollation, the per-file
// db.Collations (what the database REFERENCES) vs the engine-global db.LoadedCollations (what a loaded
// bundle PROVIDES), per-column / per-database default inheritance, collated keys, and the
// reference-only FILE ROUND-TRIP (format_version 18, entry_kind 3 metadata entries). These are the
// host-API + persistence behaviors the conformance corpus cannot express (CLAUDE.md §10); the
// in-memory SQL behavior a collation drives (COLLATE / ORDER BY / derivation / 42P21 / 42P22) lives in
// suites/collation/collate.test, which runs on every core. There is NO ImportCollation: the bare
// binary carries no Unicode data and the host loads jed's own pinned bundle bytes (the SQLite model,
// §9/§16), then uses collations by name. Mirrors impl/rust/tests/collation_host.rs and
// impl/ts/tests/collation_host.test.ts.

import (
	"strings"
	"testing"
)

func texts(t *testing.T, rows [][]Value) []string {
	t.Helper()
	out := make([]string, len(rows))
	for i, r := range rows {
		if r[0].Kind != ValText {
			t.Fatalf("expected text, got %v", r[0])
		}
		out[i] = r[0].str()
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

// ---- the loaded set (the engine-global property a bundle provides) ----

func TestLoadedCollationsIsTheRealSet(t *testing.T) {
	t.Parallel()
	// db.LoadedCollations reports what a loaded bundle PROVIDES — after loading jed's pinned production
	// bundle, the real version-pinned set (es, unicode), ascending by name, no IsDefault (an engine
	// property, not a per-db one). C is built in and never listed. The pin is UCA/UCD 17.0.0.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	v := db.LoadedCollations()
	names := make([]string, len(v))
	for i, c := range v {
		names[i] = c.Name
		if c.IsDefault {
			t.Fatalf("loaded %q must not be IsDefault", c.Name)
		}
	}
	if !eqStrings(names, []string{"es", "unicode"}) {
		t.Fatalf("loaded set: got %v, want [es unicode]", names)
	}
	if v[1].Name != "unicode" || v[1].UnicodeVersion != "17.0.0" {
		t.Fatalf("unicode entry: got %+v", v[1])
	}
	if LoadedCollation("unicode") == nil || LoadedCollation("es") == nil {
		t.Fatalf("unicode and es should be loaded")
	}
	if LoadedCollation("C") != nil {
		t.Fatalf("C must never be loaded")
	}
}

// ---- using a loaded collation needs NO import ----

func TestLoadedCollationUsedInAnExpression(t *testing.T) {
	t.Parallel()
	// COLLATE "unicode" resolves from the engine's loaded set with no import: 'ä' < 'z' is true under
	// the root (ä near a), the opposite of the C byte order where it is false. A transient query COLLATE
	// does not make the database REFERENCE the collation, so db.Collations() stays empty.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if n := len(db.Collations()); n != 0 {
		t.Fatalf("fresh db references %d collations, want 0", n)
	}
	rows := query(t, db, `SELECT 'ä' < 'z' COLLATE "unicode"`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("got %v, want [[true]]", rows)
	}
}

func TestEsOrdersEnyeAsADistinctLetter(t *testing.T) {
	t.Parallel()
	// The es tailoring (&N<ñ<<<Ñ) makes ñ a distinct PRIMARY letter after n: 'nz' < 'ña' (n < ñ),
	// whereas under the untailored root ñ is n+accent so 'ña' < 'nz'. The Spanish-collation headline.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if rows := query(t, db, `SELECT 'nz' < 'ña' COLLATE "es"`); len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("es 'nz' < 'ña': got %v, want [[true]]", rows)
	}
	if rows := query(t, db, `SELECT 'nz' < 'ña' COLLATE "unicode"`); len(rows) != 1 || rows[0][0] != BoolValue(false) {
		t.Fatalf("unicode 'nz' < 'ña': got %v, want [[false]]", rows)
	}
}

func TestUnknownCollationIs42704(t *testing.T) {
	t.Parallel()
	// A collation neither loaded nor referenced is 42704 (the loaded-set fallback must not mask it).
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, `SELECT 'x' COLLATE "no-such-collation"`, nil); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("want 42704, got %v", err)
	}
}

func TestPerColumnCollationOrdersImplicitlyAndIsReferenced(t *testing.T) {
	t.Parallel()
	// A column declared COLLATE "unicode" (loaded, no import) sorts by that collation with no explicit
	// COLLATE on the query — unicode puts ä next to a. Because the SCHEMA now references unicode,
	// db.Collations() (the per-file view) lists exactly it.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode")`)
	run(t, db, `INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("implicit unicode order: got %v", got)
	}
	refs := db.Collations()
	if len(refs) != 1 || refs[0].Name != "unicode" || refs[0].IsDefault {
		t.Fatalf("referenced set: got %+v", refs)
	}
	// An explicit COLLATE "C" on the query overrides back to byte order (ä is 2-byte UTF-8 → after z).
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name COLLATE "C"`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("explicit C order: got %v", got)
	}
}

func TestImplicitConflictIs42P22(t *testing.T) {
	t.Parallel()
	// Two columns with DIFFERENT implicit (loaded) collations compared with no explicit COLLATE →
	// 42P22 (PG-matching). C counts as a distinct implicit collation, so unicode vs C also conflicts.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	run(t, db, `CREATE TABLE t (a text COLLATE "unicode", b text COLLATE "es", c text COLLATE "C")`)
	run(t, db, `INSERT INTO t VALUES ('a','z','b')`)
	for _, sql := range []string{`SELECT a < b FROM t`, `SELECT a < c FROM t`} {
		if _, err := queryOutcome(db, sql, nil); err == nil || err.(*EngineError).Code() != "42P22" {
			t.Fatalf("%s: want 42P22, got %v", sql, err)
		}
	}
	// An explicit COLLATE on one side breaks the tie (no error): a='a' < (b='z') = true.
	rows := query(t, db, `SELECT a < b COLLATE "unicode" FROM t`)
	if len(rows) != 1 || rows[0][0] != BoolValue(true) {
		t.Fatalf("explicit override: got %v", rows)
	}
	// The table references both loaded collations → db.Collations() lists them (sorted).
	var names []string
	for _, c := range db.Collations() {
		names = append(names, c.Name)
	}
	if !eqStrings(names, []string{"es", "unicode"}) {
		t.Fatalf("referenced names: got %v", names)
	}
}

func TestNonTextCollateIs42804UnknownName42704(t *testing.T) {
	t.Parallel()
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if _, err := queryOutcome(db, `CREATE TABLE t (a i32 COLLATE "unicode")`, nil); err == nil || err.(*EngineError).Code() != "42804" {
		t.Fatalf("non-text COLLATE: want 42804, got %v", err)
	}
	if _, err := queryOutcome(db, `CREATE TABLE t (a text COLLATE "nope")`, nil); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("unknown name: want 42704, got %v", err)
	}
}

// ---- the per-database default (over the loaded set, no import) ----

func TestDefaultCollationInheritedByUnannotatedColumn(t *testing.T) {
	t.Parallel()
	// SetDefaultCollation moves the per-database default to a LOADED collation (no import); an
	// un-annotated text column created AFTER inherits it (frozen), one created BEFORE keeps C.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if db.DefaultCollation() != "C" {
		t.Fatalf("fresh default: got %q", db.DefaultCollation())
	}
	run(t, db, `CREATE TABLE before (id i32 PRIMARY KEY, name text)`)
	if err := db.SetDefaultCollation("unicode"); err != nil {
		t.Fatalf("set default: %v", err)
	}
	if db.DefaultCollation() != "unicode" {
		t.Fatalf("default after set: got %q", db.DefaultCollation())
	}
	run(t, db, `CREATE TABLE after (id i32 PRIMARY KEY, name text)`)
	run(t, db, `INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')`)
	// after.name inherited unicode → ä sorts next to a even with no COLLATE clause.
	if got := texts(t, query(t, db, `SELECT name FROM after ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("after inherited unicode: got %v", got)
	}
	// before.name was frozen at C → byte order.
	run(t, db, `INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')`)
	if got := texts(t, query(t, db, `SELECT name FROM before ORDER BY name`)); !eqStrings(got, []string{"a", "z", "ä"}) {
		t.Fatalf("before frozen at C: got %v", got)
	}
	// The default makes unicode referenced (IsDefault true).
	refs := db.Collations()
	if len(refs) != 1 || !refs[0].IsDefault {
		t.Fatalf("referenced set: got %+v", refs)
	}
}

func TestSetDefaultUnknownIs42704(t *testing.T) {
	t.Parallel()
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	if err := db.SetDefaultCollation("nope"); err == nil || err.(*EngineError).Code() != "42704" {
		t.Fatalf("set default unknown: want 42704, got %v", err)
	}
	if err := db.SetDefaultCollation("C"); err != nil { // C always resolves (resets to byte order)
		t.Fatalf("set default C: %v", err)
	}
}

// ---- collated keys (slice 1e, on-disk/internal — the corpus cannot express it) ----

func TestCollatedPrimaryKeyStoredInCollationOrder(t *testing.T) {
	t.Parallel()
	// A collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so the B-tree
	// physically iterates in COLLATION order. unicode (loaded, no import): a < A < b < Z; C bytes:
	// A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	run(t, db, `CREATE TABLE t (name text COLLATE "unicode" PRIMARY KEY)`)
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
	t.Parallel()
	// A collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A' are
	// DISTINCT, both admitted — collation.md §7), like a C unique key; only a byte-duplicate violates.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode" UNIQUE)`)
	run(t, db, `INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')`)
	if _, err := queryOutcome(db, `INSERT INTO t VALUES (4,'a')`, nil); err == nil || err.(*EngineError).Code() != "23505" {
		t.Fatalf("collated UNIQUE duplicate: want 23505, got %v", err)
	}
	if got := texts(t, query(t, db, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "A", "b"}) {
		t.Fatalf("collated UNIQUE order: got %v", got)
	}
}

// ---- reference-only file round-trip (format_version 18) ----

func TestReferenceOnlyFileRoundTrip(t *testing.T) {
	t.Parallel()
	// A collated table + the per-database default survive a close + paged reopen. The file stores only a
	// metadata REFERENCE entry (no table); on reopen the table is resolved from a loaded bundle (the host
	// must have loaded one providing it BEFORE open — collation.md §4/§9).
	loadFixtureBundle(t)
	path := t.TempDir() + "/collation_refonly_roundtrip.jed"
	db, err := create(path, databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := db.SetDefaultCollation("unicode"); err != nil { // loaded — no import
		t.Fatalf("set default: %v", err)
	}
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode", plain text)`)
	run(t, db, `INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')`)
	if err := db.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	re, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if re.DefaultCollation() != "unicode" {
		t.Fatalf("reopened default: got %q", re.DefaultCollation())
	}
	// The database still references unicode (per-file view) — resolved from a loaded bundle.
	refs := re.Collations()
	if len(refs) != 1 || refs[0].Name != "unicode" || refs[0].UnicodeVersion != "17.0.0" || !refs[0].IsDefault {
		t.Fatalf("reopened referenced set: got %+v", refs)
	}
	if got := texts(t, query(t, re, `SELECT name FROM t ORDER BY name`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened collated column: got %v", got)
	}
	// plain (un-annotated) inherited the default (unicode) at create → also unicode order.
	if got := texts(t, query(t, re, `SELECT plain FROM t ORDER BY plain`)); !eqStrings(got, []string{"a", "ä", "z"}) {
		t.Fatalf("reopened inherited column: got %v", got)
	}
	if err := re.Close(); err != nil {
		t.Fatalf("close re: %v", err)
	}
}

// ---- slice 2d: the graded version-skew verdict (spec/design/collation.md §12/§14) ----
// Skew has NO PostgreSQL analog (PG's collversion is the opposite, host-OS-drift, §15), so it is a
// documented PG divergence tested per-core, not in the oracle corpus (CLAUDE.md §10). White-box (it
// injects a file-pin/loaded-version mismatch a public API cannot manufacture — a fresh file pins the
// loaded version). Mirrors impl/rust skew_tests and impl/ts/tests/collation_host.test.ts.

func TestCollationVersionSkewVerdict(t *testing.T) {
	t.Parallel()
	// The pure verdict (VersionSkew) — the cross-core contract (every core computes the identical
	// result): same version ⇒ Full; a different pin ⇒ Skewed (reporting the loaded version); an
	// unloaded name ⇒ no skew verdict (the absent case is refused at open, not a skew).
	loadFixtureBundle(t)
	loaded := LoadedCollation("unicode")
	if loaded == nil {
		t.Fatal("unicode must be loaded")
	}
	if _, _, skewed := versionSkew("unicode", loaded.UnicodeVersion, loaded.CldrVersion); skewed {
		t.Fatal("same version must be Full")
	}
	lu, lc, skewed := versionSkew("unicode", "0.0.0", "0")
	if !skewed || lu != loaded.UnicodeVersion || lc != loaded.CldrVersion {
		t.Fatalf("a different pin must be Skewed reporting the loaded version: got (%q,%q,%v)", lu, lc, skewed)
	}
	if _, _, skewed := versionSkew("zz-not-loaded", "1", "1"); skewed {
		t.Fatal("an unloaded name must yield no skew verdict")
	}
}

func TestSkewedCollationBlocksWrites(t *testing.T) {
	t.Parallel()
	// A unicode-collated PK table is read-write while Full; once its unicode reference is pinned to a
	// different version than the loaded bundle (the open-time state of a file built under an older
	// bundle), the table degrades to read-only: reads still return the rows (the heap-scan fallback),
	// every write raises XX002, and the skew is legible via db.Collations.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		`CREATE TABLE t (x text COLLATE "unicode" PRIMARY KEY)`,
		`INSERT INTO t VALUES ('b'), ('a')`,
		`ANALYZE t (x)`,
		`INSERT INTO t VALUES ('c')`, // Full → succeeds
	} {
		if _, err := queryOutcome(db, sql, nil); err != nil {
			t.Fatalf("%s: unexpected error %v", sql, err)
		}
	}
	for _, c := range db.Collations() {
		if c.Verdict != verdictFull {
			t.Fatalf("%q must be Full before skew injection, got %v", c.Name, c.Verdict)
		}
	}

	// Inject skew: the file pinned unicode to an older version than the loaded bundle. This is exactly
	// the catalog state Open produces for a file built under a prior bundle (collation.md §5/§12).
	loaded := LoadedCollation("unicode")
	skewed := *loaded
	skewed.UnicodeVersion = "0.0.0"
	db.engine.committed.collations["unicode"] = &skewed

	// The verdict is now Skewed and visible via introspection (the file's pin is reported).
	found := false
	for _, c := range db.Collations() {
		if c.Name == "unicode" {
			found = true
			if c.Verdict != verdictSkewed {
				t.Fatalf("unicode verdict: got %v, want Skewed", c.Verdict)
			}
			if c.UnicodeVersion != "0.0.0" {
				t.Fatalf("unicode pin: got %q, want 0.0.0", c.UnicodeVersion)
			}
		}
	}
	if !found {
		t.Fatal("unicode must be referenced")
	}

	// Reads still work — all three rows come back (values are version-independent §4.1).
	out, err := queryOutcome(db, `SELECT x FROM t ORDER BY x COLLATE "unicode"`, nil)
	if err != nil {
		t.Fatalf("read after skew: %v", err)
	}
	if len(out.Rows) != 3 {
		t.Fatalf("read after skew: got %d rows, want 3", len(out.Rows))
	}

	// Every write is refused with XX002.
	for _, sql := range []string{
		`INSERT INTO t VALUES ('d')`,
		`UPDATE t SET x = 'z' WHERE x = 'a'`,
		`DELETE FROM t WHERE x = 'a'`,
		`CREATE INDEX t_x ON t (x)`,
	} {
		if _, err := queryOutcome(db, sql, nil); err == nil || err.(*EngineError).Code() != "XX002" {
			t.Fatalf("%s: want XX002, got %v", sql, err)
		}
	}
}

func TestUpgradeCollationsClearsSkew(t *testing.T) {
	t.Parallel()
	// The COLLATION UPGRADE migration (db.UpgradeCollations, collation.md §12) clears the skew: after
	// it the collation's pin is the loaded version, db.Collations reports Full, and the table is
	// read-write again. Asserts the internal state the shared corpus
	// (suites/collation/collation_upgrade.test) cannot read — the verdict-flip + the re-pin count —
	// plus idempotence. The skew injection mirrors TestSkewedCollationBlocksWrites.
	loadFixtureBundle(t)
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		`CREATE TABLE t (x text COLLATE "unicode" PRIMARY KEY)`,
		`INSERT INTO t VALUES ('b'), ('a')`,
		`ANALYZE t (x)`,
	} {
		if _, err := queryOutcome(db, sql, nil); err != nil {
			t.Fatalf("%s: unexpected error %v", sql, err)
		}
	}
	loaded := LoadedCollation("unicode")
	skewed := *loaded
	skewed.UnicodeVersion = "0.0.0"
	db.engine.committed.collations["unicode"] = &skewed
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatalf("serialize skewed statistics: %v", err)
	}
	loadedEngine, err := loadEngine(image)
	if err != nil {
		t.Fatalf("open skewed statistics: %v", err)
	}
	if loadedEngine.committed.columnStatistics("t", 0) == nil {
		t.Fatal("persisted statistics should remain structurally present")
	}
	if loadedEngine.columnStatisticsScoped(nil, "t", 0) != nil {
		t.Fatal("skewed statistics must be unavailable to the estimator")
	}
	db.engine = loadedEngine

	n, err := db.UpgradeCollations()
	if err != nil {
		t.Fatalf("upgrade: %v", err)
	}
	if n != 1 {
		t.Fatalf("re-pinned: got %d, want 1", n)
	}
	for _, c := range db.Collations() {
		if c.Name == "unicode" {
			if c.Verdict != verdictFull {
				t.Fatalf("unicode verdict after upgrade: got %v, want Full", c.Verdict)
			}
			if c.UnicodeVersion != loaded.UnicodeVersion {
				t.Fatalf("unicode pin after upgrade: got %q, want %q", c.UnicodeVersion, loaded.UnicodeVersion)
			}
		}
	}
	if db.engine.committed.columnStatistics("t", 0) != nil {
		t.Fatal("upgrade must clear facts ordered under the old collation")
	}
	if _, err := queryOutcome(db, `INSERT INTO t VALUES ('c')`, nil); err != nil {
		t.Fatalf("writable after upgrade: %v", err)
	}
	if n, err := db.UpgradeCollations(); err != nil || n != 0 {
		t.Fatalf("idempotent no-op: got (%d, %v), want (0, nil)", n, err)
	}
}

func TestCollationOpenRefusesAbsent(t *testing.T) {
	t.Parallel()
	// A file that references a collation NO loaded bundle provides is the graded verdict's legible
	// refusal (collation.md §12, slice 2d): decoding the reference entry fails with XX002 naming it +
	// its version, rather than the old bare 42704. "zz-absent-collation" is never in any bundle, so
	// this is independent of the engine-global loaded set (no bundle load needed).
	coll := &Collation{Name: "zz-absent-collation", UnicodeVersion: "17.0.0", CldrVersion: "48"}
	buf := collationEntryBytes(coll, false)
	pos := 0
	_, _, err := decodeCollationEntry(buf, &pos)
	if err == nil || err.(*EngineError).Code() != "XX002" {
		t.Fatalf("absent reference: want XX002, got %v", err)
	}
	if !strings.Contains(err.Error(), "zz-absent-collation") || !strings.Contains(err.Error(), "17.0.0/48") {
		t.Fatalf("message should name the collation + version: %v", err)
	}
}
