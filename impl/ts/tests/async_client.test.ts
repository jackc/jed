import assert from "node:assert/strict";
import { test } from "node:test";
import {
  AsyncPreparedStatement,
  AsyncRows,
  AsyncSession,
  AsyncTransaction,
  RuntimeConnection,
  acquireWorker,
} from "../src/async/client.ts";
import { encodeError, TransportError } from "../src/async/errors.ts";
import { PackedRowsWriter, type PackedRows } from "../src/async/packed.ts";
import type { Value } from "../src/value.ts";
import { Decimal } from "../src/decimal.ts";
import { engineError, EngineError } from "../src/errors.ts";
import type { JsParam } from "../src/ergonomic.ts";
import {
  RUNTIME_VERSION,
  type RuntimeReply,
  type RuntimeRequest,
  type RuntimeTransport,
} from "../src/async/protocol.ts";

class FakeTransport implements RuntimeTransport {
  requests: RuntimeRequest[] = [];
  terminated = false;
  message: (reply: RuntimeReply) => void = () => {};
  handle: (request: RuntimeRequest) => void = (request) => this.reply(request, null);
  postMessage(request: RuntimeRequest): void {
    this.requests.push(request);
    queueMicrotask(() => this.handle(request));
  }
  onMessage(handler: (reply: RuntimeReply) => void): void {
    this.message = handler;
  }
  onFailure(): void {}
  terminate(): void {
    this.terminated = true;
  }
  reply(request: RuntimeRequest, result: unknown): void {
    this.message({ version: RUNTIME_VERSION, id: request.id, ok: true, result });
  }
  reject(request: RuntimeRequest): void {
    this.message({
      version: RUNTIME_VERSION,
      id: request.id,
      ok: false,
      error: encodeError(new TransportError("QUEUE_FULL", "full", "not-started")),
    });
  }
}

const queueFull = (error: unknown): boolean =>
  error instanceof TransportError && error.code === "QUEUE_FULL";

function packedRows(rows: Value[][]): PackedRows {
  const writer = new PackedRowsWriter(1024 * 1024);
  for (const row of rows) assert.equal(writer.append(row), true);
  return writer.finish();
}

test("transaction cleanup and handle close remain retryable after rejected admission", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport);
  const rejected = new Set<string>();
  transport.handle = (request) => {
    if (!rejected.has(request.op)) {
      rejected.add(request.op);
      transport.reject(request);
    } else transport.reply(request, null);
  };
  const transaction = new AsyncTransaction(connection, 1, 2);
  await assert.rejects(transaction.commit(), queueFull);
  await assert.rejects(transaction.rollback(), queueFull);
  await transaction.rollback();
  assert.equal(transport.requests.filter((request) => request.op === "rollback").length, 2);
  const session = new AsyncSession(connection, 1);
  await assert.rejects(session.close(), queueFull);
  await session.close();
  const statement = new AsyncPreparedStatement(connection, 3, 1);
  await assert.rejects(statement.close(), queueFull);
  await statement.close();
});

test("malformed worker errors reject the matching request and all peers", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport);
  transport.handle = (request) => {
    transport.message({
      version: RUNTIME_VERSION,
      id: request.id,
      ok: false,
      error: null,
    } as unknown as RuntimeReply);
  };
  const calls = [connection.call("execute"), connection.call("execute")];
  const settled = await Promise.allSettled(calls);
  assert.ok(
    settled.every(
      (result) =>
        result.status === "rejected" &&
        result.reason instanceof TransportError &&
        result.reason.outcome === "unknown",
    ),
  );
  assert.equal(transport.terminated, true);
});

test("a reported worker failure invalidates peer requests and the connection", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport);
  transport.handle = (request) => {
    if (request.id !== 1) return;
    transport.message({
      version: RUNTIME_VERSION,
      id: request.id,
      ok: false,
      error: encodeError(new Error("worker invariant failed")),
    });
  };
  const failed = (error: unknown): boolean =>
    error instanceof TransportError &&
    error.code === "WORKER_FAILED" &&
    error.outcome === "unknown";
  const first = connection.call("execute");
  const second = connection.call("execute");
  await Promise.all([assert.rejects(first, failed), assert.rejects(second, failed)]);
  await assert.rejects(connection.call("execute"), failed);
  await connection.close();
  assert.equal(transport.terminated, true);
});

test("concurrent row reads serialize and close releases the batch reservation", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  transport.handle = (request) =>
    transport.reply(request, {
      rows: packedRows([[{ kind: "int", int: 1n }], [{ kind: "int", int: 2n }]]),
      cost: 42n,
      done: true,
    });
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: ["n"],
    columnTypes: ["i64"],
    rowsAffected: null,
    cost: 0n,
  });
  const first = rows.next();
  const second = rows.next();
  const closing = rows.close();
  assert.deepEqual((await first).value, [{ kind: "int", int: 1n }]);
  assert.deepEqual((await second).value, [{ kind: "int", int: 2n }]);
  await closing;
  assert.equal((await rows.next()).done, true);
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch"],
  );
});

test("resident reads preserve order across a queued fetch and close", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, {
    maxBatchBytes: 512,
    maxResultBytes: 512,
    maxQueuedRequests: 2,
  });
  let fetched = 0;
  let releaseFetch!: () => void;
  transport.handle = (request) => {
    if (request.op !== "fetch") {
      transport.reply(request, null);
      return;
    }
    const values = ++fetched === 1 ? [1n, 2n, 3n] : [4n, 5n];
    const reply = () =>
      transport.reply(request, {
        rows: packedRows(values.map((int) => [{ kind: "int", int }])),
        cost: BigInt(fetched),
        done: false,
      });
    if (fetched === 1) reply();
    else releaseFetch = reply;
  };
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: ["n"],
    columnTypes: ["i64"],
    rowsAffected: null,
    cost: 0n,
  });
  assert.deepEqual((await rows.next()).value, [{ kind: "int", int: 1n }]);
  const second = rows.next();
  const third = rows.next();
  const fourth = rows.next();
  const fifth = rows.next();
  await assert.rejects(rows.next(), queueFull);
  const closing = rows.close();
  assert.equal(fetched, 2);
  releaseFetch();
  assert.deepEqual(
    (await Promise.all([second, third, fourth, fifth])).map((result) => result.value),
    [2n, 3n, 4n, 5n].map((int) => [{ kind: "int", int }]),
  );
  await closing;
  assert.equal((await rows.next()).done, true);
  assert.equal(rows.cost, 2n);
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch", "fetch", "cursor.close"],
  );
});

test("a close queued ahead of a resident read discards the remaining batch", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  transport.handle = (request) =>
    transport.reply(
      request,
      request.op === "fetch"
        ? {
            rows: packedRows([[{ kind: "int", int: 1n }], [{ kind: "int", int: 2n }]]),
            cost: 2n,
            done: false,
          }
        : null,
    );
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: ["n"],
    columnTypes: ["i64"],
    rowsAffected: null,
    cost: 0n,
  });
  assert.equal((await rows.next()).done, false);
  const closing = rows.close();
  assert.equal((await rows.next()).done, true);
  await closing;
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch", "cursor.close"],
  );
});

test("abort discards resident rows and reaches a live worker cursor", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  const controller = new AbortController();
  let fetched = 0;
  transport.handle = (request) => {
    if (request.op !== "fetch") transport.reply(request, null);
    else if (++fetched === 1)
      transport.reply(request, {
        rows: packedRows([1n, 2n].map((int) => [{ kind: "int", int }])),
        cost: 2n,
        done: false,
      });
    else {
      assert.equal(request.aborted, true);
      transport.message({
        version: RUNTIME_VERSION,
        id: request.id,
        ok: false,
        error: encodeError(engineError("query_canceled", "canceled")),
      });
    }
  };
  const rows = new AsyncRows(
    connection,
    { cursor: 1, columnNames: ["n"], columnTypes: ["i64"], rowsAffected: null, cost: 0n },
    undefined,
    undefined,
    { signal: controller.signal },
  );
  assert.deepEqual((await rows.next()).value, [{ kind: "int", int: 1n }]);
  controller.abort();
  await assert.rejects(
    rows.next(),
    (error) => error instanceof EngineError && error.code() === "57014",
  );
  assert.equal((await rows.next()).done, true);
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch", "fetch", "cursor.close"],
  );
});

test("a malformed later value rejects the complete batch before its first row", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  const packet = packedRows([[{ kind: "int", int: 1n }], [{ kind: "int", int: 2n }]]);
  packet.data = packet.data.slice(0, -1);
  transport.handle = (request) =>
    transport.reply(
      request,
      request.op === "fetch"
        ? {
            rows: packet,
            done: true,
            cost: 2n,
          }
        : null,
    );
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: ["n"],
    columnTypes: ["i64"],
    rowsAffected: null,
    cost: 0n,
  });
  await assert.rejects(
    rows.next(),
    (error) => error instanceof TransportError && error.code === "PROTOCOL",
  );
  assert.equal((await rows.next()).done, true);
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch", "cursor.close"],
  );
});

test("a fetched batch owns decoded values before later pulls", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  const wire = packedRows([
    [{ kind: "bytea", bytes: new Uint8Array([1]) }],
    [{ kind: "bytea", bytes: new Uint8Array([2]) }],
  ]);
  transport.handle = (request) => transport.reply(request, { rows: wire, done: true, cost: 2n });
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: ["n"],
    columnTypes: ["bytea"],
    rowsAffected: null,
    cost: 0n,
  });
  assert.deepEqual((await rows.next()).value, [{ kind: "bytea", bytes: new Uint8Array([1]) }]);
  new Uint8Array(wire.data).fill(99);
  assert.deepEqual((await rows.next()).value, [{ kind: "bytea", bytes: new Uint8Array([2]) }]);
  assert.equal((await rows.next()).done, true);
  connection.reserveResult()();
});

test("a malformed empty batch closes its worker cursor and releases its reservation", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  transport.handle = (request) =>
    transport.reply(
      request,
      request.op === "fetch" ? { rows: packedRows([]), done: false, cost: 0n } : null,
    );
  const rows = new AsyncRows(connection, {
    cursor: 1,
    columnNames: [],
    columnTypes: [],
    rowsAffected: null,
    cost: 0n,
  });
  await assert.rejects(
    rows.next(),
    (error) => error instanceof TransportError && error.code === "PROTOCOL",
  );
  connection.reserveResult()();
  assert.deepEqual(
    transport.requests.map((request) => request.op),
    ["fetch", "cursor.close"],
  );
});

test("ergonomic binding preflights nested arrays before conversion and rejects huge option integers", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxQueuedBytes: 1024 });
  const session = new AsyncSession(connection, 1);
  const large = new Array<JsParam>(1000).fill(null);
  large.map = () => {
    throw new Error("mapping must follow admission");
  };
  for (const operation of [
    session.run.bind(session),
    session.get.bind(session),
    session.all.bind(session),
  ]) {
    await assert.rejects(
      operation("SELECT $1", large),
      (error) => error instanceof TransportError && error.code === "RESULT_TOO_LARGE",
    );
  }
  const cycle: JsParam[] = [];
  cycle.push(cycle);
  await assert.rejects(session.run("SELECT $1", cycle), TransportError);
  await assert.rejects(session.configure({ maxCost: 1n << 100_000n }), TransportError);
  assert.equal(transport.requests.length, 0);
});

test("ergonomic results bound decimal rendering before allocating its large text", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  transport.handle = (request) =>
    transport.reply(
      request,
      request.op === "query"
        ? { cursor: 1, columnNames: ["n"], columnTypes: ["decimal"], rowsAffected: null, cost: 0n }
        : request.op === "fetch"
          ? {
              rows: packedRows([[{ kind: "decimal", dec: Decimal.zero(16383) }]]),
              done: true,
              cost: 0n,
            }
          : null,
    );
  const session = new AsyncSession(connection, 1);
  await assert.rejects(
    session.get("SELECT 0::numeric(1000,1000)"),
    (error) => error instanceof TransportError && error.code === "RESULT_TOO_LARGE",
  );
  connection.reserveResult()();
});

test("failed connections retain their worker slot and close waits for platform termination", async () => {
  const transport = new FakeTransport();
  let finish!: () => void;
  const terminated = new Promise<void>((resolve) => {
    finish = resolve;
  });
  transport.terminate = () => terminated;
  const release = acquireWorker({ maxWorkers: 1 });
  let released = 0;
  const connection = new RuntimeConnection(transport, {}, false, () => {
    released++;
    release();
  });
  transport.handle = (request) =>
    transport.message({
      version: RUNTIME_VERSION,
      id: request.id,
      ok: false,
      error: null,
    } as unknown as RuntimeReply);
  await assert.rejects(connection.call("execute"), TransportError);
  let closed = false;
  const closing = connection.close().then(() => {
    closed = true;
  });
  await Promise.resolve();
  assert.equal(closed, false);
  assert.equal(released, 0);
  assert.throws(
    () => acquireWorker({ maxWorkers: 1 }),
    (error) => error instanceof TransportError && error.code === "WORKER_LIMIT",
  );
  finish();
  await closing;
  assert.equal(released, 1);
  acquireWorker({ maxWorkers: 1 })();
  await connection.close();
  assert.equal(released, 1);
});

test("request admission reports queue exhaustion separately from typed value encoding", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxQueuedBytes: 256 });
  await assert.rejects(connection.call("execute", { sql: "x".repeat(256) }), queueFull);
  assert.equal(transport.requests.length, 0);
});
