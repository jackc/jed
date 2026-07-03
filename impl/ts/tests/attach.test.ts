// Host-attached in-memory databases — the Database.attach/detach host API (spec/design/attached-
// databases.md §4/§6, Slice 1b). These are the behaviors the shared corpus CANNOT express (it is
// single-handle SQL-in/rows-out and cannot call db.attach — CLAUDE.md §10): the attach/detach
// lifecycle, the read-only write-rejection (25006), detach-in-use (55006), reserved/duplicate names
// (42710), unknown detach (42704), and the file-source deferral (0A000). The SQL routing itself lives
// in the corpus (suites/attach/in_memory.test). Mirrors impl/go/attach_test.go and
// impl/rust/tests/attach.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { attachFile, attachMemory, type Database, EngineError, type Session } from "../src/lib.ts";
import { memDb } from "./mem_db.ts";

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

// TestAttachFileSourceDeferred — a FILE-backed attach source is Slice 2; Database.attach with a file
// source throws 0A000 now, so the host-API signature never changes when file attach lands. This is
// also the one-durable-writer guard's inert form in 1b (no writable file attachment can exist yet).
test("file attach source is deferred (0A000)", () => {
  const db = memDb();
  assert.throws(
    () => db.attach("f", attachFile("/tmp/whatever.jed"), false),
    (e: unknown) => e instanceof EngineError && e.code() === "0A000",
  );
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
