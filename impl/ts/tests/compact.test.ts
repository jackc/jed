// Host compaction — Database.compact (spec/design/api.md §2.6). Compaction is a host-API act the SQL
// corpus cannot reach, so its contract lives here: the rewrite returns dead pages and equals the
// from-scratch image of the committed snapshot at the next version, rows/costs/prepared statements
// survive it, a reopen sees it, and every precondition fails 42704/25006/0A000/55006 without writing.
// Cross-process presence (another process holding the file) is covered by the shared process corpus
// (spec/conformance/process/compact.process.toml). Mirrors impl/rust/tests/compact.rs and
// impl/go/compact_test.go.

import assert from "node:assert/strict";
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import {
  attachFile,
  attachMemory,
  createDatabase,
  Database,
  intValue,
  openDatabase,
  type Value,
} from "../src/lib.ts";
import { createOpfsWithHandle } from "../src/opfs.ts";
import type { SyncAccessHandle } from "../src/opfsblockstore.ts";
import { errCode } from "./util.ts";

const PAGE = 4096;

// tmp is a fresh path in this run's scratch directory (never the repo tree). The directory is removed
// whole after the last test, taking each database's `<path>.lock/` coordination bundle
// (spec/design/locking.md) with it.
const scratch = mkdtempSync(join(tmpdir(), "jed-compact-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function tmp(name: string): string {
  return join(scratch, name);
}

function createFile(path: string): Database {
  return createDatabase({ path, pageSize: PAGE, skipFsync: true });
}

// growAndShrink grows table t to 2000 wide rows, then deletes all but 100: most pages end up dead.
function growAndShrink(db: Database): void {
  const s = db.session();
  s.execute("CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)");
  s.execute("CREATE INDEX t_n ON t (n)");
  s.execute("INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g");
  s.execute("DELETE FROM t WHERE id > 100");
  s.close();
}

// rowsAndCost is the rows and the cost of sql.
function rowsAndCost(db: Database, sql: string): { rows: Value[][]; cost: bigint } {
  const s = db.session();
  try {
    const rows = s.query(sql);
    try {
      const collected = [...rows];
      return { rows: collected, cost: rows.cost };
    } finally {
      rows.close();
    }
  } finally {
    s.close();
  }
}

function count(db: Database, sql: string): Value[][] {
  return rowsAndCost(db, sql).rows;
}

const PROBE = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

test("file compaction returns dead pages and keeps rows and costs", () => {
  const path = tmp("compact-file.jed");
  let db = createFile(path);
  growAndShrink(db);
  const beforePages = db.pageCount;
  const beforeVersion = db.version;
  const before = rowsAndCost(db, PROBE);
  const total = rowsAndCost(db, "SELECT count(*) FROM t");

  db.compact("main");

  const afterPages = db.pageCount;
  assert.ok(
    afterPages * 4 < beforePages,
    `compaction returns dead pages: ${beforePages} -> ${afterPages}`,
  );
  assert.equal(db.version, beforeVersion + 1n);
  assert.equal(
    statSync(path).size,
    afterPages * PAGE,
    "the file is exactly its image, with no preallocation slack",
  );
  assert.ok(!existsSync(path + ".jedtmp"));
  assert.deepEqual(rowsAndCost(db, PROBE), before, "rows and cost survive");
  assert.deepEqual(rowsAndCost(db, "SELECT count(*) FROM t"), total);

  // The compacted file is the from-scratch image of the committed snapshot at the new version.
  const image = db.toImage(PAGE, db.version);
  assert.deepEqual(new Uint8Array(readFileSync(path)), image);

  // Later commits build on it, and a reopen sees both.
  const s = db.session();
  s.execute("INSERT INTO t VALUES (5000, 'tail', 3)");
  s.close();
  db.close();
  db = openDatabase(path);
  const reopened = rowsAndCost(db, PROBE).rows;
  assert.equal(reopened.length, before.rows.length + 1);
  assert.deepEqual(reopened.slice(0, before.rows.length), before.rows);
  db.close();
});

test("compaction of a compact file is a fixed point", () => {
  const path = tmp("compact-fixed-point.jed");
  const db = createFile(path);
  growAndShrink(db);
  db.compact("main");
  const first = readFileSync(path);
  db.compact("main");
  const second = readFileSync(path);
  assert.equal(first.length, second.length);
  // Only the version (and so the meta checksums) moved.
  assert.ok(first.subarray(PAGE * 2).equals(second.subarray(PAGE * 2)));
  db.close();
});

test("in-memory compaction lowers storage bytes", () => {
  const db = createDatabase({ pageSize: PAGE });
  growAndShrink(db);
  const before = db.storageBytes("main");
  const probe = rowsAndCost(db, PROBE);
  const version = db.version;
  db.compact("MAIN");
  assert.ok(db.storageBytes("main") * 4n < before);
  assert.equal(db.version, version + 1n);
  assert.deepEqual(rowsAndCost(db, PROBE), probe);
  const s = db.session();
  s.execute("INSERT INTO t VALUES (5000, 'tail', 3)");
  s.close();
  assert.deepEqual(count(db, "SELECT count(*) FROM t"), [[intValue(101n)]]);
});

test("prepared statements survive compaction", () => {
  const db = createDatabase();
  growAndShrink(db);
  const s = db.session();
  const stmt = s.prepareStatement("SELECT count(*) FROM t WHERE n = $1");
  const run = (): Value[][] => {
    const rows = s.queryPrepared(stmt, [intValue(3n)]);
    try {
      return [...rows];
    } finally {
      rows.close();
    }
  };
  const before = run();
  db.compact("main");
  assert.deepEqual(run(), before);
  s.close();
});

test("compaction preconditions fail without writing", () => {
  const path = tmp("compact-preconditions.jed");
  const db = createFile(path);
  growAndShrink(db);
  const pages = db.pageCount;

  assert.equal(
    errCode(() => db.compact("nope")),
    "42704",
  );
  assert.equal(
    errCode(() => db.compact("temp")),
    "42704",
  );

  // A pinned reader blocks it, and so does an open cursor.
  const reader = db.readSession();
  assert.equal(
    errCode(() => db.compact("main")),
    "55006",
  );
  reader.close();
  const s = db.session();
  const rows = s.query("SELECT id FROM t");
  assert.ok(!rows[Symbol.iterator]().next().done);
  assert.equal(
    errCode(() => db.compact("main")),
    "55006",
  );
  rows.close();

  // An open write transaction blocks it; compaction never waits for the writer gate.
  const writer = db.session();
  writer.begin(true);
  assert.equal(
    errCode(() => db.compact("main")),
    "55006",
  );
  writer.rollback();
  s.close();
  writer.close();

  assert.equal(db.pageCount, pages, "a refused compaction writes nothing");
  db.compact("main");
  db.close();

  const ro = openDatabase(path, { readOnly: true });
  assert.equal(
    errCode(() => ro.compact("main")),
    "25006",
  );
  ro.close();
});

test("attachments compact independently", () => {
  const work = tmp("compact-attachment.jed");
  {
    const db = createFile(work);
    growAndShrink(db);
    db.close();
  }
  const db = createDatabase();
  db.attach("work", attachFile(work), false);
  db.attach("scratch", attachMemory(), false);
  const mainVersion = db.version;
  const before = statSync(work).size;
  db.compact("Work");
  db.compact("scratch");
  assert.ok(statSync(work).size * 4 < before);
  assert.equal(db.version, mainVersion, "main is untouched");
  assert.deepEqual(count(db, "SELECT count(*) FROM work.t"), [[intValue(100n)]]);
  const s = db.session();
  s.execute("INSERT INTO work.t VALUES (5000, 'tail', 3)");
  s.close();
  db.detach("work");
  db.attach("frozen", attachFile(work), true);
  assert.equal(
    errCode(() => db.compact("frozen")),
    "25006",
  );
  assert.deepEqual(count(db, "SELECT count(*) FROM frozen.t"), [[intValue(101n)]]);
  db.close();
});

test("a stale temp file is ignored and replaced", () => {
  const path = tmp("compact-stale-temp.jed");
  let db = createFile(path);
  growAndShrink(db);
  db.close();
  // A crash mid-compaction leaves a partial temp file; open never reads it.
  writeFileSync(path + ".jedtmp", "partial image");
  db = openDatabase(path);
  assert.deepEqual(count(db, "SELECT count(*) FROM t"), [[intValue(100n)]]);
  db.compact("main");
  assert.ok(!existsSync(path + ".jedtmp"));
  db.close();
});

test("compaction keeps symlinks and permission bits", {
  skip: process.platform === "win32",
}, () => {
  const path = tmp("compact-target.jed");
  const link = tmp("compact-link.jed");
  let db = createFile(path);
  growAndShrink(db);
  db.close();
  chmodSync(path, 0o640);
  symlinkSync(path, link);

  db = openDatabase(link);
  db.compact("main");
  db.close();
  assert.ok(lstatSync(link).isSymbolicLink());
  assert.equal(statSync(path).mode & 0o777, 0o640);
  db = openDatabase(link);
  assert.deepEqual(count(db, "SELECT count(*) FROM t"), [[intValue(100n)]]);
  db.close();
});

// MemoryHandle is a minimal in-memory FileSystemSyncAccessHandle stand-in (hosts.md §5), enough to run
// the OPFS host in Node.
class MemoryHandle implements SyncAccessHandle {
  private bytes = new Uint8Array(0);

  read(buffer: Uint8Array, opts: { at: number }): number {
    const n = Math.max(0, Math.min(buffer.length, this.bytes.length - opts.at));
    buffer.set(this.bytes.subarray(opts.at, opts.at + n));
    return n;
  }

  write(buffer: Uint8Array, opts: { at: number }): number {
    const end = opts.at + buffer.length;
    if (end > this.bytes.length) this.truncate(end);
    this.bytes.set(buffer, opts.at);
    return buffer.length;
  }

  truncate(newSize: number): void {
    const next = new Uint8Array(newSize);
    next.set(this.bytes.subarray(0, Math.min(newSize, this.bytes.length)));
    this.bytes = next;
  }

  getSize(): number {
    return this.bytes.length;
  }

  flush(): void {}

  close(): void {}
}

test("the OPFS host cannot compact yet (0A000)", () => {
  const db = Database.fromEngine(createOpfsWithHandle(new MemoryHandle(), { pageSize: PAGE }));
  growAndShrink(db);
  const pages = db.pageCount;
  assert.equal(
    errCode(() => db.compact("main")),
    "0A000",
  );
  assert.equal(db.pageCount, pages);
  assert.deepEqual(count(db, "SELECT count(*) FROM t"), [[intValue(100n)]]);
  db.close();
});
