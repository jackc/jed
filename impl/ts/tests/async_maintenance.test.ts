// Host maintenance on the public async API (spec/design/ts-runtime.md §10; api.md §2.6 and
// Encryption at rest): compact, changeCredential and rekey cross the worker boundary to the same engine
// calls the synchronous API makes. The engine contract itself is tested in encryption.test.ts and the
// compaction tests; these exercise the transport, the never-waits rule against the worker's own
// sessions and cursors, and SQLSTATEs surviving the boundary. Host-API surface (CLAUDE.md §10).
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import {
  type Argon2Params,
  type AsyncDatabase,
  createDatabase,
  type Credential,
  EngineError,
  openDatabase,
  TransportError,
} from "../src/async/node.ts";

const PAGE = 1024;
const KEY: Credential = { key: new Uint8Array(32).fill(0x4a) };
const SMALL_KDF: Argon2Params = { memoryKib: 64, iterations: 1, parallelism: 1 };

const scratch = mkdtempSync(join(tmpdir(), "jed-async-maintenance-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function sqlstate(code: string): (error: unknown) => boolean {
  return (error) => {
    assert.ok(error instanceof EngineError, String(error));
    assert.equal(error.code(), code);
    return true;
  };
}

async function count(db: AsyncDatabase): Promise<bigint> {
  const row = await db.get("SELECT count(*) AS n FROM t");
  return row!.n as bigint;
}

async function populate(db: AsyncDatabase): Promise<void> {
  await db.executeScript(
    "CREATE TABLE t (id i64 PRIMARY KEY, note text);" +
      "INSERT INTO t SELECT g, 'secret-' || g FROM generate_series(1, 2000) g",
  );
}

test("compact returns dead pages and never waits for a session", async () => {
  const path = join(scratch, "compact.jed");
  const db = await createDatabase({ path, pageSize: PAGE, skipFsync: true });
  try {
    await populate(db);
    await db.execute("DELETE FROM t WHERE id > 100");
    const before = statSync(path).size;

    // A write transaction or an open cursor on another session is 55006, as on the sync API.
    const session = await db.session();
    const tx = await session.begin();
    await tx.execute("INSERT INTO t VALUES (5000, 'pending')");
    await assert.rejects(db.compact("main"), sqlstate("55006"));
    await tx.rollback();
    const rows = await db.query("SELECT id FROM t");
    await assert.rejects(db.compact("main"), sqlstate("55006"));
    await rows.close();
    await session.close();

    await assert.rejects(db.compact("nope"), sqlstate("42704"));
    await assert.rejects(
      db.compact(7 as unknown as string),
      (error: unknown) => error instanceof TransportError && error.code === "PROTOCOL",
    );

    await db.compact("main");
    assert.ok(statSync(path).size < before, `${statSync(path).size} >= ${before}`);
    assert.equal(await count(db), 100n);
    await db.execute("INSERT INTO t VALUES (6000, 'after')");
  } finally {
    await db.close();
  }
  const reopened = await openDatabase(path, { skipFsync: true });
  try {
    assert.equal(await count(reopened), 101n);
  } finally {
    await reopened.close();
  }
});

test("an in-memory database compacts but has no credential", async () => {
  const db = await createDatabase();
  try {
    await populate(db);
    await db.execute("DELETE FROM t WHERE id > 10");
    await db.compact("main");
    assert.equal(await count(db), 10n);
    await assert.rejects(db.changeCredential("main", KEY), sqlstate("55000"));
    await assert.rejects(db.rekey("main", KEY), sqlstate("55000"));
  } finally {
    await db.close();
  }
});

test("changeCredential rewraps the key so only the new credential opens the file", async () => {
  const path = join(scratch, "credential.jed");
  const pass: Credential = { passphrase: "new passphrase" };
  const db = await createDatabase({ path, pageSize: PAGE, skipFsync: true, credential: KEY });
  try {
    await populate(db);
    const session = await db.session();
    const tx = await session.begin();
    await assert.rejects(db.changeCredential("main", pass, SMALL_KDF), sqlstate("55006"));
    await tx.rollback();
    await session.close();
    await assert.rejects(
      db.changeCredential("main", { key: new Uint8Array(16) }),
      sqlstate("22023"),
    );
    await assert.rejects(
      db.changeCredential("main", null as unknown as Credential),
      sqlstate("22023"),
    );
    await assert.rejects(
      db.changeCredential("main", pass, { memoryKib: 1, iterations: 1, parallelism: 1 }),
      sqlstate("22023"),
    );
    await assert.rejects(db.changeCredential("nope", pass, SMALL_KDF), sqlstate("42704"));

    await db.changeCredential("main", pass, SMALL_KDF);
    // The handle keeps its data key: it reads and writes on.
    await db.execute("INSERT INTO t VALUES (5000, 'after')");
  } finally {
    await db.close();
  }
  await assert.rejects(openDatabase(path, { skipFsync: true, credential: KEY }), sqlstate("28P01"));
  const reopened = await openDatabase(path, { skipFsync: true, credential: pass });
  try {
    assert.equal(await count(reopened), 2001n);
  } finally {
    await reopened.close();
  }
  const plain = await createDatabase({ path: join(scratch, "plain.jed"), skipFsync: true });
  try {
    await assert.rejects(plain.changeCredential("main", KEY), sqlstate("28P03"));
  } finally {
    await plain.close();
  }
});

test("rekey encrypts, rotates and decrypts through the worker", async () => {
  const path = join(scratch, "rekey.jed");
  let db = await createDatabase({ path, pageSize: PAGE, skipFsync: true });
  try {
    await populate(db);
    // A handle whose scratch files are plaintext cannot encrypt.
    await assert.rejects(db.rekey("main", KEY), sqlstate("55000"));
  } finally {
    await db.close();
  }

  db = await openDatabase(path, { skipFsync: true, scratchEncryption: "on" });
  try {
    const rows = await db.query("SELECT id FROM t");
    await assert.rejects(db.rekey("main", KEY), sqlstate("55006"));
    await rows.close();
    await db.rekey("main", KEY);
    assert.equal(await count(db), 2000n);
  } finally {
    await db.close();
  }
  await assert.rejects(openDatabase(path, { skipFsync: true }), sqlstate("28P02"));

  const pass: Credential = { passphrase: "rotated" };
  db = await openDatabase(path, { skipFsync: true, credential: KEY });
  try {
    await db.rekey("main", pass, SMALL_KDF);
    await db.execute("INSERT INTO t VALUES (5000, 'after')");
  } finally {
    await db.close();
  }
  await assert.rejects(openDatabase(path, { skipFsync: true, credential: KEY }), sqlstate("28P01"));

  db = await openDatabase(path, { skipFsync: true, credential: pass });
  try {
    await db.rekey("main", null);
  } finally {
    await db.close();
  }
  await assert.rejects(
    openDatabase(path, { skipFsync: true, credential: pass }),
    sqlstate("28P03"),
  );
  db = await openDatabase(path, { skipFsync: true });
  try {
    assert.equal(await count(db), 2001n);
    assert.deepEqual(await db.get("SELECT note FROM t WHERE id = 5000"), { note: "after" });
  } finally {
    await db.close();
  }
});

test("a read-only handle refuses maintenance", async () => {
  const path = join(scratch, "readonly.jed");
  const db = await createDatabase({ path, pageSize: PAGE, skipFsync: true, credential: KEY });
  await populate(db);
  await db.close();
  const ro = await openDatabase(path, { readOnly: true, credential: KEY });
  try {
    await assert.rejects(ro.compact("main"), sqlstate("25006"));
    await assert.rejects(ro.changeCredential("main", KEY), sqlstate("25006"));
    await assert.rejects(ro.rekey("main", null), sqlstate("25006"));
  } finally {
    await ro.close();
  }
});
