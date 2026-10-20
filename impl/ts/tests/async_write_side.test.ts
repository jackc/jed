// The write side's asynchronous storage paths (spec/design/ts-runtime.md §11): commit, recovery, and
// maintenance driven by runAsync over a device and page codec whose synchronous forms throw must
// await every page operation and still produce the synchronous control's results, cost, and bytes.
// Host-API/byte-level coverage the corpus cannot express (CLAUDE.md §10).
import assert from "node:assert/strict";
import { test } from "node:test";
import { AsyncDatabase, RuntimeConnection } from "../src/async/client.ts";
import type { RuntimeReply, RuntimeTransport } from "../src/async/protocol.ts";
import { RuntimeServer } from "../src/async/server.ts";
import { Environment } from "../src/environment.ts";
import { runAsync, runSync, type Steps } from "../src/execution.ts";
import { loadEnginePagedSteps, serializationRetries, toImage } from "../src/format.ts";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";
import { Pager, type PageTransform } from "../src/pager.ts";
import { SharedPaging } from "../src/paging.ts";
import { persistImpl, persistSteps } from "../src/persist.ts";
import { Database, type Session } from "../src/shared.ts";
import { Engine, execute } from "../src/tooling.ts";
import { render, textValue } from "../src/value.ts";
import { fillerText } from "./util.ts";

const PAGE = 512;

function turn(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

// A byte device whose write side is asynchronous. strict rejects every synchronous data call, so a
// path that skipped an await fails loudly; size() stays synchronous metadata (§11.4).
class AsyncDevice extends MemoryBlockStore {
  strict = true;
  readonly ops = { read: 0, write: 0, sync: 0, setSize: 0 };
  onWrite?: (offset: number) => void;

  private refuse(what: string): void {
    if (this.strict) throw new Error(`synchronous device ${what}`);
  }
  override readAt(offset: number, len: number): Uint8Array {
    this.refuse("read");
    return super.readAt(offset, len);
  }
  override writeAt(offset: number, bytes: Uint8Array): void {
    this.refuse("write");
    super.writeAt(offset, bytes);
  }
  override sync(): void {
    this.refuse("sync");
  }
  override setSize(bytes: number): void {
    this.refuse("setSize");
    super.setSize(bytes);
  }
  async readAtAsync(offset: number, len: number): Promise<Uint8Array> {
    await turn();
    this.ops.read++;
    return super.readAt(offset, len);
  }
  async writeAtAsync(offset: number, bytes: Uint8Array): Promise<void> {
    await turn();
    this.ops.write++;
    this.onWrite?.(offset);
    super.writeAt(offset, bytes);
  }
  async syncAsync(): Promise<void> {
    await turn();
    this.ops.sync++;
  }
  async setSizeAsync(bytes: number): Promise<void> {
    await turn();
    this.ops.setSize++;
    super.setSize(bytes);
  }
  image(): Uint8Array {
    return super.readAt(0, this.size());
  }
}

// An identity page codec with asynchronous forms only: it changes no byte, but every page crossing
// the seam must await it. inFlight tracks the bounded encode window (§11.3).
class AsyncIdentity implements PageTransform {
  encoded = 0;
  decoded = 0;
  inFlight = 0;
  maxInFlight = 0;
  async encodeAsync(_index: number, page: Uint8Array): Promise<Uint8Array> {
    this.inFlight++;
    this.maxInFlight = Math.max(this.maxInFlight, this.inFlight);
    await turn();
    this.inFlight--;
    this.encoded++;
    return page.slice();
  }
  async decodeAsync(_index: number, stored: Uint8Array): Promise<Uint8Array> {
    await turn();
    this.decoded++;
    return stored.slice();
  }
}

function seedImage(): Uint8Array {
  const engine = new Engine();
  engine.pageSize = PAGE;
  execute(engine, "CREATE TABLE t (id i32 PRIMARY KEY, a text, b text)");
  execute(engine, "CREATE INDEX t_b ON t (b)");
  execute(engine, "CREATE TABLE k (v i32)"); // keyless: open reconstructs its rowid from the leaves
  execute(engine, "CREATE TABLE r (id i32 PRIMARY KEY, span i32range)");
  execute(engine, "CREATE INDEX r_gist ON r USING gist (span)");
  execute(engine, "BEGIN");
  for (let i = 0; i < 60; i++) {
    execute(engine, `INSERT INTO t VALUES (${i}, '${fillerText(40 + i)}', 'b${i}')`);
    execute(engine, `INSERT INTO k VALUES (${i})`);
    execute(engine, `INSERT INTO r VALUES (${i}, i32range(${i}, ${i + 5}))`);
  }
  execute(engine, "COMMIT");
  return toImage(engine.committed, PAGE, engine.committed.txid);
}

type Mode = { device: MemoryBlockStore; transform: PageTransform | null };

function* openSteps(mode: Mode, relaxed: boolean): Steps<Database> {
  const pager = yield* Pager.fromStoreSteps(mode.device);
  pager.setPageTransform(mode.transform);
  // A tiny pool keeps leaves faulting through the device.
  const engine = yield* loadEnginePagedSteps(new SharedPaging(pager, 4), new Environment());
  engine.persistHook = persistImpl;
  engine.relaxed = relaxed;
  return Database.fromEngine(engine);
}

// The workload: growth, overflow chains, rewrites that keep or settle deferred values, deletes that
// orphan leaves and chains (the live-page delta, reclamation, trailing truncation), keyless and GiST
// tables, and an explicit multi-statement block.
const big = (n: number, len: number) => fillerText(len + n);
const WORKLOAD = [
  ...Array.from(
    { length: 30 },
    (_, i) => `INSERT INTO t VALUES (${100 + i}, '${big(i, 260)}', '${big(i, 250)}')`,
  ),
  "UPDATE t SET b = 'small' WHERE id >= 100 AND id < 110",
  "UPDATE t SET a = 'x' || a WHERE id >= 110 AND id < 115",
  "INSERT INTO k VALUES (1000)",
  "INSERT INTO r VALUES (1000, i32range(1, 2))",
  "BEGIN",
  "DELETE FROM t WHERE id < 40",
  "UPDATE t SET b = 'b' || id WHERE id >= 120",
  "COMMIT",
  "DELETE FROM t WHERE id >= 100",
  "DELETE FROM k WHERE v < 50",
];
const CHECKS = [
  "SELECT id, length(a), b FROM t ORDER BY id",
  "SELECT count(*), sum(v) FROM k",
  "SELECT id FROM r WHERE span && i32range(10, 12) ORDER BY id",
];

type Run = { costs: bigint[]; rows: string[][]; image: Uint8Array };

function rowsOf(session: Session, sql: string): string[] {
  const out: string[] = [];
  for (const row of session.query(sql)) out.push(row.map(render).join("|"));
  return out;
}

function* querySteps(session: Session, sql: string): Steps<string[]> {
  const rows = yield* session.querySteps(sql);
  const out: string[] = [];
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) return out;
      out.push(step.value.map(render).join("|"));
    }
  } finally {
    rows.close();
  }
}

function controlRun(relaxed: boolean): Run {
  const device = new MemoryBlockStore(seedImage());
  const db = runSync(openSteps({ device, transform: null }, relaxed));
  const session = db.session();
  const costs = WORKLOAD.map((sql) => session.execute(sql).cost);
  const rows = CHECKS.map((sql) => rowsOf(session, sql));
  session.close();
  db.close();
  return { costs, rows, image: device.readAt(0, device.size()) };
}

async function asyncRun(relaxed: boolean, device: AsyncDevice, codec: AsyncIdentity): Promise<Run> {
  const db = await runAsync(openSteps({ device, transform: codec }, relaxed));
  const session = db.session();
  const costs: bigint[] = [];
  for (const sql of WORKLOAD) costs.push((await runAsync(session.executeSteps(sql))).cost);
  const rows: string[][] = [];
  for (const sql of CHECKS) rows.push(await runAsync(querySteps(session, sql)));
  session.close();
  await runAsync(db.closeSteps());
  return { costs, rows, image: device.image() };
}

for (const relaxed of [false, true]) {
  test(`async write side: commit, recovery, and maintenance await every page operation (synchronous = ${relaxed ? "off" : "on"})`, async () => {
    const control = controlRun(relaxed);
    const device = new AsyncDevice(seedImage());
    const codec = new AsyncIdentity();
    const actual = await asyncRun(relaxed, device, codec);
    assert.deepEqual(actual.rows, control.rows);
    assert.deepEqual(actual.costs, control.costs);
    assert.deepEqual(actual.image, control.image, "file bytes match the synchronous control");
    assert.ok(device.ops.write > 0 && device.ops.sync > 0 && device.ops.setSize > 0);
    assert.ok(codec.encoded > 0 && codec.decoded > 0);
    assert.ok(codec.maxInFlight > 1 && codec.maxInFlight <= 16, "encoding overlaps in one window");

    // Recovery validates the last commit's manifest dependencies through the awaited window.
    const reopened = new AsyncDevice(actual.image);
    const reread = new AsyncIdentity();
    const db = await runAsync(openSteps({ device: reopened, transform: reread }, relaxed));
    const session = db.session();
    const rows: string[][] = [];
    for (const sql of CHECKS) rows.push(await runAsync(querySteps(session, sql)));
    session.close();
    await runAsync(db.closeSteps());
    assert.deepEqual(rows, control.rows);
    assert.ok(reopened.ops.read > 0 && reread.decoded > 0);
  });
}

// A stored row's deferred values always match its own plan (large-values.md §15), so SQL never makes
// an incremental serialization read a chain. Rewriting a row without settling it breaks that on
// purpose: the synchronous drain reads the chain inline, the async one through the chain cache.
function* unsettledRewrite(mode: Mode): Steps<void> {
  const pager = yield* Pager.fromStoreSteps(mode.device);
  pager.setPageTransform(mode.transform);
  const engine = yield* loadEnginePagedSteps(new SharedPaging(pager, 4), new Environment());
  engine.persistHook = persistImpl;
  const snap = engine.committed.clone();
  snap.txid = engine.committed.txid + 1n;
  const store = snap.stores.get("t")!;
  const [entry] = yield* store.entriesInKeyOrderSteps();
  const row = [...entry!.row];
  assert.equal(row[1]!.kind, "unfetched", "the large value stays deferred");
  row[2] = textValue("small");
  yield* store.replaceSteps(entry!.key, row);
  yield* persistSteps(engine, snap);
  engine.paging!.close();
}

test("async write side: a serialization chain miss is awaited and the pass repeated", async () => {
  const engine = new Engine();
  engine.pageSize = PAGE;
  execute(engine, "CREATE TABLE t (id i32 PRIMARY KEY, a text, b text)");
  execute(engine, `INSERT INTO t VALUES (1, '${fillerText(150)}', '${fillerText(150)}')`);
  const image = toImage(engine.committed, PAGE, engine.committed.txid);
  const control = new MemoryBlockStore(image);
  runSync(unsettledRewrite({ device: control, transform: null }));
  const device = new AsyncDevice(image);
  const before = serializationRetries.count;
  await runAsync(unsettledRewrite({ device, transform: new AsyncIdentity() }));
  assert.equal(serializationRetries.count, before + 1);
  assert.deepEqual(device.image(), control.readAt(0, control.size()));
});

test("async write side: a synchronous drain over an asynchronous-only codec fails explicitly", () => {
  const device = new MemoryBlockStore(seedImage());
  assert.throws(
    () => runSync(openSteps({ device, transform: new AsyncIdentity() }, false)),
    (error: Error) => error.message.includes("asynchronous runtime"),
  );
});

test("async write side: cancellation after the last safe checkpoint does not interrupt a commit", async () => {
  const device = new AsyncDevice(seedImage());
  const db = await runAsync(openSteps({ device, transform: new AsyncIdentity() }, false));
  const session = db.session();
  let canceled = false;
  device.onWrite = () => {
    canceled = true; // the request arrives while the commit's writes are in flight
  };
  const result = await runAsync(session.executeSteps("INSERT INTO t VALUES (500, 'a', 'b')"), {
    check: () => canceled,
    poll: () => canceled,
  });
  assert.equal(result.changes, 1);
  device.onWrite = undefined;
  assert.deepEqual(await runAsync(querySteps(session, "SELECT b FROM t WHERE id = 500")), ["b"]);
  session.close();
  await runAsync(db.closeSteps());

  // Before the checkpoint the same request discards the statement.
  const again = await runAsync(openSteps({ device, transform: new AsyncIdentity() }, false));
  const s2 = again.session();
  await assert.rejects(
    runAsync(s2.executeSteps("INSERT INTO t VALUES (501, 'a', 'b')"), { check: () => true }),
    (error: Error) => error.message.includes("57014"),
  );
  assert.deepEqual(await runAsync(querySteps(s2, "SELECT b FROM t WHERE id = 501")), []);
  s2.close();
  await runAsync(again.closeSteps());
});

test("async write side: a failed awaited barrier poisons the handle until reopen", async () => {
  const device = new AsyncDevice(seedImage());
  const db = await runAsync(openSteps({ device, transform: null }, false));
  const session = db.session();
  device.syncAsync = async () => {
    await turn();
    throw new Error("barrier failed");
  };
  await assert.rejects(
    runAsync(session.executeSteps("INSERT INTO t VALUES (600, 'a', 'b')")),
    /barrier failed/,
  );
  delete (device as { syncAsync?: unknown }).syncAsync;
  await assert.rejects(
    runAsync(session.executeSteps("INSERT INTO t VALUES (601, 'a', 'b')")),
    /58030|reopen/,
  );
  session.close();
  await runAsync(db.closeSteps());

  // Reopen recovers a complete generation: the unacknowledged commit may or may not be selected.
  const reopened = await runAsync(
    openSteps({ device: new AsyncDevice(device.image()), transform: null }, false),
  );
  const rows = await runAsync(querySteps(reopened.session(), "SELECT id FROM t WHERE id >= 600"));
  assert.ok(rows.length <= 1 && !rows.includes("601"));
  await runAsync(reopened.closeSteps());
});

test("async write side: the worker runtime commits and closes through the async storage stack", async () => {
  const device = new AsyncDevice(seedImage());
  const codec = new AsyncIdentity();
  const core = await runAsync(openSteps({ device, transform: codec }, true));
  let receive: (reply: RuntimeReply) => void = () => {};
  const server = new RuntimeServer({
    open: () => core,
    send: (reply, transfer) => {
      const copied = structuredClone(reply, { transfer });
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
  const connection = new RuntimeConnection(transport, {}, true);
  await connection.call("open", { mode: "open", limits: connection.limits });
  const db = new AsyncDatabase(connection);
  const session = await db.session();
  const tx = await session.begin(true);
  await tx.execute("INSERT INTO t VALUES (700, 'x', 'y')");
  await tx.commit();
  await session.execute("UPDATE t SET b = 'z' WHERE id = 700");
  await session.close();
  const writes = device.ops.write;
  await db.close(); // the relaxed batch's flush is an awaited barrier
  assert.ok(writes > 0 && device.ops.sync > 0 && codec.encoded > 0);

  const reopened = await runAsync(
    openSteps({ device: new AsyncDevice(device.image()), transform: null }, false),
  );
  const s = reopened.session();
  assert.deepEqual(await runAsync(querySteps(s, "SELECT b FROM t WHERE id = 700")), ["z"]);
  s.close();
  await runAsync(reopened.closeSteps());
});
