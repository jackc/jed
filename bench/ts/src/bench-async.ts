// Dependency-free boundary benchmark: the synchronous TS core and the same core behind the
// production AsyncDatabase/RuntimeConnection proxies in a real Node execution worker.
import assert from "node:assert/strict";
import { mkdirSync, writeFileSync } from "node:fs";
import { cpus, platform, release, arch } from "node:os";
import { join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { Worker } from "node:worker_threads";
import type { Rows } from "../../../impl/ts/src/api.ts";
import {
  AsyncDatabase,
  type AsyncRows,
  RuntimeConnection,
} from "../../../impl/ts/src/async/client.ts";
import { createDatabase as createAsync } from "../../../impl/ts/src/async/node.ts";
import {
  type RuntimeReply,
  type RuntimeTransport,
  runtimeLimits,
} from "../../../impl/ts/src/async/protocol.ts";
import { createDatabase as createDirect } from "../../../impl/ts/src/file.ts";
import type { Value } from "../../../impl/ts/src/value.ts";

function setting(name: string, fallback: number): number {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isSafeInteger(value) || value < 1)
    throw new Error(`${name} must be a positive integer`);
  return value;
}

const config = {
  samples: setting("JED_ASYNC_SAMPLES", 200),
  warmup: setting("JED_ASYNC_WARMUP", 20),
  batchSamples: setting("JED_ASYNC_BATCH_SAMPLES", 30),
  largeSamples: setting("JED_ASYNC_LARGE_SAMPLES", 15),
  startupSamples: setting("JED_ASYNC_STARTUP_SAMPLES", 10),
  rows: setting("JED_ASYNC_ROWS", 4096),
  heartbeatRows: setting("JED_ASYNC_HEARTBEAT_ROWS", 250_000),
  batchSize: 100,
  limits: runtimeLimits(),
};
const directory = resolve(process.argv[2] ?? `bench/results/ts-async-${Date.now()}`);
mkdirSync(directory, { recursive: true });

type Summary = { samples: number; p50: number; p95: number; p99: number; min: number; max: number };
function summary(samples: number[]): Summary {
  const sorted = [...samples].sort((a, b) => a - b);
  const percentile = (p: number) => sorted[Math.max(0, Math.ceil(sorted.length * p) - 1)];
  return {
    samples: samples.length,
    p50: percentile(0.5),
    p95: percentile(0.95),
    p99: percentile(0.99),
    min: sorted[0],
    max: sorted.at(-1)!,
  };
}

type TransportCounts = {
  requests: number;
  fetches: number;
  batches: number;
  rows: number;
  maxBatchRows: number;
};
function emptyCounts(): TransportCounts {
  return { requests: 0, fetches: 0, batches: 0, rows: 0, maxBatchRows: 0 };
}
let counts = emptyCounts();

async function measuredWorker(): Promise<AsyncDatabase> {
  const worker = new Worker(new URL("../../../impl/ts/src/async/node-worker.ts", import.meta.url));
  const transport: RuntimeTransport = {
    postMessage(request) {
      counts.requests++;
      if (request.op === "fetch") counts.fetches++;
      worker.postMessage(request);
    },
    onMessage(receive) {
      worker.on("message", (reply: RuntimeReply) => {
        if (
          reply.ok &&
          reply.result !== null &&
          typeof reply.result === "object" &&
          "rows" in reply.result
        ) {
          const rows = (reply.result as { rows: unknown[] }).rows;
          counts.batches++;
          counts.rows += rows.length;
          counts.maxBatchRows = Math.max(counts.maxBatchRows, rows.length);
        }
        receive(reply);
      });
    },
    onFailure(fail) {
      worker.on("error", fail);
      worker.on("messageerror", fail);
      worker.on("exit", (code) => fail(new Error(`worker exited ${code}`)));
    },
    async terminate() {
      await worker.terminate();
    },
  };
  const connection = new RuntimeConnection(transport, config.limits, true);
  try {
    await connection.call("open", { mode: "create", options: {}, limits: connection.limits });
    return new AsyncDatabase(connection);
  } catch (error) {
    await connection.close();
    throw error;
  }
}

type Drained = { rows: number; checksum: bigint; cost: bigint };
function rowChecksum(row: Value[]): bigint {
  let sum = 0n;
  for (const value of row) {
    if (value.kind === "int") sum += value.int;
    else if (value.kind === "decimal") sum += BigInt(value.dec.render().length);
    else if (value.kind === "bytea") sum += BigInt(value.bytes.length);
    else if (value.kind === "array") sum += rowChecksum(value.elements);
  }
  return sum;
}
function drainDirect(rows: Rows): Drained {
  let count = 0;
  let checksum = 0n;
  try {
    for (const row of rows) {
      count++;
      checksum += rowChecksum(row);
    }
    return { rows: count, checksum, cost: rows.cost };
  } finally {
    rows.close();
  }
}
async function drainAsync(rows: AsyncRows): Promise<Drained> {
  let count = 0;
  let checksum = 0n;
  try {
    for await (const row of rows) {
      count++;
      checksum += rowChecksum(row);
    }
    return { rows: count, checksum, cost: rows.cost };
  } finally {
    await rows.close();
  }
}

type Probe = {
  name: string;
  operations: number;
  directUs: Summary;
  workerUs: Summary;
  transport: TransportCounts;
  rawSamplesUs: { direct: number[]; worker: number[] };
};
const probes: Probe[] = [];
async function probe<T>(
  name: string,
  direct: () => T,
  worker: () => Promise<T>,
  samples = config.samples,
  operations = 1,
): Promise<void> {
  for (let index = 0; index < config.warmup; index++) assert.deepEqual(await worker(), direct());
  counts = emptyCounts();
  const directSamples: number[] = [];
  const workerSamples: number[] = [];
  for (let index = 0; index < samples; index++) {
    let directResult!: T;
    let workerResult!: T;
    const runDirect = () => {
      const start = performance.now();
      directResult = direct();
      directSamples.push((performance.now() - start) * 1000);
    };
    const runWorker = async () => {
      const start = performance.now();
      workerResult = await worker();
      workerSamples.push((performance.now() - start) * 1000);
    };
    // Alternate order to avoid always giving one path the first/last slot in a changing host load.
    if (index % 2 === 0) {
      runDirect();
      await runWorker();
    } else {
      await runWorker();
      runDirect();
    }
    assert.deepEqual(
      workerResult,
      directResult,
      `${name}: values, command tags and logical cost must agree`,
    );
  }
  probes.push({
    name,
    operations,
    directUs: summary(directSamples),
    workerUs: summary(workerSamples),
    transport: { ...counts },
    rawSamplesUs: { direct: directSamples, worker: workerSamples },
  });
  process.stdout.write(
    `${name}: direct p50 ${summary(directSamples).p50.toFixed(2)} us; worker p50 ${summary(workerSamples).p50.toFixed(2)} us\n`,
  );
}

const delay = (milliseconds: number) =>
  new Promise<void>((resolveDelay) => setTimeout(resolveDelay, milliseconds));
type Heartbeat = {
  durationMs: number;
  ticksDuringWork: number;
  intervalMs: number;
  gapMs: Summary;
};
async function heartbeat(work: () => unknown | Promise<unknown>): Promise<Heartbeat> {
  const timestamps: number[] = [];
  const intervalMs = 2;
  const timer = setInterval(() => timestamps.push(performance.now()), intervalMs);
  await delay(10);
  const start = performance.now();
  try {
    await work();
  } catch (error) {
    clearInterval(timer);
    throw error;
  }
  // Keep the first delayed callback after a synchronous block inside the observed window.
  const end = performance.now();
  await delay(10);
  clearInterval(timer);
  const gaps = timestamps.slice(1).map((time, index) => time - timestamps[index]);
  return {
    durationMs: end - start,
    ticksDuringWork: timestamps.filter((time) => time >= start && time <= end).length,
    intervalMs,
    gapMs: summary(gaps),
  };
}

const directStartup: number[] = [];
const workerStartup: number[] = [];
for (let index = 0; index < config.startupSamples; index++) {
  let start = performance.now();
  const direct = createDirect();
  directStartup.push((performance.now() - start) * 1000);
  direct.close();
  start = performance.now();
  const worker = await createAsync();
  workerStartup.push((performance.now() - start) * 1000);
  await worker.close();
}

const directDb = createDirect();
const direct = directDb.session();
const workerDb = await measuredWorker();
const worker = await workerDb.session();
let heartbeats!: { direct: Heartbeat; worker: Heartbeat };
try {
  await probe(
    "cheap-query",
    () => drainDirect(direct.query("SELECT 1")),
    async () => drainAsync(await worker.query("SELECT 1")),
  );
  const preparedDirect = direct.prepareStatement("SELECT $1::i64 + 1");
  const preparedWorker = await worker.prepare("SELECT $1::i64 + 1");
  const params: Value[] = [{ kind: "int", int: 42n }];
  await probe(
    "prepared-execute",
    () => direct.executePrepared(preparedDirect, params),
    () => preparedWorker.execute(params),
  );
  await preparedWorker.close();
  const batch = Array.from({ length: config.batchSize }, () => ({ sql: "SELECT 1" }));
  await probe(
    "batch-100",
    () => batch.map(({ sql }) => direct.execute(sql)),
    () => worker.batch(batch),
    config.batchSamples,
    config.batchSize,
  );
  const large = `SELECT g::i64 AS id, g::decimal AS exact, decode('0001feff','hex') AS bytes, ARRAY[g,g+1] AS pair, NULL::text AS absent FROM generate_series(1,${config.rows}) g`;
  await probe(
    "large-typed-result",
    () => drainDirect(direct.query(large)),
    async () => drainAsync(await worker.query(large)),
    config.largeSamples,
    config.rows,
  );
  const heavy = `SELECT sum(g::decimal) FROM generate_series(1,${config.heartbeatRows}) g`;
  // Warm each heavy path before observing timer delivery.
  assert.deepEqual(await worker.execute(heavy), direct.execute(heavy));
  heartbeats = {
    direct: await heartbeat(() => direct.execute(heavy)),
    worker: await heartbeat(() => worker.execute(heavy)),
  };
} finally {
  direct.close();
  directDb.close();
  await worker.close();
  await workerDb.close();
}

const results = {
  measuredAt: new Date().toISOString(),
  revision: process.env.JED_BENCH_REVISION ?? "unknown",
  workingTree: process.env.JED_BENCH_DIRTY === "true" ? "modified" : "clean",
  host: {
    node: process.version,
    platform: platform(),
    release: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model,
    cpus: cpus().length,
  },
  config,
  startup: {
    directUs: summary(directStartup),
    workerUs: summary(workerStartup),
    rawSamplesUs: { direct: directStartup, worker: workerStartup },
  },
  probes,
  heartbeats,
};
writeFileSync(join(directory, "results.json"), `${JSON.stringify(results, null, 2)}\n`);
const number = (value: number) => value.toFixed(2);
const tableRows = probes.flatMap((entry) => [
  `| ${entry.name} | Direct core | ${entry.directUs.samples} | ${number(entry.directUs.p50)} | ${number(entry.directUs.p95)} | ${number(entry.directUs.p99)} |`,
  `| ${entry.name} | Node worker | ${entry.workerUs.samples} | ${number(entry.workerUs.p50)} | ${number(entry.workerUs.p95)} | ${number(entry.workerUs.p99)} |`,
]);
const report = `# TypeScript async boundary measurements

Measured ${results.measuredAt}, base revision ${results.revision} (${results.workingTree} working tree), ${results.host.node}, ${results.host.platform}/${results.host.architecture}, ${results.host.cpu}.

${results.workingTree === "modified" ? "The measurements include the uncommitted async implementation; the revision identifies its base commit.\n\n" : ""}Run with \`mise run bench:ts_async\`. This standalone harness adds no dependencies. Settings are in results.json; JED_ASYNC_SAMPLES, JED_ASYNC_WARMUP, JED_ASYNC_BATCH_SAMPLES, JED_ASYNC_LARGE_SAMPLES, JED_ASYNC_STARTUP_SAMPLES, JED_ASYNC_ROWS and JED_ASYNC_HEARTBEAT_ROWS override them. JED_ASYNC_RESULTS chooses the output directory.

The direct path uses the synchronous internal TS core. The worker path uses the same production AsyncDatabase and RuntimeConnection proxies over a real Node execution worker; its transport wrapper only counts requests and returned rows. Startup uses the public createDatabase factory. Both use in-memory storage and persistent sessions. Samples alternate direct/worker order, follow ${config.warmup} warmups, and check values, command tags and logical costs for equality outside the timed region. Cheap-query and large-result times include complete typed iteration and cursor close. Prepared-execute drains/discards results in the worker. Batch-100 is 100 autocommit SELECT statements in one request; its latency is for the whole batch.

## Caller-observed latency

All values are microseconds. Percentiles use nearest-rank selection; sample counts limit tail precision.

| Probe | Boundary | Samples | p50 µs | p95 µs | p99 µs |
|---|---|---:|---:|---:|---:|
${tableRows.join("\n")}
| create in-memory database | Direct core | ${directStartup.length} | ${number(results.startup.directUs.p50)} | ${number(results.startup.directUs.p95)} | ${number(results.startup.directUs.p99)} |
| create in-memory database | Node worker | ${workerStartup.length} | ${number(results.startup.workerUs.p50)} | ${number(results.startup.workerUs.p95)} | ${number(results.startup.workerUs.p99)} |

## Bounded result delivery

The large result contains ${config.rows} rows with exact integers, decimals, bytes, arrays and NULL. Transport limits are ${config.limits.maxBatchRows} rows and ${config.limits.maxBatchBytes} budget bytes per fetch, with ${config.limits.maxResultBytes} client result credits. A final empty fetch is expected when the result ends exactly at a row boundary.

| Probe | Requests | Fetch requests | Result batches | Rows delivered | Largest batch |
|---|---:|---:|---:|---:|---:|
${probes.map((entry) => `| ${entry.name} | ${entry.transport.requests} | ${entry.transport.fetches} | ${entry.transport.batches} | ${entry.transport.rows} | ${entry.transport.maxBatchRows} |`).join("\n")}

## Application event loop

A ${heartbeats.direct.intervalMs} ms interval runs while a warmed decimal aggregate over ${config.heartbeatRows} generated rows executes. The gap includes the first delayed callback after synchronous work returns. This observes responsiveness separately from operation completion latency.

| Boundary | SQL wall time ms | Heartbeats during SQL | Median timer gap ms | p95 timer gap ms | Largest timer gap ms |
|---|---:|---:|---:|---:|---:|
| Direct core | ${number(heartbeats.direct.durationMs)} | ${heartbeats.direct.ticksDuringWork} | ${number(heartbeats.direct.gapMs.p50)} | ${number(heartbeats.direct.gapMs.p95)} | ${number(heartbeats.direct.gapMs.max)} |
| Node worker | ${number(heartbeats.worker.durationMs)} | ${heartbeats.worker.ticksDuringWork} | ${number(heartbeats.worker.gapMs.p50)} | ${number(heartbeats.worker.gapMs.p95)} | ${number(heartbeats.worker.gapMs.max)} |

These are warm in-memory measurements on this host. They do not measure durable commits, cold file reads, browser OPFS, cancellation latency, parallel readers, encryption, or another architecture. Result batching bounds delivery; the aggregate still uses the core's existing blocking execution path.
`;
writeFileSync(join(directory, "report.md"), report);
process.stdout.write(`Results: ${directory}\n`);
