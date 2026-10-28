// Database ↔ bytes — Database.serialize / Database.deserialize (spec/design/api.md §2.8), and their
// async forms (db.serialize, deserializeDatabase). Both are host-API acts the SQL corpus cannot reach,
// so their contract lives here: an export is the from-scratch image of the committed snapshot at its
// version, it round-trips rows and costs, a raw file's bytes load, the loaded copy is independent of
// the bytes and of the source, and the error cases are 42704/0A000/55000/XX001. Mirrors
// impl/rust/tests/serialize.rs and impl/go/serialize_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import {
  attachMemory,
  createDatabase,
  Database,
  Environment,
  intValue,
  type Value,
} from "../src/lib.ts";
import {
  createDatabase as createAsync,
  deserializeDatabase,
  EngineError,
  TransportError,
} from "../src/async/node.ts";
import { errCode } from "./util.ts";

const PAGE = 4096;

const scratch = mkdtempSync(join(tmpdir(), "jed-serialize-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function memory(): Database {
  return createDatabase({ pageSize: PAGE });
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

function exec(db: Database, sql: string): void {
  const s = db.session();
  try {
    s.execute(sql);
  } finally {
    s.close();
  }
}

const PROBE = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

test("round trip keeps rows, costs and version", () => {
  const db = memory();
  growAndShrink(db);
  const bytes = db.serialize("main");
  assert.equal(bytes.length % PAGE, 0);
  assert.ok(bytes.length < db.pageCount * PAGE, "the export holds no dead pages");
  assert.deepEqual(bytes, db.toImage(PAGE, db.version));

  const copy = Database.deserialize(bytes);
  assert.equal(copy.version, db.version);
  assert.equal(copy.pageSize, PAGE);
  assert.deepEqual(rowsAndCost(copy, PROBE), rowsAndCost(db, PROBE));
  // A clean image re-exports to the same bytes.
  assert.deepEqual(copy.serialize("main"), bytes);

  // The copy is independent of its source in both directions.
  exec(copy, "DELETE FROM t");
  assert.deepEqual(rowsAndCost(db, "SELECT count(*) FROM t").rows, [[intValue(100n)]]);
  assert.equal(copy.version, db.version + 1n);
  exec(db, "DELETE FROM t WHERE id > 50");
  assert.deepEqual(rowsAndCost(copy, "SELECT count(*) FROM t").rows, [[intValue(0n)]]);
});

test("export excludes uncommitted changes", () => {
  const db = memory();
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO t VALUES (1)");
  const s = db.session();
  s.begin(true);
  s.execute("INSERT INTO t VALUES (2)");
  // The writer holds the gate; the export never waits for it.
  const bytes = db.serialize("main");
  s.commit();
  s.close();
  assert.deepEqual(rowsAndCost(Database.deserialize(bytes), "SELECT id FROM t").rows, [
    [intValue(1n)],
  ]);
});

test("the caller keeps its buffer", () => {
  const db = memory();
  exec(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const bytes = db.serialize("main");
  const copy = Database.deserialize(bytes);
  bytes.fill(0);
  exec(copy, "INSERT INTO t VALUES (1)");
  assert.deepEqual(rowsAndCost(copy, "SELECT id FROM t").rows, [[intValue(1n)]]);
});

test("empty database round trips", () => {
  const bytes = memory().serialize("main");
  assert.equal(bytes.length, 3 * PAGE, "two meta slots and the catalog");
  const copy = Database.deserialize(bytes);
  assert.equal(copy.version, 0n);
  assert.deepEqual(rowsAndCost(copy, "SELECT count(*) FROM jed_tables").rows, [[intValue(0n)]]);
});

test("raw file bytes load and export matches", () => {
  const path = join(scratch, "file.jed");
  const db = createDatabase({ path, pageSize: PAGE, skipFsync: true });
  growAndShrink(db);
  const exported = db.serialize("main");
  const raw = new Uint8Array(readFileSync(path));
  assert.notDeepEqual(raw, exported, "the file still holds its dead pages");

  const copy = Database.deserialize(raw);
  assert.equal(copy.version, db.version);
  assert.deepEqual(rowsAndCost(copy, PROBE), rowsAndCost(db, PROBE));
  assert.deepEqual(copy.serialize("main"), exported);
  db.close();
});

test("attachments by name", () => {
  const db = memory();
  db.attach("scratch", attachMemory(), false);
  exec(db, "CREATE TABLE scratch.u (id i32 PRIMARY KEY)");
  exec(db, "INSERT INTO scratch.u VALUES (7)");
  const copy = Database.deserialize(db.serialize("scratch"));
  assert.deepEqual(rowsAndCost(copy, "SELECT id FROM u").rows, [[intValue(7n)]]);
  assert.equal(
    errCode(() => db.serialize("nope")),
    "42704",
  );
  assert.equal(
    errCode(() => db.serialize("temp")),
    "42704",
  );
});

test("encrypted and malformed inputs fail", () => {
  const path = join(scratch, "encrypted.jed");
  const db = createDatabase({
    path,
    pageSize: 1024,
    skipFsync: true,
    environment: Environment.builder().insecureSeededKeyEntropy(1n).build(),
    credential: { key: new Uint8Array(32).fill(0x4a) },
  });
  assert.equal(
    errCode(() => db.serialize("main")),
    "0A000",
  );
  db.close();
  const raw = new Uint8Array(readFileSync(path));
  assert.equal(
    errCode(() => Database.deserialize(raw)),
    "55000",
  );
  assert.equal(
    errCode(() => Database.deserialize(new TextEncoder().encode("not a database"))),
    "XX001",
  );
  assert.equal(
    errCode(() => Database.deserialize(new Uint8Array())),
    "XX001",
  );
});

test("storage limit applies to the copy", () => {
  const db = memory();
  growAndShrink(db);
  const bytes = db.serialize("main");
  const copy = Database.deserialize(bytes, { maxStorageBytes: BigInt(bytes.length) });
  assert.equal(
    errCode(() =>
      exec(
        copy,
        "INSERT INTO t SELECT g, repeat('y', 300), 0 FROM generate_series(10000, 12000) g",
      ),
    ),
    "54P06",
  );
});

test("async serialize and deserializeDatabase cross the worker boundary", async () => {
  const db = await createAsync({ pageSize: PAGE });
  try {
    await db.executeScript(
      "CREATE TABLE t (id i64 PRIMARY KEY, note text);" +
        "INSERT INTO t SELECT g, 'n' || g FROM generate_series(1, 300) g",
    );
    const bytes = await db.serialize("main");
    assert.ok(bytes instanceof Uint8Array);
    // The image equals what the synchronous core exports for the same state.
    assert.deepEqual(Database.deserialize(bytes).serialize("main"), bytes);
    await assert.rejects(db.serialize("nope"), (error: unknown) => {
      assert.ok(error instanceof EngineError, String(error));
      assert.equal(error.code(), "42704");
      return true;
    });

    const copy = await deserializeDatabase(bytes);
    try {
      const row = await copy.get("SELECT count(*) AS n FROM t");
      assert.equal(row!.n, 300n);
      await copy.execute("DELETE FROM t WHERE id > 10");
      assert.equal((await db.get("SELECT count(*) AS n FROM t"))!.n, 300n);
    } finally {
      await copy.close();
    }

    // The image is bounded by the transport's result limit.
    const small = await createAsync({
      pageSize: PAGE,
      runtime: { maxResultBytes: 256 * 1024, maxBatchBytes: 4096 },
    });
    try {
      await small.execute("CREATE TABLE big (id i64 PRIMARY KEY, pad text)");
      await small.execute(
        "INSERT INTO big SELECT g, repeat('x', 500) FROM generate_series(1, 1000) g",
      );
      await assert.rejects(small.serialize("main"), (error: unknown) => {
        assert.ok(error instanceof TransportError, String(error));
        assert.equal(error.code, "RESULT_TOO_LARGE");
        return true;
      });
    } finally {
      await small.close();
    }
  } finally {
    await db.close();
  }
  await assert.rejects(deserializeDatabase(new Uint8Array(10)), (error: unknown) => {
    assert.ok(error instanceof EngineError, String(error));
    assert.equal(error.code(), "XX001");
    return true;
  });
});
