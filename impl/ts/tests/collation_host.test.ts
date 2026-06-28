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
import { loadedCollation, versionSkew } from "../src/collation.ts";
import { close, commit, create, Engine, execute, loadUnicodeData, open } from "../src/tooling.ts";
import { specPath } from "./tomlmini.ts";
import { errCode, query } from "./util.ts";

// exec runs a statement (DDL / INSERT) whose outcome is not a query result.
function exec(db: Engine, sql: string): void {
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
  const v = new Engine().loadedCollations();
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
  const db = new Engine();
  assert.equal(db.collations().length, 0);
  assert.deepEqual(query(db, `SELECT 'ä' < 'z' COLLATE "unicode"`), [["true"]]);
});

test("es orders ñ as a distinct letter", () => {
  // The es tailoring (&N<ñ<<<Ñ) makes ñ a distinct PRIMARY letter after n: 'nz' < 'ña' (n < ñ),
  // whereas under the untailored root ñ is n+accent so 'ña' < 'nz'. The Spanish-collation headline.
  loadFixtureBundle();
  const db = new Engine();
  assert.deepEqual(query(db, `SELECT 'nz' < 'ña' COLLATE "es"`), [["true"]]);
  assert.deepEqual(query(db, `SELECT 'nz' < 'ña' COLLATE "unicode"`), [["false"]]);
});

test("unknown collation is 42704", () => {
  // A collation neither loaded nor referenced is 42704 (the loaded-set fallback must not mask it).
  loadFixtureBundle();
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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

// ---- slice 2d: the graded version-skew verdict (spec/design/collation.md §12/§14) ----
// Skew has NO PostgreSQL analog (PG's collversion is the opposite, host-OS-drift, §15), so it is a
// documented PG divergence tested per-core, not in the oracle corpus (CLAUDE.md §10). Mirrors
// impl/rust skew_tests and impl/go/collation_host_test.go. (The absent-at-open refusal — an entirely
// unloaded referenced collation → XX002 — is a structurally-identical 1-line decode change covered by
// the Rust + Go decode unit tests; TS does not export the internal entry codec, and the write-block
// below already exercises XX002 in this core.)

test("collation version-skew verdict (pure)", () => {
  // The pure verdict (versionSkew) — the cross-core contract (every core computes the identical
  // result): same version ⇒ undefined (Full); a different pin ⇒ the loaded version (Skewed); an
  // unloaded name ⇒ undefined (the absent case is refused at open, not a skew verdict).
  loadFixtureBundle();
  const loaded = loadedCollation("unicode");
  assert.notEqual(loaded, undefined);
  assert.equal(versionSkew("unicode", loaded!.unicodeVersion, loaded!.cldrVersion), undefined);
  assert.deepEqual(versionSkew("unicode", "0.0.0", "0"), [
    loaded!.unicodeVersion,
    loaded!.cldrVersion,
  ]);
  assert.equal(versionSkew("zz-not-loaded", "1", "1"), undefined);
});

test("a version-skewed collation blocks writes but reads still work", () => {
  // A unicode-collated PK table is read-write while Full; once its unicode reference is pinned to a
  // different version than the loaded bundle (the open-time state of a file built under an older
  // bundle), the table degrades to read-only: reads still return the rows (the heap-scan fallback),
  // every write raises XX002, and the skew is legible via db.collations().
  loadFixtureBundle();
  const db = new Engine();
  exec(db, `CREATE TABLE t (x text COLLATE "unicode" PRIMARY KEY)`);
  exec(db, `INSERT INTO t VALUES ('b'), ('a')`);
  exec(db, `INSERT INTO t VALUES ('c')`); // Full → succeeds
  assert.ok(db.collations().every((c) => c.verdict === "full"));

  // Inject skew: the file pinned unicode to an older version than the loaded bundle. This is exactly
  // the catalog state Engine.open produces for a file built under a prior bundle (collation.md
  // §5/§12). collations is a public Snapshot field; we clone so the engine-global loaded set is intact.
  const loaded = loadedCollation("unicode")!;
  db.committed.collations.set("unicode", { ...loaded, unicodeVersion: "0.0.0" });

  // The verdict is now Skewed and visible via introspection (the file's pin is reported).
  const uni = db.collations().find((c) => c.name === "unicode");
  assert.notEqual(uni, undefined);
  assert.equal(uni!.verdict, "skewed");
  assert.equal(uni!.unicodeVersion, "0.0.0");

  // Reads still work — all three rows come back (values are version-independent §4.1).
  assert.equal(query(db, `SELECT x FROM t ORDER BY x COLLATE "unicode"`).length, 3);

  // Every write is refused with XX002.
  for (const sql of [
    `INSERT INTO t VALUES ('d')`,
    `UPDATE t SET x = 'z' WHERE x = 'a'`,
    `DELETE FROM t WHERE x = 'a'`,
    `CREATE INDEX t_x ON t (x)`,
  ]) {
    assert.equal(
      errCode(() => exec(db, sql)),
      "XX002",
      sql,
    );
  }
});

test("upgradeCollations clears the skew", () => {
  // The COLLATION UPGRADE migration (db.upgradeCollations, collation.md §12) clears the skew: after
  // it the collation's pin is the loaded version, db.collations() reports Full, and the table is
  // read-write again. Asserts the internal state the shared corpus
  // (suites/collation/collation_upgrade.test) cannot read — the verdict-flip + the re-pin count —
  // plus idempotence. The skew injection mirrors the test above.
  loadFixtureBundle();
  const db = new Engine();
  exec(db, `CREATE TABLE t (x text COLLATE "unicode" PRIMARY KEY)`);
  exec(db, `INSERT INTO t VALUES ('b'), ('a')`);
  const loaded = loadedCollation("unicode")!;
  db.committed.collations.set("unicode", { ...loaded, unicodeVersion: "0.0.0" });

  assert.equal(db.upgradeCollations(), 1); // one collation re-pinned
  const uni = db.collations().find((c) => c.name === "unicode");
  assert.equal(uni!.verdict, "full");
  assert.equal(uni!.unicodeVersion, loaded.unicodeVersion);
  exec(db, `INSERT INTO t VALUES ('c')`); // writable after upgrade
  assert.equal(db.upgradeCollations(), 0); // idempotent no-op
});
