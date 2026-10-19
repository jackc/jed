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
import { Rows } from "../src/api.ts";
import { newQueryAccount } from "../src/cost.ts";
import { Cursor } from "../src/cursor.ts";
import { ioSteps, type Steps } from "../src/execution.ts";
import { createDatabase } from "../src/file.ts";
import { PrivilegeSet } from "../src/privileges.ts";
import { type Database, setAfterPersistHook } from "../src/shared.ts";
import { render } from "../src/value.ts";

function harness(
  core: Database,
  limits: Partial<RuntimeLimits> = {},
  sharedCancellation = true,
  executionClock?: () => number,
) {
  let receive: (reply: RuntimeReply) => void = () => {};
  const transferredBatches: number[] = [];
  const server = new RuntimeServer({
    open: () => core,
    executionClock,
    send: (reply, transfer) => {
      const copied = structuredClone(reply, { transfer });
      if (transfer !== undefined) {
        assert.equal(transfer.length, 1);
        assert.equal(transfer[0].byteLength, 0, "the worker relinquishes its result buffer");
        assert.ok(copied.ok);
        transferredBatches.push((copied.result as { rows: { length: number } }).rows.length);
      }
      queueMicrotask(() => receive(copied));
    },
  });
  const transport: RuntimeTransport = {
    postMessage: (request) => server.receive(structuredClone(request)),
    onMessage: (handler) => {
      receive = handler;
    },
    onFailure: () => {},
    terminate: () => {},
  };
  const connection = new RuntimeConnection(transport, limits, sharedCancellation);
  return { server, connection, db: new AsyncDatabase(connection), transferredBatches };
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
  const req = (id: number, op: string): RuntimeRequest => ({ version: 2, id, op, args: {} });
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
    version: 2,
    id: 100,
    op: "execute",
    args: { sql: "INSERT INTO t VALUES (1)", params: [] },
  };
  server.receive(request);
  server.receive(request);
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(core.get("SELECT count(*) AS n FROM t")!.n, 1n);
  // The direct high id intentionally makes the connection unusable; close via the protocol.
  server.receive({ version: 2, id: 101, op: "close", args: {} });
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

test("fetch row limits bound evaluation before early cursor close", async (t) => {
  const cases: { name: string; limits: Partial<RuntimeLimits>; expected: number }[] = [
    { name: "default", limits: {}, expected: 512 },
    { name: "explicit 128", limits: { maxBatchRows: 128 }, expected: 128 },
    { name: "explicit 1", limits: { maxBatchRows: 1 }, expected: 1 },
  ];
  for (const { name, limits, expected } of cases) {
    await t.test(name, async () => {
      const core = createDatabase();
      const originalSession = core.session.bind(core);
      let pulled = 0;
      let closed = 0;
      let released = 0;
      core.session = (...args) => {
        const session = originalSession(...args);
        session.queryPreparedSteps = function* (): Steps<Rows> {
          const account = newQueryAccount(0n);
          const rows = new Rows(
            ["n"],
            ["i64"],
            Cursor.streaming({
              nextRow: () => {
                if (++pulled > expected) throw new Error("read beyond the requested batch");
                return [{ kind: "int", int: BigInt(pulled) }];
              },
              cost: () => BigInt(pulled),
              queryAccount: () => account,
              guardCost: () => {},
              close: () => {
                closed++;
              },
            }),
            null,
          );
          rows.attachPin(() => {
            released++;
          });
          return rows;
        };
        return session;
      };
      const { db, connection, transferredBatches } = harness(core, {
        ...limits,
        maxResultBytes: 256 * 1024,
      });
      await connection.call("open", { limits: connection.limits });
      try {
        const rows = await db.query("SELECT 1");
        assert.equal(pulled, 0, "opening a cursor does not fetch its first batch");
        assert.deepEqual((await rows.next()).value, [{ kind: "int", int: 1n }]);
        assert.equal(pulled, expected);
        assert.equal(rows.cost, BigInt(expected));
        assert.deepEqual(transferredBatches, [expected]);
        if (expected > 1) {
          assert.deepEqual((await rows.next()).value, [{ kind: "int", int: 2n }]);
          assert.equal(pulled, expected, "resident rows do not fetch another batch");
        }
        await rows.close();
        await rows.close();
        assert.equal(pulled, expected, "close must not evaluate the following row");
        assert.equal(closed, 1);
        assert.equal(released, 1);
        connection.reserveResult()();
      } finally {
        await db.close();
      }
    });
  }
});

test("packed transfers retain the byte boundary and pending row across fetches", async () => {
  const core = createDatabase();
  const sql = "SELECT g::i64 AS n, repeat('x',100) AS s FROM generate_series(1,3) g";
  const direct = core.query(sql);
  const expected = [...direct];
  const cost = direct.cost;
  direct.close();
  const { db, connection, transferredBatches } = harness(core, {
    maxBatchBytes: 768,
    maxResultBytes: 768,
  });
  await connection.call("open", { limits: connection.limits });
  try {
    const rows = await db.query(sql);
    const actual = [];
    for await (const row of rows) actual.push(row);
    assert.deepEqual(actual, expected);
    assert.equal(rows.cost, cost);
    assert.deepEqual(transferredBatches, [1, 1, 1]);
    connection.reserveResult()();
  } finally {
    await db.close();
  }
});

test("message cancellation interrupts a larger fetch before queued work and releases its cursor", async () => {
  const core = createDatabase();
  const originalSession = core.session.bind(core);
  const controller = new AbortController();
  const events: string[] = [];
  let queries = 0;
  let pulled = 0;
  let queued: Promise<unknown> | undefined;
  let cancelTimer: ReturnType<typeof setTimeout> | undefined;
  core.session = (...args) => {
    const session = originalSession(...args);
    session.queryPreparedSteps = function* (): Steps<Rows> {
      const account = newQueryAccount(0n);
      if (queries++ > 0) {
        events.push("next statement");
        return new Rows([], [], Cursor.buffered([], 0n, account), null);
      }
      const rows = new Rows(
        ["n"],
        ["i64"],
        Cursor.streaming({
          nextRow: () => {
            pulled++;
            if (pulled === 129) {
              // A message-only abort cannot arrive until the worker gives the task queue a turn.
              // The source has no checkpoints. The injected elapsed clock makes the fetch
              // loop's first 256-unit checkpoint expire before this batch reaches 512 rows.
              cancelTimer = setTimeout(() => {
                events.push("cancel sent");
                queued = db.execute("SELECT 2");
                controller.abort();
              }, 0);
            }
            return [{ kind: "int", int: BigInt(pulled) }];
          },
          cost: () => BigInt(pulled),
          queryAccount: () => account,
          guardCost: () => {},
          close: () => {
            events.push("cursor closed");
          },
        }),
        null,
      );
      rows.attachPin(() => events.push("pin released"));
      return rows;
    };
    return session;
  };
  const { db, connection, transferredBatches } = harness(
    core,
    { maxCursors: 1, maxResultBytes: 256 * 1024 },
    false,
    () => pulled / 32,
  );
  await connection.call("open", { limits: connection.limits });
  try {
    const rows = await db.query("SELECT 1", [], { signal: controller.signal });
    await assert.rejects(
      rows.next(),
      (error: unknown) => error instanceof EngineError && error.code() === "57014",
    );
    assert.ok(pulled > 128 && pulled < 512, `canceled after ${pulled} row pulls`);
    assert.ok(queued);
    await queued;
    assert.deepEqual(events, ["cancel sent", "cursor closed", "pin released", "next statement"]);
    assert.deepEqual(transferredBatches, [], "a canceled batch never exposes a partial result");
    assert.equal((await rows.next()).done, true);
    connection.reserveResult()();
    const replacement = await db.query("SELECT 3");
    await replacement.close();
  } finally {
    clearTimeout(cancelTimer);
    await db.close();
  }
});

test("oversized RETURNING transport preserves committed writes and uncertain outcome", async () => {
  const { db, connection } = harness(createDatabase(), { maxBatchBytes: 1024, maxCursors: 1 });
  await connection.call("open", { limits: connection.limits });
  try {
    await db.execute("CREATE TABLE t (v text)");
    const rows = await db.query("INSERT INTO t VALUES (repeat('x',1000)) RETURNING v");
    await assert.rejects(
      rows.next(),
      (error: unknown) =>
        error instanceof TransportError &&
        error.code === "RESULT_TOO_LARGE" &&
        error.outcome === "unknown",
    );
    assert.deepEqual(await db.all("SELECT length(v) AS n FROM t"), [{ n: 1000n }]);
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

// This host-lifecycle seam deliberately suspends one row at its page read. The SQL concurrency
// semantics remain in the corpus above; here we verify worker control handling and continuation
// ownership while a provider promise is pending, including hosts without SharedArrayBuffer.
function suspendedHarness() {
  const core = createDatabase();
  let release!: () => void;
  let notifyStarted!: () => void;
  const started = new Promise<void>((resolve) => {
    notifyStarted = resolve;
  });
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  const events: string[] = [];
  const originalSession = core.session.bind(core);
  let queries = 0;
  core.session = (...args) => {
    const session = originalSession(...args);
    session.queryPreparedSteps = function* (): Steps<Rows> {
      if (queries++ > 0) {
        events.push("next statement");
        return new Rows([], [], Cursor.buffered([], 0n, newQueryAccount(0n)), null);
      }
      let finished = false;
      return new Rows(
        ["n"],
        ["i32"],
        Cursor.streaming({
          nextRow: () => {
            throw new Error("unexpected synchronous row pull");
          },
          *nextRowSteps() {
            try {
              events.push("row started");
              yield* ioSteps(
                () => {},
                async () => {
                  notifyStarted();
                  await pending;
                },
              );
              return undefined;
            } finally {
              finished = true;
              events.push("row unwound");
            }
          },
          cost: () => 0n,
          queryAccount: () => newQueryAccount(0n),
          guardCost: () => {},
          close: () => {
            assert.equal(finished, true, "close must wait for the suspended row to unwind");
            events.push("cursor closed");
          },
        }),
        null,
      );
    };
    return session;
  };
  return { ...harness(core, {}, false), release, started, events };
}

test("message cancellation unwinds suspended fetch before queued SQL runs", async () => {
  const { db, connection, started, release, events } = suspendedHarness();
  await connection.call("open");
  const controller = new AbortController();
  const rows = await db.query("SELECT 1", [], { signal: controller.signal });
  const fetching = rows.next();
  const rejected = assert.rejects(
    fetching,
    (error: unknown) => error instanceof EngineError && error.code() === "57014",
  );
  await started;
  const queued = db.execute("SELECT 2");
  controller.abort();
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.deepEqual(events, ["row started"]);
  release();
  await rejected;
  await queued;
  assert.deepEqual(events, ["row started", "row unwound", "cursor closed", "next statement"]);
  await db.close();
});

test("close waits for suspended fetch cleanup before releasing its session", async () => {
  const { db, connection, started, release, events } = suspendedHarness();
  await connection.call("open");
  const rows = await db.query("SELECT 1");
  const rejected = assert.rejects(
    rows.next(),
    (error: unknown) => error instanceof EngineError && error.code() === "57014",
  );
  await started;
  let closed = false;
  const closing = db.close().then(() => {
    closed = true;
  });
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(closed, false);
  assert.deepEqual(events, ["row started"]);
  release();
  await rejected;
  await closing;
  assert.deepEqual(events, ["row started", "row unwound", "cursor closed"]);
});

test("resident filtered scans accept message cancellation before producing a row", async () => {
  const core = createDatabase();
  core.execute("CREATE TABLE t (id i32)");
  core.execute("INSERT INTO t SELECT g FROM generate_series(1, 2000) g");
  let clock = 0;
  const { db, connection } = harness(core, {}, false, () => (clock += 1));
  await connection.call("open");
  const session = await db.session();
  const tx = await session.begin(false);
  const controller = new AbortController();
  const rows = await tx.query("SELECT id FROM t WHERE id % 2 = -1", [], {
    signal: controller.signal,
  });
  const fetching = rows.next();
  const cancel = setTimeout(() => controller.abort(), 0);
  try {
    await assert.rejects(
      fetching,
      (error: unknown) => error instanceof EngineError && error.code() === "57014",
    );
    await assert.rejects(
      tx.execute("SELECT 1"),
      (error: unknown) => error instanceof EngineError && error.code() === "25P02",
    );
    await tx.rollback();
    assert.deepEqual(await session.all("SELECT 1 AS n"), [{ n: 1n }]);
  } finally {
    clearTimeout(cancel);
    await session.close();
    await db.close();
  }
});

test("canceled suspended scripts discard earlier writes and release the writer", async () => {
  const core = createDatabase();
  core.execute("CREATE TABLE source (id i32)");
  core.execute("CREATE TABLE target (id i32)");
  core.execute("INSERT INTO source SELECT g FROM generate_series(1, 2000) g");
  let clock = 0;
  const { db, connection } = harness(core, {}, false, () => (clock += 1));
  await connection.call("open");
  const controller = new AbortController();
  const script = db.executeScript(
    "INSERT INTO target VALUES (1); SELECT id FROM source WHERE id % 2 = -1; INSERT INTO target VALUES (3)",
    { signal: controller.signal },
  );
  const rejected = assert.rejects(
    script,
    (error: unknown) => error instanceof EngineError && error.code() === "57014",
  );
  const after = db.execute("INSERT INTO target VALUES (2)");
  void after.catch(() => {});
  const cancel = setTimeout(() => controller.abort(), 0);
  try {
    await rejected;
    await after;
    assert.deepEqual(await db.all("SELECT id FROM target"), [{ id: 2n }]);
  } finally {
    clearTimeout(cancel);
    await db.close();
  }
});

test("abort after durable commit reports success while discarding RETURNING rows", async () => {
  const directory = mkdtempSync(join(tmpdir(), "jed-async-committed-"));
  const core = createDatabase({
    path: join(directory, "database.jed"),
    locking: "none",
    skipFsync: true,
  });
  core.execute("CREATE TABLE t (id i32)");
  const { db, connection } = harness(core);
  await connection.call("open");
  const controller = new AbortController();
  let persisted = false;
  setAfterPersistHook(() => {
    persisted = true;
    controller.abort();
  });
  try {
    // The hook flips the SAB and delivers a control request while publication is masked. The
    // committed statement must finish its buffered discard without another cancellation point.
    const result = await db.execute(
      "INSERT INTO t SELECT g FROM generate_series(1, 2000) g RETURNING id",
      [],
      { signal: controller.signal },
    );
    assert.equal(persisted, true);
    assert.ok(result.cost > 0n);
    setAfterPersistHook(null);
    assert.equal((await db.get("SELECT count(*) AS n FROM t"))!.n, 2000n);
  } finally {
    setAfterPersistHook(null);
    await db.close();
    rmSync(directory, { recursive: true, force: true });
  }
});

test("row close releases its snapshot pin once when source cleanup fails", () => {
  let released = 0;
  const rows = new Rows(
    [],
    [],
    Cursor.streaming({
      nextRow: () => undefined,
      cost: () => 0n,
      queryAccount: () => newQueryAccount(0n),
      guardCost: () => {},
      close: () => {
        throw new Error("source cleanup failed");
      },
    }),
    null,
  );
  rows.attachPin(() => {
    released++;
  });
  assert.throws(() => rows.close(), /source cleanup failed/);
  assert.throws(() => rows.close(), /source cleanup failed/);
  assert.equal(released, 1);

  const detached = new Rows([], [], Cursor.buffered([], 0n, newQueryAccount(0n)), null);
  detached.attachPin(() => {
    released++;
    throw new Error("pin cleanup failed");
  });
  assert.throws(() => detached.close(), /pin cleanup failed/);
  detached.close();
  assert.equal(released, 2);
});
