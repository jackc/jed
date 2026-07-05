// Host-attached databases — the Database.attach/detach host API (spec/design/attached-databases.md
// §4/§6, Slices 1b + 2). These are the behaviors the shared corpus CANNOT express (it is single-handle
// SQL-in/rows-out and cannot call db.attach — CLAUDE.md §10): the attach/detach lifecycle, the read-only
// write-rejection (25006), detach-in-use (55006), reserved/duplicate names (42710), unknown detach
// (42704), and — for FILE attachments (Slice 2) — cross-file read/join, read-write durability across a
// standalone reopen, the one-durable-writer rule (0A000), page-size independence, and missing-file
// (58P01). The in-memory SQL routing lives in the corpus (suites/attach/in_memory.test); file durability
// / reopen is inherently a per-core host test (out of corpus reach). Mirrors impl/go/attach_test.go and
// impl/rust/tests/attach.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  attachFile,
  attachMemory,
  createDatabase,
  type Database,
  EngineError,
  openDatabase,
  type Session,
  type Value,
} from "../src/lib.ts";
import { memDb } from "./mem_db.ts";

// tmpDir makes a fresh scratch directory (never the repo tree), matching tests/api.test.ts.
function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-attach-"));
}

// makeFileDb creates a fresh single-file database at dir/name with page size `pageSize` (0 → default),
// runs each statement (autocommitting durably), and closes it — the reusable fixture for the file-attach
// tests (a self-describing jed file another handle can attach). Returns the path.
function makeFileDb(dir: string, name: string, pageSize: number, stmts: string[]): string {
  const path = join(dir, name);
  const db = createDatabase(
    pageSize === 0 ? { path, skipFsync: true } : { path, pageSize, skipFsync: true },
  );
  const s = db.session();
  for (const sql of stmts) s.execute(sql);
  s.close();
  db.close();
  return path;
}

// intCol collects a single-column bigint result from a session query, closing the cursor.
function intCol(s: Session, sql: string): bigint[] {
  const rows = s.query(sql);
  const out: bigint[] = [];
  for (const r of rows) {
    const v = r[0]!;
    out.push(v.kind === "int" ? v.int : -1n);
  }
  rows.close();
  return out;
}

// textCol collects a single-column text result from a session query, closing the cursor.
function textCol(s: Session, sql: string): string[] {
  const rows = s.query(sql);
  const out: string[] = [];
  for (const r of rows) {
    const v: Value = r[0]!;
    out.push(v.kind === "text" ? v.text : "");
  }
  rows.close();
  return out;
}

// errCode runs a statement expected to fail and returns its SQLSTATE.
function errCode(s: Session, sql: string): string {
  try {
    s.execute(sql);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error(`${sql}: expected an error, got none`);
}

// attachErrCode runs db.attach expected to fail and returns its SQLSTATE.
function attachErrCode(db: Database, name: string): string {
  try {
    db.attach(name, attachMemory(), false);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error(`attach ${name}: expected an error, got none`);
}

// detachErrCode runs db.detach expected to fail and returns its SQLSTATE.
function detachErrCode(db: Database, name: string): string {
  try {
    db.detach(name);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error(`detach ${name}: expected an error, got none`);
}

// TestAttachLifecycle drives the whole single-handle arc: attach an in-memory database, create +
// populate a table in it by qualifier, read it back, then detach it (making it unreachable again).
test("attach lifecycle: create, populate, read, detach", () => {
  const db = memDb();
  db.attach("mydb", attachMemory(), false);
  const s = db.session();
  s.execute("CREATE TABLE mydb.t (id i32 PRIMARY KEY, v i32)");
  s.execute("INSERT INTO mydb.t VALUES (1, 10), (2, 20)");

  const rows = s.query("SELECT v FROM mydb.t ORDER BY id");
  const got: bigint[] = [];
  for (const r of rows) {
    const v = r[0]!;
    got.push(v.kind === "int" ? v.int : -1n);
  }
  rows.close(); // release the streaming cursor's reader pin (a live cursor would block a later detach)
  assert.deepEqual(got, [10n, 20n]);

  // The committed attachment change is visible to a freshly-minted session over the same handle (the
  // attached-roots publish, §5) — proves the commit published a new attached root. Drain + close the
  // cursor so it releases its reader pin (an undrained/open streaming cursor would hold the roots).
  const s2 = db.session();
  const r2 = s2.query("SELECT v FROM mydb.t WHERE id = 1");
  let n = 0;
  for (const _ of r2) n++;
  r2.close();
  assert.equal(n, 1);

  db.detach("mydb");
  // After detach the qualifier is unknown again (42P01).
  assert.equal(errCode(db.session(), "SELECT v FROM mydb.t"), "42P01");
});

// TestAttachReadOnlyRejectsWrites — a read-only attachment rejects every write (DML + DDL) with 25006
// before any I/O (attached-databases.md §4), while a bare/main write is unaffected.
test("read-only attachment rejects writes (25006)", () => {
  const db = memDb();
  db.attach("ro", attachMemory(), true);
  const s = db.session();
  for (const sql of [
    "CREATE TABLE ro.t (id i32 PRIMARY KEY)",
    "CREATE INDEX ix ON ro.t (id)",
    "INSERT INTO ro.t VALUES (1)",
    "UPDATE ro.t SET id = 2",
    "DELETE FROM ro.t",
  ]) {
    assert.equal(errCode(s, sql), "25006", sql);
  }
  // A write to main is unaffected by a read-only attachment elsewhere.
  s.execute("CREATE TABLE keep (id i32 PRIMARY KEY)");
});

// TestDetachInUseIs55006 — detaching while a live reader session pins the committed roots is 55006
// (object_in_use); once the reader closes, the detach succeeds (attached-databases.md §4/§5, the
// reader-liveness watermark — a reader pins the whole roots, so it pins every attachment).
test("detach while a reader is live is 55006", () => {
  const db = memDb();
  db.attach("mydb", attachMemory(), false);
  const reader = db.readSession(); // pins the committed roots in the live registry
  assert.equal(detachErrCode(db, "mydb"), "55006");
  reader.close(); // drains the pin
  db.detach("mydb"); // now succeeds
});

// TestAttachNameErrors — a reserved name (main/temp) or an already-attached name is 42710; detaching
// an unknown / reserved database is 42704. Both are case-insensitive.
test("reserved / duplicate attach names are 42710; unknown detach is 42704", () => {
  const db = memDb();
  db.attach("mydb", attachMemory(), false);
  for (const name of ["main", "temp", "MAIN", "Temp", "mydb", "MyDB"]) {
    assert.equal(attachErrCode(db, name), "42710", `attach ${name}`);
  }
  for (const name of ["nope", "main", "temp"]) {
    assert.equal(detachErrCode(db, name), "42704", `detach ${name}`);
  }
});

// Attach an existing file database read-only, join a local table against it, and confirm every write to
// it is 25006 (the natural reference-database mode, attached-databases.md §4, Slice 2). Reads fault the
// attached file's pages through its own pager.
test("file attach read-only: cross-file join + 25006 writes", () => {
  const dir = tmpDir();
  try {
    const ref = makeFileDb(dir, "ref.jed", 0, [
      "CREATE TABLE city (id i32 PRIMARY KEY, name text)",
      "INSERT INTO city VALUES (1, 'Ada'), (2, 'Bos')",
    ]);
    const db = memDb();
    db.attach("ref", attachFile(ref), true);
    const s = db.session();
    s.execute("CREATE TABLE visit (city_id i32 PRIMARY KEY, n i32)");
    s.execute("INSERT INTO visit VALUES (1, 7), (2, 9)");

    // A cross-FILE join: local `visit` against the read-only attached file's `city`.
    assert.deepEqual(
      textCol(s, "SELECT c.name FROM visit v JOIN ref.city c ON c.id = v.city_id ORDER BY c.id"),
      ["Ada", "Bos"],
    );
    // Every write to the read-only attachment is 25006, before any I/O.
    for (const sql of [
      "CREATE TABLE ref.t (id i32 PRIMARY KEY)",
      "INSERT INTO ref.city VALUES (3, 'Cai')",
      "UPDATE ref.city SET name = 'x'",
      "DELETE FROM ref.city",
    ]) {
      assert.equal(errCode(s, sql), "25006", sql);
    }
    s.close();
    db.detach("ref");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Attach a file read-write, create+populate a table in it by qualifier, detach, then open that file
// STANDALONE and confirm the writes are durable (attached-databases.md §5 — a file attachment commits
// durably through its own pager + alternating meta slot + fsync).
test("file attach read-write persists across a standalone reopen", () => {
  const dir = tmpDir();
  try {
    const work = makeFileDb(dir, "work.jed", 0, []); // an empty writable file to attach
    const db = memDb();
    db.attach("work", attachFile(work), false);
    const s = db.session();
    s.execute("CREATE TABLE work.acct (id i32 PRIMARY KEY, bal i32)");
    s.execute("INSERT INTO work.acct VALUES (1, 100), (2, 200)");
    s.execute("CREATE INDEX acct_bal ON work.acct (bal)");
    s.close();
    db.detach("work");
    db.close();

    // Reopen the attached file on its own — the rows + index must be there (durable + self-describing).
    const reopened = openDatabase(work, { skipFsync: true });
    const rs = reopened.session();
    assert.deepEqual(intCol(rs, "SELECT bal FROM acct ORDER BY id"), [100n, 200n]);
    rs.close();
    const tbl = reopened.table("acct")!;
    assert.equal(tbl.indexes.length, 1);
    assert.equal(tbl.indexes[0]!.name, "acct_bal");
    reopened.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// A transaction may write at most one FILE-backed database (§5). With a FILE main and a read-write FILE
// attachment, a block that writes BOTH is 0A000 at COMMIT and commits nothing; writing either one alone
// succeeds. In-memory attachments never count against the slot.
test("file attach: one durable writer per transaction (0A000)", () => {
  const dir = tmpDir();
  try {
    const mainPath = makeFileDb(dir, "main.jed", 0, ["CREATE TABLE m (id i32 PRIMARY KEY)"]);
    const extra = makeFileDb(dir, "extra.jed", 0, ["CREATE TABLE e (id i32 PRIMARY KEY)"]);

    const db = openDatabase(mainPath, { skipFsync: true });
    db.attach("extra", attachFile(extra), false);
    const s = db.session();
    s.begin(true);
    s.execute("INSERT INTO m VALUES (1)"); // main (file) dirtied
    s.execute("INSERT INTO extra.e VALUES (1)"); // a SECOND durable (file) database dirtied
    assert.throws(
      () => s.commit(),
      (e: unknown) => e instanceof EngineError && e.code() === "0A000",
    );
    // Nothing was committed — both files are still empty of the attempted rows.
    assert.deepEqual(intCol(s, "SELECT count(*) FROM m"), [0n]);
    assert.deepEqual(intCol(s, "SELECT count(*) FROM extra.e"), [0n]);

    // Writing each durable database ALONE (its own autocommit statement) is fine.
    s.execute("INSERT INTO m VALUES (2)");
    s.execute("INSERT INTO extra.e VALUES (2)");
    assert.deepEqual(intCol(s, "SELECT id FROM m"), [2n]);
    assert.deepEqual(intCol(s, "SELECT id FROM extra.e"), [2n]);
    s.close();
    db.detach("extra");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The slot counts only FILE databases: an IN-MEMORY main plus a read-write FILE attachment is ONE
// durable writer, so a block writing both commits cleanly (§5).
test("file attach: memory main + one file attachment commits together", () => {
  const dir = tmpDir();
  try {
    const work = makeFileDb(dir, "work.jed", 0, ["CREATE TABLE w (id i32 PRIMARY KEY)"]);
    const db = memDb(); // in-memory main — not durable
    db.attach("work", attachFile(work), false);
    const s = db.session();
    s.execute("CREATE TABLE local (id i32 PRIMARY KEY)");
    s.begin(true);
    s.execute("INSERT INTO local VALUES (1)"); // in-memory main (free)
    s.execute("INSERT INTO work.w VALUES (1)"); // the one durable writer
    s.commit();
    assert.deepEqual(intCol(s, "SELECT id FROM work.w"), [1n]);
    s.close();
    db.detach("work");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// An attached file keeps its OWN page space (§2): attaching a file created at a non-default page size
// and writing into it serializes at THAT page size, verified by a standalone reopen. Guards the CREATE
// TABLE / CREATE INDEX page-size routing (attachPageSize).
test("file attach: attachment keeps its own page size", () => {
  const dir = tmpDir();
  try {
    const small = makeFileDb(dir, "small.jed", 256, []); // a 256-byte-page file, unlike the default main
    const db = memDb();
    db.attach("small", attachFile(small), false);
    const s = db.session();
    s.execute("CREATE TABLE small.grid (id i32 PRIMARY KEY, v i32)");
    // Enough rows to force at least one leaf split at the small page size (its own page space).
    for (let i = 1; i <= 40; i++) s.execute(`INSERT INTO small.grid VALUES (${i}, ${i * i})`);
    s.close();
    db.detach("small");
    db.close();

    const reopened = openDatabase(small, { skipFsync: true });
    assert.equal(reopened.pageSize, 256);
    const rs = reopened.session();
    // sum of i*i for i in 1..=40 = 22140.
    assert.deepEqual(intCol(rs, "SELECT count(*) FROM grid"), [40n]);
    assert.deepEqual(intCol(rs, "SELECT sum(v) FROM grid"), [22140n]);
    rs.close();
    reopened.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Detaching a file releases it, so the same file can be attached again.
test("file attach: detach releases, re-attach works", () => {
  const dir = tmpDir();
  try {
    const ref = makeFileDb(dir, "ref.jed", 0, [
      "CREATE TABLE t (id i32 PRIMARY KEY)",
      "INSERT INTO t VALUES (1)",
    ]);
    const db = memDb();
    for (let i = 0; i < 3; i++) {
      db.attach("ref", attachFile(ref), true);
      const s = db.session();
      assert.deepEqual(intCol(s, "SELECT id FROM ref.t"), [1n], `attach #${i}`);
      s.close();
      db.detach("ref");
    }
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Attaching a nonexistent file surfaces the same host/file code as opening main (§11 / hosts.md §4); the
// failed attach leaves no registry entry.
test("file attach: missing file is 58P01, leaves the name free", () => {
  const dir = tmpDir();
  try {
    const db = memDb();
    assert.throws(
      () => db.attach("x", attachFile(join(dir, "nope.jed")), true),
      (e: unknown) => e instanceof EngineError && e.code() === "58P01",
    );
    // The name is free after the failed attach.
    db.attach("x", attachMemory(), false);
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// TestAttachCaseInsensitiveQualifier — an attachment is reached case-insensitively by its qualifier
// (unquoted identifiers fold to lower case), matching how main/temp resolve.
test("attachment qualifier is case-insensitive", () => {
  const db = memDb();
  db.attach("Reports", attachMemory(), false);
  const s = db.session();
  s.execute("CREATE TABLE reports.sales (id i32 PRIMARY KEY)");
  s.execute("INSERT INTO REPORTS.sales VALUES (1)");
  const rows = s.query("SELECT id FROM Reports.sales");
  let n = 0;
  for (const _ of rows) n++;
  rows.close();
  assert.equal(n, 1);
});
