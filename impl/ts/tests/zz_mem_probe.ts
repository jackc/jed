// EXPERIMENT (not part of the core's test contract, and not matched by the suite's tests/*.test.ts):
// the live heap a file database holds after its tables have been read, against the bytes of the pages
// read and the handle's cache_bytes. The statements are those of the Rust probe
// (impl/rust/examples/zz_mem_probe.rs), run on the Go experiment's sb.jed (TestSuperBlockExperiment).
// It also prints what the buffer pool charged (PackedLeaf.held), so the per-leaf object estimate in
// format.ts (LEAF_OBJECT_BYTES, REGION_OBJECT_BYTES) can be checked against the heap.
//
//   JED_MEM_PROBE=/path/sb.jed [JED_MEM_PROBE_CACHE=<bytes>] node --expose-gc tests/zz_mem_probe.ts

import { statSync } from "node:fs";
import { openDatabase, type Database } from "../src/lib.ts";

const path = process.env.JED_MEM_PROBE;
if (path === undefined) process.exit(0);
const gc = (globalThis as { gc?: () => void }).gc;
if (gc === undefined) throw new Error("run with node --expose-gc");
const cacheBytes = Number(process.env.JED_MEM_PROBE_CACHE ?? 1 << 30);

// live is the heap in use after a full collection: V8's heap plus the ArrayBuffers (page bytes).
function live(): number {
  gc!();
  gc!();
  const m = process.memoryUsage();
  return m.heapUsed + m.arrayBuffers;
}
const mb = (x: number): string => (x / (1 << 20)).toFixed(1);

// charged and leaves read the pool through the handle's internals (an experiment, not an API).
type Internals = {
  core: {
    storage: { paging: { residentBytes?(): number; residentLeaves(): number } | null };
  };
};
function pool(db: Database): { charged: number; leaves: number } {
  const p = (db as unknown as Internals).core.storage.paging!;
  return { charged: p.residentBytes?.() ?? Number.NaN, leaves: p.residentLeaves() };
}

const fileMb = statSync(path).size / (1 << 20);
console.log(`file ${fileMb.toFixed(1)} MB, cache_bytes ${mb(cacheBytes)} MB`);
const base = live();
const db = openDatabase(path, { readOnly: true, cacheBytes });
const sess = db.session();
const opened = live();
console.log(`live heap after open: ${mb(opened - base)} MB`);
const tables = ["orders", "line_items", "events", "users", "sessions", "activity", "samples"];
const run = (sql: string): void => {
  for (const _ of sess.query(sql, [])) {
    // drained: each row is dropped as it is read
  }
};
const report = (what: string): void => {
  const held = live() - opened;
  const { charged, leaves } = pool(db);
  console.log(
    `${what}: live heap ${mb(held)} MB beyond the open handle; pool charged ${mb(charged)} MB over ${leaves} leaves`,
  );
};
for (const t of tables) run(`SELECT count(*) FROM ${t} WHERE true`);
report("after count(*) of every table");
for (const t of tables) run(`SELECT * FROM ${t}`);
report("after SELECT * of every table");
for (const q of [
  "SELECT count(*) FROM orders WHERE customer_id >= 0",
  "SELECT count(*) FROM orders WHERE status >= ''",
  "SELECT count(*) FROM line_items WHERE product_id >= 0",
  "SELECT count(*) FROM events WHERE device_id >= 0",
  "SELECT count(*) FROM users WHERE email >= ''",
]) {
  run(q);
}
report("after index range scans");
sess.close();
db.close();
