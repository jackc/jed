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
import { encodeValues } from "../src/async/wire.ts";
import { Decimal } from "../src/decimal.ts";
import type { JsParam } from "../src/ergonomic.ts";
import type { RuntimeReply, RuntimeRequest, RuntimeTransport } from "../src/async/protocol.ts";

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
    this.message({ version: 1, id: request.id, ok: true, result });
  }
  reject(request: RuntimeRequest): void {
    this.message({
      version: 1,
      id: request.id,
      ok: false,
      error: encodeError(new TransportError("QUEUE_FULL", "full", "not-started")),
    });
  }
}

const queueFull = (error: unknown): boolean =>
  error instanceof TransportError && error.code === "QUEUE_FULL";

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
      version: 1,
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
      version: 1,
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
      rows: [encodeValues([{ kind: "int", int: 1n }]), encodeValues([{ kind: "int", int: 2n }])],
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

test("a malformed empty batch closes its worker cursor and releases its reservation", async () => {
  const transport = new FakeTransport();
  const connection = new RuntimeConnection(transport, { maxBatchBytes: 512, maxResultBytes: 512 });
  transport.handle = (request) =>
    transport.reply(request, request.op === "fetch" ? { rows: [], done: false, cost: 0n } : null);
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
              rows: [encodeValues([{ kind: "decimal", dec: Decimal.zero(16383) }])],
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
      version: 1,
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
