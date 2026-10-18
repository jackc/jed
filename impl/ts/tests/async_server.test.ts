// The shared concurrency schedules drive the common asynchronous runtime. Keeping their SQL and
// expected results in the language-neutral corpus prevents a second host-specific semantic suite.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  AsyncDatabase,
  type AsyncSession,
  type AsyncTransaction,
  RuntimeConnection,
} from "../src/async/client.ts";
import { TransportError } from "../src/async/errors.ts";
import type {
  RuntimeLimits,
  RuntimeReply,
  RuntimeRequest,
  RuntimeTransport,
} from "../src/async/protocol.ts";
import { RuntimeServer } from "../src/async/server.ts";
import { EngineError } from "../src/errors.ts";
import { createDatabase } from "../src/file.ts";
import { PrivilegeSet } from "../src/privileges.ts";
import type { Database } from "../src/shared.ts";
import { render } from "../src/value.ts";

function harness(core: Database, limits: Partial<RuntimeLimits> = {}) {
  let receive: (reply: RuntimeReply) => void = () => {};
  const server = new RuntimeServer({
    open: () => core,
    send: (reply) => queueMicrotask(() => receive(structuredClone(reply))),
  });
  const transport: RuntimeTransport = {
    postMessage: (request) => server.receive(structuredClone(request)),
    onMessage: (handler) => {
      receive = handler;
    },
    onFailure: () => {},
    terminate: () => {},
  };
  const connection = new RuntimeConnection(transport, limits, true);
  return { server, connection, db: new AsyncDatabase(connection) };
}

const suites = new URL("../../../spec/conformance/suites/concurrency/", import.meta.url);
const files = readdirSync(suites)
  .filter((name) => name.endsWith(".test"))
  .sort();
const directives = new Set(["open", "on", "commit", "rollback", "close", "expect"]);
function boundary(line: string): boolean {
  const text = line.trim();
  return text === "" || text.startsWith("#") || directives.has(text.split(/\s+/)[0]);
}

async function schedule(text: string, core: Database): Promise<void> {
  const initialVersion = core.version; // File bootstrap is txid 1; the corpus starts from memory txid 0.
  for (const line of text.split("\n")) {
    const attach = /^# attach:\s*(\S+)/.exec(line);
    if (attach) core.attach(attach[1]);
    const limit = /^# max_storage_bytes:\s*(\d+)(?:\s+(\S+))?/.exec(line);
    if (limit) core.setMaxStorageBytes(limit[2] ?? "main", BigInt(limit[1]));
  }
  const { connection, db } = harness(core, { maxBatchRows: 3 });
  await connection.call("open", { mode: "create", limits: connection.limits });
  type Handle = { session: AsyncSession; handle: AsyncSession | AsyncTransaction; write: boolean };
  const sessions = new Map<string, Handle>();
  let blocked:
    | { name: string; session: AsyncSession; promise: Promise<AsyncTransaction>; settled: boolean }
    | undefined;
  const lines = text.split("\n");
  try {
    for (let index = 0; index < lines.length; ) {
      const line = lines[index++].trim();
      if (line === "" || line.startsWith("#")) continue;
      const fields = line.split(/\s+/);
      const sid = fields[1];
      switch (fields[0]) {
        case "open": {
          const write = fields[2] === "write";
          const session = write ? await db.session() : await db.readSession();
          if (fields[3] === "blocks") {
            assert.equal(blocked, undefined);
            const waiting = { name: sid, session, promise: session.begin(), settled: false };
            waiting.promise.then(
              () => {
                waiting.settled = true;
              },
              () => {
                waiting.settled = true;
              },
            );
            blocked = waiting;
          } else
            sessions.set(sid, { session, handle: write ? await session.begin() : session, write });
          break;
        }
        case "commit":
        case "rollback":
        case "close": {
          const active = sessions.get(sid)!;
          assert.ok(active, line);
          if (fields[0] === "commit") await active.handle.commit();
          else if (fields[0] === "rollback") await active.handle.rollback();
          else await active.handle.close();
          await active.session.close();
          sessions.delete(sid);
          if (active.write && blocked) {
            sessions.set(blocked.name, {
              session: blocked.session,
              handle: await blocked.promise,
              write: true,
            });
            blocked = undefined;
          }
          break;
        }
        case "expect":
          assert.equal(
            sid === "version" ? core.version : core.oldestLiveTxid(),
            BigInt(fields[2]) + initialVersion,
            line,
          );
          break;
        case "on": {
          const handle = sessions.get(sid)!.handle;
          const sql: string[] = [];
          const expected: string[] = [];
          if (fields[2] === "query") {
            while (index < lines.length && lines[index].trim() !== "----") sql.push(lines[index++]);
            index++;
            while (index < lines.length && !boundary(lines[index]))
              expected.push(lines[index++].trim());
          } else while (index < lines.length && !boundary(lines[index])) sql.push(lines[index++]);
          const run = async (): Promise<string[]> => {
            const rows = await handle.query(sql.join("\n"));
            const values: string[][] = [];
            try {
              for await (const row of rows) values.push(row.map(render));
            } finally {
              await rows.close();
            }
            if (fields[4] === "rowsort")
              values.sort((a, b) =>
                a.join("\0") < b.join("\0") ? -1 : a.join("\0") > b.join("\0") ? 1 : 0,
              );
            const flat = values.flat();
            if (fields[4] === "valuesort") flat.sort();
            return flat;
          };
          if (fields[2] === "statement" && fields[3] === "error") {
            await assert.rejects(
              run,
              (error: unknown) => error instanceof EngineError && error.code() === fields[4],
              sql.join("\n"),
            );
          } else {
            const actual = await run();
            if (fields[2] === "query") {
              let sorted = expected;
              if (fields[4] === "valuesort") sorted = [...expected].sort();
              if (fields[4] === "rowsort") {
                const width = fields[3].length;
                const groups: string[][] = [];
                for (let at = 0; at < expected.length; at += width)
                  groups.push(expected.slice(at, at + width));
                groups.sort((a, b) =>
                  a.join("\0") < b.join("\0") ? -1 : a.join("\0") > b.join("\0") ? 1 : 0,
                );
                sorted = groups.flat();
              }
              assert.deepEqual(actual, sorted, sql.join("\n"));
            }
          }
          break;
        }
        default:
          assert.fail(`unknown concurrency directive: ${line}`);
      }
      if (blocked)
        assert.equal(
          blocked.settled,
          false,
          "the corpus writer remains parked before owner release",
        );
    }
    assert.equal(sessions.size, 0);
    assert.equal(blocked, undefined);
  } finally {
    await db.close();
  }
}

for (const name of files) {
  const text = readFileSync(new URL(name, suites), "utf8");
  test(`async runtime shared concurrency corpus: ${name}`, { timeout: 15_000 }, async () => {
    await schedule(text, createDatabase());
  });
  // resource.storage_memory intentionally specifies memory-only forced-compaction behavior.
  if (!text.includes("resource.storage_memory")) {
    test(`async file runtime shared concurrency corpus: ${name}`, { timeout: 15_000 }, async () => {
      const directory = mkdtempSync(join(tmpdir(), "jed-async-schedule-"));
      try {
        await schedule(
          text,
          createDatabase({
            path: join(directory, "database.jed"),
            locking: "none",
            skipFsync: true,
          }),
        );
      } finally {
        rmSync(directory, { recursive: true, force: true });
      }
    });
  }
}

test("runtime close during asynchronous open releases the opened database", async () => {
  const core = createDatabase();
  let release!: () => void;
  const opening = new Promise<void>((resolve) => {
    release = resolve;
  });
  const replies: RuntimeReply[] = [];
  const server = new RuntimeServer({
    open: async () => {
      await opening;
      return core;
    },
    send: (reply) => replies.push(reply),
  });
  const req = (id: number, op: string): RuntimeRequest => ({ version: 1, id, op, args: {} });
  server.receive(req(1, "open"));
  await new Promise((resolve) => setTimeout(resolve, 5));
  server.receive(req(2, "close"));
  release();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.deepEqual(
    replies.map((reply) => [reply.id, reply.ok]),
    [
      [1, true],
      [2, true],
    ],
  );
  server.receive(req(3, "session.open"));
  assert.equal(replies.at(-1)?.ok, false);
});

test("runtime rejects duplicate operation ids without executing SQL twice", async () => {
  const core = createDatabase();
  const { connection, server, db } = harness(core);
  await connection.call("open");
  await db.execute("CREATE TABLE t (id i32)");
  const request: RuntimeRequest = {
    version: 1,
    id: 100,
    op: "execute",
    args: { sql: "INSERT INTO t VALUES (1)", params: [] },
  };
  server.receive(request);
  server.receive(request);
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(core.get("SELECT count(*) AS n FROM t")!.n, 1n);
  // The direct high id intentionally makes the connection unusable; close via the protocol.
  server.receive({ version: 1, id: 101, op: "close", args: {} });
});

test("read sessions preserve resource limits and reconstructed privileges", async () => {
  const { db, connection } = harness(createDatabase());
  await connection.call("open");
  try {
    await db.execute("CREATE TABLE t (id i32)");
    const denied = await db.readSession({ defaultPrivileges: PrivilegeSet.empty() });
    await assert.rejects(
      denied.execute("SELECT * FROM t"),
      (error: unknown) => error instanceof EngineError && error.code() === "42501",
    );
    await denied.close();
    const limited = await db.readSession({ maxCost: 1n });
    await assert.rejects(
      limited.execute("SELECT 1 + 2"),
      (error: unknown) => error instanceof EngineError && error.code() === "54P01",
    );
    await limited.close();
  } finally {
    await db.close();
  }
});

test("oversized result row releases its cursor capacity", async () => {
  const { db, connection } = harness(createDatabase(), { maxBatchBytes: 1024, maxCursors: 1 });
  await connection.call("open", { limits: connection.limits });
  try {
    const rows = await db.query("SELECT repeat('x', 1000)");
    await assert.rejects(
      async () => {
        for await (const _row of rows) {
        }
      },
      (error: unknown) => error instanceof TransportError && error.code === "RESULT_TOO_LARGE",
    );
    assert.deepEqual(await db.all("SELECT 1 AS n"), [{ n: 1n }]);
  } finally {
    await db.close();
  }
});

test("cancellation before commit releases the transaction owned by its closed proxy", async () => {
  const { db, connection } = harness(createDatabase());
  await connection.call("open");
  try {
    const { session } = (await connection.call("session.open")) as { session: number };
    const { transaction } = (await connection.call("begin", { writable: true }, session)) as {
      transaction: number;
    };
    await connection.call(
      "execute",
      { sql: "CREATE TABLE discarded (id i32)" },
      session,
      transaction,
    );
    await assert.rejects(
      connection.call("commit", {}, session, transaction, { signal: AbortSignal.abort() }),
      (error: unknown) => error instanceof EngineError && error.code() === "57014",
    );
    const status = (await connection.call("status", {}, session)) as {
      status: string;
      inTransaction: boolean;
    };
    assert.equal(status.status, "Idle");
    assert.equal(status.inTransaction, false);
    await db.execute("CREATE TABLE after_cancel (id i32)");
    await assert.rejects(
      db.execute("SELECT * FROM discarded"),
      (error: unknown) => error instanceof EngineError && error.code() === "42P01",
    );
  } finally {
    await db.close();
  }
});

test("canceling a queued cursor fetch closes it and preserves failed transaction state", async () => {
  const { db, connection } = harness(createDatabase(), { maxCursors: 1 });
  await connection.call("open", { limits: connection.limits });
  try {
    const session = await db.session();
    const tx = await session.begin(false);
    const controller = new AbortController();
    const rows = await tx.query("SELECT 1", [], { signal: controller.signal });
    controller.abort();
    await assert.rejects(
      rows.next(),
      (error: unknown) => error instanceof EngineError && error.code() === "57014",
    );
    await assert.rejects(
      tx.execute("SELECT 2"),
      (error: unknown) => error instanceof EngineError && error.code() === "25P02",
    );
    await tx.rollback();
    assert.deepEqual(await session.all("SELECT 3 AS n"), [{ n: 3n }]);
    await session.close();
  } finally {
    await db.close();
  }
});
