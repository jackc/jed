// Collation host API (spec/design/collation.md §1/§4): db.importCollation (1c) plus the slice-1d
// host surface — exportCollation, setDefaultCollation / defaultCollation, collations, per-database
// default inheritance, and the baked file round-trip (format_version 17, entry_kind 3). These are the
// host-API + persistence behaviors the conformance corpus cannot express (CLAUDE.md §10); the
// in-memory SQL behavior lives in suites/collation/collate.test. Mirrors
// impl/rust/tests/collation_host.rs and impl/go/collation_host_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { close, commit, create, Database, execute, open } from "../src/lib.ts";
import { type Collation, compileCollation } from "../src/collation.ts";
import { specPath } from "./tomlmini.ts";
import { errCode, query } from "./util.ts";

// exec runs a statement (DDL / INSERT) whose outcome is not a query result.
function exec(db: Database, sql: string): void {
  execute(db, sql);
}

function devRoot(): Collation {
  return compileCollation(
    "dev-root",
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8"),
  );
}

// A collation under the name "dev-root" but with the dev-nordic table (a different content hash) —
// the conflicting import.
function devRootNamedButNordicTable(): Collation {
  const def =
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8") +
    "\n" +
    readFileSync(specPath("collation/fixtures/dev-nordic.ldml"), "utf8");
  return compileCollation("dev-root", def);
}

test("importCollation then use in a query", () => {
  const db = new Database();
  assert.equal(db.importCollation(devRoot()), "dev-root");
  // The imported collation is usable by name: 'ä' < 'z' is true under dev-root (ä near a), the
  // opposite of the C byte order where it is false.
  assert.deepEqual(query(db, `SELECT 'ä' < 'z' COLLATE "dev-root"`), [["true"]]);
});

test("importCollation is idempotent by name and hash", () => {
  const db = new Database();
  db.importCollation(devRoot());
  // Re-importing the identical (name, content) collation is a no-op success.
  assert.equal(db.importCollation(devRoot()), "dev-root");
});

test("importCollation conflict (same name, different table) is 42710", () => {
  const db = new Database();
  db.importCollation(devRoot());
  // A DIFFERENT table under a name already in use is a conflict (collation.md §4).
  assert.equal(
    errCode(() => db.importCollation(devRootNamedButNordicTable())),
    "42710",
  );
});

test("importing C is rejected", () => {
  const db = new Database();
  // C is table-free and built in; it is never imported (collation.md §4).
  const c = compileCollation(
    "C",
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8"),
  );
  assert.equal(
    errCode(() => db.importCollation(c)),
    "42710",
  );
});

// ---- slice 1d ----

function devNordic(): Collation {
  const def =
    readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8") +
    "\n" +
    readFileSync(specPath("collation/fixtures/dev-nordic.ldml"), "utf8");
  return compileCollation("dev-nordic", def);
}

test("per-column collation orders implicitly", () => {
  const db = new Database();
  db.importCollation(devRoot());
  exec(db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root")`);
  exec(db, `INSERT INTO t VALUES (1,'z'),(2,'ä'),(3,'a')`);
  // No explicit COLLATE: name sorts by its frozen dev-root collation (ä next to a).
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name`), [["a"], ["ä"], ["z"]]);
  // An explicit COLLATE "C" overrides back to byte order (ä is 2-byte UTF-8 → after z).
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name COLLATE "C"`), [
    ["a"],
    ["z"],
    ["ä"],
  ]);
});

test("implicit conflict is 42P22", () => {
  const db = new Database();
  db.importCollation(devRoot());
  db.importCollation(devNordic());
  exec(
    db,
    `CREATE TABLE t (a text COLLATE "dev-root", b text COLLATE "dev-nordic", c text COLLATE "C")`,
  );
  exec(db, `INSERT INTO t VALUES ('a','z','b')`);
  assert.equal(errCode(() => query(db, `SELECT a < b FROM t`)), "42P22");
  assert.equal(errCode(() => query(db, `SELECT a < c FROM t`)), "42P22");
  // An explicit COLLATE on one side breaks the tie: a='a' < (b='z') = true.
  assert.deepEqual(query(db, `SELECT a < b COLLATE "dev-root" FROM t`), [["true"]]);
});

test("COLLATE column errors (non-text 42804, unknown 42704)", () => {
  const db = new Database();
  db.importCollation(devRoot());
  assert.equal(
    errCode(() => exec(db, `CREATE TABLE t (a i32 COLLATE "dev-root")`)),
    "42804",
  );
  assert.equal(
    errCode(() => exec(db, `CREATE TABLE t (a text COLLATE "nope")`)),
    "42704",
  );
});

test("per-database default collation inheritance", () => {
  const db = new Database();
  db.importCollation(devRoot());
  assert.equal(db.defaultCollation(), "C");
  exec(db, `CREATE TABLE before (id i32 PRIMARY KEY, name text)`);
  db.setDefaultCollation("dev-root");
  assert.equal(db.defaultCollation(), "dev-root");
  exec(db, `CREATE TABLE after (id i32 PRIMARY KEY, name text)`);
  exec(db, `INSERT INTO after VALUES (1,'z'),(2,'ä'),(3,'a')`);
  assert.deepEqual(query(db, `SELECT name FROM after ORDER BY name`), [["a"], ["ä"], ["z"]]);
  exec(db, `INSERT INTO before VALUES (1,'z'),(2,'ä'),(3,'a')`);
  assert.deepEqual(query(db, `SELECT name FROM before ORDER BY name`), [["a"], ["z"], ["ä"]]);
  assert.equal(errCode(() => db.setDefaultCollation("nope")), "42704");
  db.setDefaultCollation("C");
});

test("export round-trips and introspects", () => {
  const db = new Database();
  db.importCollation(devRoot());
  const exported = db.exportCollation("dev-root");
  assert.equal(exported.name, "dev-root");
  const db2 = new Database();
  assert.equal(db2.importCollation(exported), "dev-root");
  assert.equal(errCode(() => db.exportCollation("nope")), "42704");
  assert.equal(errCode(() => db.exportCollation("C")), "42704");
  db.setDefaultCollation("dev-root");
  const infos = db.collations();
  assert.equal(infos.length, 1);
  assert.equal(infos[0]!.name, "dev-root");
  assert.equal(infos[0]!.isDefault, true);
});

test("baked file round trip (format_version 17, entry_kind 3)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-coll-"));
  const path = join(dir, "collation_baked.jed");
  try {
    const db = create(path, { pageSize: 256 });
    db.importCollation(devRoot());
    db.setDefaultCollation("dev-root");
    exec(db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root", plain text)`);
    exec(db, `INSERT INTO t VALUES (1,'z','z'),(2,'ä','ä'),(3,'a','a')`);
    commit(db);
    close(db);

    const re = open(path);
    assert.equal(re.defaultCollation(), "dev-root");
    assert.equal(re.collations().length, 1);
    assert.deepEqual(query(re, `SELECT name FROM t ORDER BY name`), [["a"], ["ä"], ["z"]]);
    // plain (un-annotated) inherited the default (dev-root) at create.
    assert.deepEqual(query(re, `SELECT plain FROM t ORDER BY plain`), [["a"], ["ä"], ["z"]]);
    close(re);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// slice 1e — a collated text PRIMARY KEY's storage key is the UCA sort key (encoding.md §2.12), so
// the B-tree physically iterates in COLLATION order. A no-ORDER-BY single-table scan returns jed's
// stored (key) order, so this asserts the *key* is collated (distinct from the in-memory ORDER BY
// sorter 1c had). dev-root: a < A < b < Z; C bytes: A < Z < a < b.
test("collated primary key stored in collation order", () => {
  const db = new Database();
  db.importCollation(devRoot());
  exec(db, `CREATE TABLE t (name text COLLATE "dev-root" PRIMARY KEY)`);
  exec(db, `INSERT INTO t VALUES ('Z'),('a'),('b'),('A')`);
  assert.deepEqual(query(db, `SELECT name FROM t`), [["a"], ["A"], ["b"], ["Z"]]);
  exec(db, `CREATE TABLE c (name text PRIMARY KEY)`);
  exec(db, `INSERT INTO c VALUES ('Z'),('a'),('b'),('A')`);
  assert.deepEqual(query(db, `SELECT name FROM c`), [["A"], ["Z"], ["a"], ["b"]]);
});

// slice 1e — a collated UNIQUE key dedups by byte-identity (a deterministic collation: 'a' and 'A'
// are DISTINCT, both admitted — collation.md §7), like a C unique key.
test("collated secondary index and unique keys", () => {
  const db = new Database();
  db.importCollation(devRoot());
  exec(
    db,
    `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root" UNIQUE)`,
  );
  exec(db, `INSERT INTO t VALUES (1,'a'),(2,'A'),(3,'b')`);
  assert.equal(errCode(() => exec(db, `INSERT INTO t VALUES (4,'a')`)), "23505");
  assert.deepEqual(query(db, `SELECT name FROM t ORDER BY name`), [
    ["a"],
    ["A"],
    ["b"],
  ]);
});
