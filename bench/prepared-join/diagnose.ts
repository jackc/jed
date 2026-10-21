// Untimed cache counters and sampled V8 allocations. Never use these runs as latency samples.
import { readFileSync } from "node:fs";
import { Session as Inspector } from "node:inspector";
import { Engine, execute, prepare, queryPrepared, intValue } from "../../impl/ts/src/tooling.ts";
import { planMemoKey } from "../../impl/ts/src/plan_params.ts";
const db = new Engine();
execute(db, "CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)");
execute(
  db,
  `INSERT INTO items VALUES ${Array.from({ length: 1000 }, (_, i) => `(${i},${i % 10},${(i * 37) % 10000},'row${i}')`).join(",")}`,
);
const plan = JSON.parse(
  readFileSync(process.env.JOIN_PLAN ?? "bench/prepared-join/plan.json", "utf8"),
);
for (const sql of plan.setup ?? []) execute(db, sql);
const inspector = new Inspector();
inspector.connect();
const post = (method: string, params = {}): Promise<any> =>
  new Promise((resolve, reject) =>
    inspector.post(method, params, (err, result) => (err ? reject(err) : resolve(result))),
  );
const engine = db as any;
const original = engine.planQuery;
let planningCalls = 0;
const noAlloc = process.env.JOIN_NO_ALLOC === "1";
let scannedRows = 0;
if (noAlloc) {
  const store = engine.lkpStore("items");
  for (const method of ["rangeScanWithUnitsSteps", "scanWithUnitsSteps"]) {
    const originalScan = store[method];
    store[method] = function* (...args: any[]) {
      const result = yield* originalScan.apply(this, args);
      scannedRows += result.entries.length;
      return result;
    };
  }
}

engine.planQuery = function (...args: any[]) {
  planningCalls++;
  return original.apply(this, args);
};
for (const w of plan.workloads) {
  const stmt = prepare(db, w.sql);
  const run = (i: number) => {
    const r = queryPrepared(db, stmt, [intValue(BigInt(w.offset + ((i * w.step) % w.span)))]);
    let n = 0;
    for (const row of r) n++;
    r.close();
    return n;
  };
  for (let i = 0; i < w.warmup; i++) run(i);
  planningCalls = 0;
  scannedRows = 0;
  let hits = 0,
    misses = 0,
    rows = 0;
  for (let i = 0; i < w.iterations * 5; i++) {
    const index = i % w.iterations;
    const c = (stmt as any).scHolder.cache;
    const key = planMemoKey([intValue(BigInt(w.offset + ((index * w.step) % w.span)))]);
    if (c && (!c.sensitive || c.memo.some((m: any) => m.key === key))) hits++;
    else misses++;
    rows += run(index);
  }
  const c = (stmt as any).scHolder.cache;
  const calls = planningCalls;
  const scanned = noAlloc ? scannedRows : null;
  planningCalls = 0;
  const unseenCount = [
    "indexed_join",
    "join_text_predicate",
    "join_text_projection",
    "join_integer_outputs",
  ].includes(w.name)
    ? 50
    : 0;
  let unseenHits = 0;
  for (let i = w.warmup; i < w.warmup + unseenCount; i++) {
    const c = (stmt as any).scHolder.cache;
    const key = planMemoKey([intValue(BigInt(w.offset + ((i * w.step) % w.span)))]);
    if (c && (!c.sensitive || c.memo.some((m: any) => m.key === key))) unseenHits++;
    run(i);
  }
  const unseenPlanningCalls = planningCalls;

  let sampledBytesPerExecution: number | null = null;
  if (!noAlloc) {
    for (let i = 0; i < 1000; i++) run(i % w.iterations);
    await post("HeapProfiler.startSampling", {
      samplingInterval: 256,
      includeObjectsCollectedByMajorGC: true,
      includeObjectsCollectedByMinorGC: true,
    });
    const iterations = w.iterations * 20;
    for (let i = 0; i < iterations; i++) run(i % w.iterations);
    const { profile } = await post("HeapProfiler.stopSampling");
    sampledBytesPerExecution =
      profile.samples.reduce((n: number, s: any) => n + s.size, 0) / iterations;
  }
  console.log(
    JSON.stringify({
      workload: w.name,
      sensitive: c?.sensitive,
      nullRead: c?.nullRead,
      memo: c?.memo.length,
      executions: w.iterations * 5,
      hits,
      misses,
      planningCalls: calls,
      unseenExecutions: unseenCount,
      unseenHits,
      unseenPlanningCalls,
      rows,
      scannedRows: scanned,
      sampledBytesPerExecution,
    }),
  );
}
inspector.disconnect();
