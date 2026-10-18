// Host-boundary tests: the shared corpus covers SQL/concurrency semantics; these exercise
// real Node workers, transport, cancellation, lifecycle and durable file adapters.
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import {
  createDatabase,
  openDatabase,
  EngineError,
  TransportError,
  Decimal,
  intValue,
  render,
} from "../src/async/node.ts";

test("Node worker returns typed values and reuses prepared parameters", async () => {
  const db = await createDatabase();
  try {
    const statement = await db.prepare("SELECT $1::i64 AS n, $2::decimal AS d, $3::bytea AS b");
    try {
      const rows = await statement.query([
        intValue(9223372036854775807n),
        { kind: "decimal", dec: Decimal.fromDigitsScale(false, "12300", 4) },
        { kind: "bytea", bytes: new Uint8Array([0, 255]) },
      ]);
      const values = [];
      for await (const row of rows) values.push(row);
      assert.equal(values[0][0].kind, "int");
      assert.equal(render(values[0][0]), "9223372036854775807");
      assert.equal(values[0][1].kind, "decimal");
      if (values[0][1].kind === "decimal") assert.ok(values[0][1].dec instanceof Decimal);
      assert.equal(render(values[0][1]), "1.2300");
      assert.deepEqual(values[0][2], { kind: "bytea", bytes: new Uint8Array([0, 255]) });
      assert.ok(rows.cost > 0n);
      assert.equal(
        (await statement.get(7n, Decimal.fromDigitsScale(false, "250", 2), new Uint8Array([1])))?.n,
        7n,
      );
    } finally {
      await statement.close();
    }
  } finally {
    await db.close();
  }
});

test("application event loop cancels resident SQL in the execution worker", {
  timeout: 10000,
}, async () => {
  const db = await createDatabase();
  try {
    const session = await db.session();
    const tx = await session.begin();
    const controller = new AbortController();
    let heartbeats = 0;
    const timer = setInterval(() => {
      if (++heartbeats === 5) controller.abort();
    }, 5);
    try {
      await assert.rejects(
        tx.execute(
          "WITH RECURSIVE n(x) AS (VALUES (1) UNION ALL SELECT x+1 FROM n WHERE x<10000000) SELECT sum(x) FROM n",
          [],
          { signal: controller.signal },
        ),
        (error: unknown) => error instanceof EngineError && error.code() === "57014",
      );
      assert.ok(heartbeats >= 5, "main thread stayed responsive while SQL ran");
      await assert.rejects(
        tx.execute("SELECT 1"),
        (error: unknown) => error instanceof EngineError && error.code() === "25P02",
      );
    } finally {
      clearInterval(timer);
      await tx.rollback();
    }
    assert.equal((await session.get("SELECT 42 AS answer"))?.answer, 42n);
    await session.close();
  } finally {
    await db.close();
  }
});

test("file worker close rolls back uncommitted changes and releases ownership", async () => {
  const directory = mkdtempSync(join(tmpdir(), "jed-async-file-"));
  const path = join(directory, "test.jed");
  try {
    const db = await createDatabase({ path });
    await db.executeScript(
      "CREATE TABLE t (id i32 PRIMARY KEY, value text); INSERT INTO t VALUES (1, 'committed')",
    );
    const session = await db.session();
    const tx = await session.begin();
    await tx.execute("INSERT INTO t VALUES (2, 'pending')");
    await db.close();
    await db.close();
    await assert.rejects(
      session.query("SELECT 1"),
      (error: unknown) => error instanceof TransportError && error.code === "CLOSED",
    );
    const reopened = await openDatabase(path);
    try {
      assert.deepEqual(await reopened.all("SELECT * FROM t ORDER BY id"), [
        { id: 1n, value: "committed" },
      ]);
    } finally {
      await reopened.close();
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("failed open releases worker admission and preserves SQLSTATE", async () => {
  const directory = mkdtempSync(join(tmpdir(), "jed-async-open-"));
  try {
    for (let n = 0; n < 2; n++) {
      await assert.rejects(
        openDatabase(join(directory, "missing.jed"), { runtime: { maxWorkers: 1 } }),
        (error: unknown) => error instanceof EngineError && error.code() === "58P01",
      );
    }
    const db = await createDatabase({ runtime: { maxWorkers: 1 } });
    try {
      await assert.rejects(
        createDatabase({ runtime: { maxWorkers: 1 } }),
        (error: unknown) => error instanceof TransportError && error.code === "WORKER_LIMIT",
      );
    } finally {
      await db.close();
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("SQL structured diagnostics and bounded transport errors remain distinct", async () => {
  const db = await createDatabase({
    runtime: { maxBatchBytes: 1024, maxResultBytes: 1024, maxBatchRows: 1, maxQueuedRequests: 1 },
  });
  try {
    await db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    await db.execute("INSERT INTO t VALUES (1)");
    await assert.rejects(db.execute("INSERT INTO t VALUES (1)"), (error: unknown) => {
      assert.ok(error instanceof EngineError);
      assert.equal(error.code(), "23505");
      assert.equal(error.tableName, "t");
      assert.ok(error.constraintName);
      assert.ok(error.detail);
      return true;
    });
    const first = db.execute("SELECT 1");
    await assert.rejects(
      db.execute("SELECT 2"),
      (error: unknown) => error instanceof TransportError && error.code === "QUEUE_FULL",
    );
    await first;
    const rows = await db.query("SELECT repeat('x', 2000)");
    await assert.rejects(
      rows.next(),
      (error: unknown) => error instanceof TransportError && error.code === "RESULT_TOO_LARGE",
    );
    assert.equal((await db.get("SELECT 3 AS n"))?.n, 3n);
  } finally {
    await db.close();
  }
});

test("trusted extensions register in the execution worker, including worker death", async () => {
  const directory = mkdtempSync(join(tmpdir(), "jed-async-extension-"));
  const modulePath = join(directory, "extension.mjs");
  writeFileSync(
    modulePath,
    `
    import { threadId } from 'node:worker_threads';
    export function configure(builder) {
      builder.registerFunction({name:'host_thread',argTypes:[],result:'i64',kernel:()=>({kind:'int',int:BigInt(threadId)})});
      builder.registerFunction({name:'host_exit',argTypes:[],result:'i64',kernel:()=>process.exit(17)});
    }
  `,
  );
  try {
    const db = await createDatabase({ extensionModule: pathToFileURL(modulePath).href });
    try {
      const row = await db.get("SELECT host_thread() AS id");
      assert.ok(typeof row?.id === "bigint" && row.id > 0n);
      await assert.rejects(
        db.execute("SELECT host_exit()"),
        (error: unknown) =>
          error instanceof TransportError &&
          error.code === "WORKER_FAILED" &&
          error.outcome === "unknown",
      );
      await assert.rejects(
        db.query("SELECT 1"),
        (error: unknown) => error instanceof TransportError && error.code === "WORKER_FAILED",
      );
    } finally {
      await db.close();
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
