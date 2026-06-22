// Collation host API + persistence (spec/design/collation.md §1/§4.2): the host-loaded surface —
// loadUnicodeData (the JUCD bundle load seam), setDefaultCollation / defaultCollation, the per-file
// db.collations() (what the database REFERENCES) vs the engine-global db.loadedCollations() (what a
// loaded bundle PROVIDES), per-column / per-database default inheritance, collated keys, and the
// reference-only FILE ROUND-TRIP (format_version 18, entry_kind 3 metadata entries). These are the
// host-API + persistence behaviors the conformance corpus cannot express (CLAUDE.md §10); the
// in-memory SQL behavior a collation drives (COLLATE / ORDER BY / derivation / 42P21 / 42P22) lives in
// suites/collation/collate.test, which runs on every core. There is NO importCollation: the bare
// binary carries no Unicode data and the host loads jed's own pinned bundle bytes (the SQLite model,
// §9/§16), then uses collations by name. Mirrors impl/rust/tests/collation_host.rs and
// impl/go/collation_host_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadedCollation } from "../src/collation.ts";
import { close, commit, create, Database, execute, loadUnicodeData, open } from "../src/lib.ts";
import { specPath } from "./tomlmini.ts";
import { errCode, query } from "./util.ts";

// exec runs a statement (DDL / INSERT) whose outcome is not a query result.
function exec(db: Database, sql: string): void {
  execute(db, sql);
}

// Load jed's pinned production JUCD bundle (spec/collation/fixtures/unicode.jucd) into the
// engine-global loaded set — what a production host does once via db.loadUnicodeData before opening
// files / running collated queries (collation.md §4). Idempotent (global, first-wins).
function loadFixtureBundle(): void {
  loadUnicodeData(readFileSync(specPath("collation/fixtures/unicode.jucd")));
}

// ---- the loaded set (the engine-global property a bundle provides) ----

test("loadedCollations is the real set", () => {
  // db.loadedCollations() reports what a loaded bundle PROVIDES — after loading jed's pinned production
  // bundle, the real version-pinned set (es, unicode), ascending by name, no isDefault (an engine
  // property, not a per-db one). C is built in and never listed. The pin is UCA/UCD 17.0.0.
  loadFixtureBundle();
  const v = new Database().loadedCollations();
  assert.deepEqual(
    v.map((c) => c.name),
    ["es", "unicode"],
  );
  assert.ok(v.every((c) => !c.isDefault));
  assert.equal(v[1]!.name, "unicode");
  assert.equal(v[1]!.unicodeVersion, "17.0.0");
  assert.notEqual(loadedCollation("unicode"), undefined);
  assert.notEqual(loadedCollation("es"), undefined);
  assert.equal(loadedCollation("C"), undefined);
});

// ---- using a loaded collation needs NO import ----

test("loaded collation used in an expression", () => {
  // COLLATE "unicode" resolves from the engine's loaded set with no import: 'ä' < 'z' is true under
  // the root (ä near a), the opposite of the C byte order where it is false. A transient query COLLATE
  // does not make the database REFERENCE the collation, so db.collations() stays empty.
  loadFixtureBundle();
  const db = new Database();
  assert.equal(db.collations().length, 0);
  assert.deepEqual(query(db, `SELECT 'ä' < 'z' COLLATE "unicode"`), [["true"]]);
});

test("es orders ñ as a distinct letter", () => {
  // The es tailoring (&N<ñ<<<Ñ) makes ñ a distinct PRIMARY letter after n: 'nz' < 'ña' (n < ñ),
  // whereas under the untailored root ñ is n+accent so 'ña' < 'nz'. The Spanish-collation headline.
  loadFixtureBundle();
  const db = new Database();
  assert.deepEqual(query(db, `SELECT 'nz' < 'ña' COLLATE "es"`), [["true"]]);
  assert.deepEqual(query(db, `SELECT 'nz' < 'ña' COLLATE "unicode"`), [["false"]]);
});

test("unknown collation is 42704", () => {
  // A collation neither loaded nor referenced is 42704 (the loaded-set fallback must not mask it).
  loadFixtureBundle();
  const db = new Database();
  assert.equal(
    errCode(() => query(db, `SELECT 'x' COLLATE "no-such-collation"`)),
    "42704",
  );
});

test("per-column collation orders implicitly and is referenced", () => {
  // A column declared COLLATE "unicode" (loaded, no import) sorts by that collation with no explicit
  // COLLATE on the query — unicode puts ä next to a. Because the SCHEMA now references unicode,
  // db.collations() (the per-file view) lists exactly it.
  loadFixtureBundle();
  const db = new Database();
  exec(db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode")`);
  exec(db, `INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')`);
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name`), [["a"], ["ä"], ["z"]]);
  const refs = db.collations();
  assert.equal(refs.length, 1);
  assert.equal(refs[0]!.name, "unicode");
  assert.equal(refs[0]!.isDefault, false); // referenced by a column, but not the db default
  // An explicit COLLATE "C" on the query overrides back to byte order (ä is 2-byte UTF-8 → after z).
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name COLLATE "C"`), [
    ["a"],
    ["z"],
    ["ä"],
  ]);
});

test("implicit conflict is 42P22", () => {
  // Two columns with DIFFERENT implicit (loaded) collations compared with no explicit COLLATE →
  // 42P22 (PG-matching). C counts as a distinct implicit collation, so unicode vs C also conflicts.
  loadFixtureBundle();
  const db = new Database();
  exec(db, `CREATE TABLE t (a text COLLATE "unicode", b text COLLATE "es", c text COLLATE "C")`);
  exec(db, `INSERT INTO t VALUES ('a','z','b')`);
  assert.equal(
    errCode(() => query(db, `SELECT a < b FROM t`)),
    "42P22",
  );
  assert.equal(
    errCode(() => query(db, `SELECT a < c FROM t`)),
    "42P22",
  );
  // An explicit COLLATE on one side breaks the tie (no error): a='a' < (b='z') = true.
  assert.deepEqual(query(db, `SELECT a < b COLLATE "unicode" FROM t`), [["true"]]);
  // The table references both loaded collations → db.collations() lists them (sorted).
  assert.deepEqual(
    db.collations().map((c) => c.name),
    ["es", "unicode"],
  );
});

test("COLLATE column errors (non-text 42804, unknown 42704)", () => {
  loadFixtureBundle();
  const db = new Database();
  assert.equal(
    errCode(() => exec(db, `CREATE TABLE t (a i32 COLLATE "unicode")`)),
    "42804",
  );
  assert.equal(
    errCode(() => exec(db, `CREATE TABLE t (a text COLLATE "nope")`)),
    "42704",
  );
});

// ---- the per-database default (over the loaded set, no import) ----

test("default collation inherited by unannotated column", () => {
  // setDefaultCollation moves the per-database default to a LOADED collation (no import); an
  // un-annotated text column created AFTER inherits it (frozen), one created BEFORE keeps C.
  loadFixtureBundle();
  const db = new Database();
  assert.equal(db.defaultCollation(), "C");
  exec(db, `CREATE TABLE before (id i32 PRIMARY KEY, name text)`);
  db.setDefaultCollation("unicode");
  assert.equal(db.defaultCollation(), "unicode");
  exec(db, `CREATE TABLE after (id i32 PRIMARY KEY, name text)`);
  exec(db, `INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')`);
  // after.name inherited unicode → ä sorts next to a even with no COLLATE clause.
  assert.deepEqual(query(db, `SELECT name FROM after ORDER BY name`), [["a"], ["ä"], ["z"]]);
  // before.name was frozen at C → byte order.
  exec(db, `INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')`);
  assert.deepEqual(query(db, `SELECT name FROM before ORDER BY name`), [["a"], ["z"], ["ä"]]);
  // The default makes unicode referenced (isDefault true).
  const refs = db.collations();
  assert.equal(refs.length, 1);
  assert.equal(refs[0]!.isDefault, true);
});

test("set default unknown is 42704", () => {
  loadFixtureBundle();
  const db = new Database();
  assert.equal(
    errCode(() => db.setDefaultCollation("nope")),
    "42704",
  );
  db.setDefaultCollation("C"); // C always resolves (resets to byte order)
});

// ---- collated keys (slice 1e, on-disk/internal — the corpus cannot express it) ----

test("collated primary key stored in collation order", () => {
  // A collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so the B-tree
  // physically iterates in COLLATION order. unicode (loaded, no import): a < A < b < Z; C bytes:
  // A < Z < a < b. A no-ORDER-BY single-table scan returns jed's stored (key) order.
  loadFixtureBundle();
  const db = new Database();
  exec(db, `CREATE TABLE t (name text COLLATE "unicode" PRIMARY KEY)`);
  exec(db, `INSERT INTO t VALUES ('Z'),('a'),('b'),('A')`);
  assert.deepEqual(query(db, `SELECT name FROM t`), [["a"], ["A"], ["b"], ["Z"]]);
  exec(db, `CREATE TABLE c (name text PRIMARY KEY)`);
  exec(db, `INSERT INTO c VALUES ('Z'),('a'),('b'),('A')`);
  assert.deepEqual(query(db, `SELECT name FROM c`), [["A"], ["Z"], ["a"], ["b"]]);
});

test("collated unique dedups by byte identity", () => {
  // A collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A' are
  // DISTINCT, both admitted — collation.md §7), like a C unique key; only a byte-duplicate violates.
  loadFixtureBundle();
  const db = new Database();
  exec(db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode" UNIQUE)`);
  exec(db, `INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')`);
  assert.equal(
    errCode(() => exec(db, `INSERT INTO t VALUES (4,'a')`)),
    "23505",
  );
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name`), [["a"], ["A"], ["b"]]);
});

// ---- reference-only file round-trip (format_version 18) ----

test("reference-only file round trip (format_version 18, entry_kind 3)", () => {
  // A collated table + the per-database default survive a close + paged reopen. The file stores only a
  // metadata REFERENCE entry (no table); on reopen the table is resolved from a loaded bundle (the host
  // must have loaded one providing it BEFORE open — collation.md §4/§9).
  loadFixtureBundle();
  const dir = mkdtempSync(join(tmpdir(), "jed-coll-"));
  const path = join(dir, "collation_refonly_roundtrip.jed");
  try {
    const db = create(path, { pageSize: 256 });
    db.setDefaultCollation("unicode"); // loaded — no import
    exec(db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "unicode", plain text)`);
    exec(db, `INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')`);
    commit(db);
    close(db);

    const re = open(path);
    assert.equal(re.defaultCollation(), "unicode");
    // The database still references unicode (per-file view) — resolved from a loaded bundle.
    const refs = re.collations();
    assert.equal(refs.length, 1);
    assert.equal(refs[0]!.name, "unicode");
    assert.equal(refs[0]!.unicodeVersion, "17.0.0");
    assert.equal(refs[0]!.isDefault, true);
    assert.deepEqual(query(re, `SELECT name FROM t ORDER BY name`), [["a"], ["ä"], ["z"]]);
    // plain (un-annotated) inherited the default (unicode) at create → also unicode order.
    assert.deepEqual(query(re, `SELECT plain FROM t ORDER BY plain`), [["a"], ["ä"], ["z"]]);
    close(re);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
