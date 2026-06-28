// stress is the TS runner for Layer 3 of the concurrency contract
// (spec/design/concurrency-testing.md §6): the parallelism-stress format. Unlike the Layer 1/2
// `# format: concurrency` schedules (an explicit total order, run inside the conformance harness),
// a `stress/*.stress.toml` file has NO order — writers and readers run concurrently and correctness
// is checked by INVARIANTS, not a transcript. It is bench-family (outside `rake ci`).
//
// The TS core is single-threaded (JS has no shared-memory threads for live objects), so it runs the
// **seeded-sequential interleaver** (§6) — the only mode here. The workers are flattened to a fixed
// index order and each modeled as a program of atomic ops (writer: acquire · exec… · commit; reader:
// open · check · close); at each step the shared splitmix64(seed) stream picks the next runnable
// worker. A writer's `acquire` is runnable only while the single-writer gate is free, so the
// interleaver never needs to truly block — the property that lets a single-threaded core run it at
// all (and why write() never throws 25001 here). Deterministic given the seed; reproduces the
// logical interleavings (isolation/atomicity/visibility) without CPU parallelism or memory races.
// A `parallel = "required"` file is skipped (reported), since this core cannot exercise it.

import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { parse as parseToml } from "smol-toml";

import { Database, type Session, render } from "../../../impl/ts/src/lib.ts";

import { Checksum, Prng } from "./lib.ts";

const LANG = "ts";
const CAN_THREAD = false; // single-threaded core → seeded-sequential interleaver only

// --- the stress file format (concurrency-testing.md §6) --------------------------------------

interface Worker {
  kind: string; // "writer" | "reader"
  count: number;
  iterations: number;
  op: string; // writer
  invariantQuery: string; // reader
  invariantExpect: string; // reader
}

interface StressFile {
  name: string;
  parallel: string;
  seed: bigint;
  setup: string[];
  workers: Worker[];
  finalQuery: string | null;
  finalExpect: number[][];
  crossCore: boolean;
}

interface StressResult {
  schema: number;
  name: string;
  lang: string;
  mode: string; // "threaded" | "sequential" | "skipped"
  status: string; // "pass" | "fail" | "skip"
  invariant_checks: number;
  writers: number;
  writer_iters: number;
  final_ok: boolean;
  checksum: string;
  cross_core_checksum: boolean;
  duration_ms: number;
  error?: string;
}

type Dict = Record<string, unknown>;

function parseStress(path: string): StressFile {
  const f = parseToml(readFileSync(path, "utf8")) as Dict;
  const meta = (f.meta ?? {}) as Dict;
  const setup = (f.setup ?? {}) as Dict;
  const workers: Worker[] = ((f.worker as Dict[] | undefined) ?? []).map((w) => ({
    kind: String(w.kind ?? ""),
    count: Number(w.count ?? 0),
    iterations: Number(w.iterations ?? 0),
    op: String(w.op ?? ""),
    invariantQuery: String(w.invariant_query ?? ""),
    invariantExpect: String(w.invariant_expect ?? ""),
  }));
  const fin = f.final as Dict | undefined;
  return {
    name: String(meta.name ?? ""),
    parallel: String(meta.parallel ?? "optional"),
    seed: BigInt((meta.seed as number | bigint | undefined) ?? 0),
    setup: ((setup.sql as unknown[] | undefined) ?? []).map(String),
    workers,
    finalQuery: fin?.query != null ? String(fin.query) : null,
    finalExpect: ((fin?.expect as unknown[][] | undefined) ?? []).map((row) => row.map(Number)),
    crossCore: Boolean(fin?.cross_core_checksum ?? false),
  };
}

// parseOp splits a writer's `op` into the executable statements: bare BEGIN/COMMIT/ROLLBACK are
// transaction MARKERS, mapped onto the handle's open/commit (§6), so they are dropped here.
function parseOp(op: string): string[] {
  return op
    .split(";")
    .map((s) => s.trim())
    .filter((s) => s.length > 0)
    .filter((s) => !["BEGIN", "COMMIT", "ROLLBACK"].includes(s.toUpperCase()));
}

// queryScalar runs a single-column, single-row query and renders the scalar to its canonical string
// (render — so the decimal `sum(bigint)` result `1000` renders identically across cores). The
// invariant is a string compare, not folded into the cross-core checksum.
function queryScalar(rh: Session, sql: string): string {
  for (const row of rh.query(sql, [])) {
    return render(row[0]);
  }
  throw new Error(`no row from ${sql}`);
}

// --- setup + the final check -----------------------------------------------------------------

function setup(db: Database, sql: string[]): void {
  const wh = db.writeSession();
  try {
    for (const s of sql) wh.execute(s, []);
    wh.commit();
  } catch (e) {
    wh.rollback();
    throw e;
  }
}

function checkFinal(db: Database, f: StressFile): { checksum: string; ok: boolean } {
  if (f.finalQuery === null) return { checksum: "", ok: true };
  const rh = db.readSession();
  try {
    const sum = new Checksum();
    const got: bigint[][] = [];
    for (const row of rh.query(f.finalQuery, [])) {
      const ints: bigint[] = [];
      for (const v of row) {
        if (v.kind === "int") {
          sum.int(v.int);
          ints.push(v.int);
        } else if (v.kind === "null") {
          sum.null();
        } else {
          throw new Error(`stress final query must return integer columns, got ${v.kind}`);
        }
      }
      sum.endRow();
      got.push(ints);
    }
    return { checksum: sum.hex(), ok: finalEqual(got, f.finalExpect) };
  } finally {
    rh.close();
  }
}

// finalEqual compares the observed final rows to the pinned expectation. An empty expectation means
// the workload is invariant-only (not confluent) — the exact-rows check is skipped.
function finalEqual(got: bigint[][], want: number[][]): boolean {
  if (want.length === 0) return true;
  if (got.length !== want.length) return false;
  for (let i = 0; i < got.length; i++) {
    if (got[i].length !== want[i].length) return false;
    for (let j = 0; j < got[i].length; j++) {
      if (got[i][j] !== BigInt(want[i][j])) return false;
    }
  }
  return true;
}

// --- the seeded-sequential interleaver (the §6 algorithm; identical in spirit to Go/Rust) ------

interface SeqWorker {
  kind: string;
  stmts: string[];
  query: string;
  expect: string;
  iterations: number;
  iter: number;
  op: number;
  wh: Session | null;
  rh: Session | null;
}

// runnable: the only gated op is a writer's acquire (op 0), which needs the gate free; every other
// op is always runnable, so the gate holder always progresses and there is no deadlock.
function runnable(w: SeqWorker, gateFree: boolean): boolean {
  if (w.iter >= w.iterations) return false;
  if (w.kind === "writer" && w.op === 0) return gateFree;
  return true;
}

interface State {
  gateHeld: boolean;
  checks: number;
}

function stepSeq(db: Database, w: SeqWorker, st: State): void {
  if (w.kind === "writer") {
    if (w.op === 0) {
      w.wh = db.writeSession(); // gate is free (guaranteed by runnable) — never throws 25001 here
      st.gateHeld = true;
      w.op++;
    } else if (w.op <= w.stmts.length) {
      w.wh!.execute(w.stmts[w.op - 1], []); // wh is set while op > 0
      w.op++;
    } else {
      w.wh!.commit();
      w.wh = null;
      st.gateHeld = false;
      w.op = 0;
      w.iter++;
    }
  } else {
    if (w.op === 0) {
      w.rh = db.readSession();
      w.op++;
    } else if (w.op === 1) {
      const got = queryScalar(w.rh!, w.query); // rh is set while op > 0
      st.checks++;
      if (got !== w.expect) {
        w.rh!.close();
        throw new Error(`invariant ${w.query}: got ${got}, want ${w.expect}`);
      }
      w.op++;
    } else {
      w.rh!.close(); // drop → deregister (advance the watermark)
      w.rh = null;
      w.op = 0;
      w.iter++;
    }
  }
}

function runSequential(db: Database, f: StressFile): number {
  const workers: SeqWorker[] = [];
  for (const wk of f.workers) {
    for (let c = 0; c < wk.count; c++) {
      workers.push({
        kind: wk.kind,
        stmts: wk.kind === "writer" ? parseOp(wk.op) : [],
        query: wk.invariantQuery,
        expect: wk.invariantExpect,
        iterations: wk.iterations,
        iter: 0,
        op: 0,
        wh: null,
        rh: null,
      });
    }
  }
  const prng = new Prng(f.seed);
  const st: State = { gateHeld: false, checks: 0 };
  for (;;) {
    const runnableIdx: number[] = [];
    for (let i = 0; i < workers.length; i++) {
      if (runnable(workers[i], !st.gateHeld)) runnableIdx.push(i);
    }
    if (runnableIdx.length === 0) break; // all done (the gate holder is always runnable)
    const idx = runnableIdx[Number(prng.next() % BigInt(runnableIdx.length))];
    stepSeq(db, workers[idx], st);
  }
  return st.checks;
}

// --- driving one file ------------------------------------------------------------------------

function runFile(path: string, forceSequential: boolean): StressResult {
  const start = performance.now();
  const res: StressResult = {
    schema: 1,
    name: "",
    lang: LANG,
    mode: "",
    status: "",
    invariant_checks: 0,
    writers: 0,
    writer_iters: 0,
    final_ok: false,
    checksum: "",
    cross_core_checksum: false,
    duration_ms: 0,
  };
  let f: StressFile;
  try {
    f = parseStress(path);
  } catch (e) {
    res.status = "fail";
    res.error = `parse ${path}: ${e instanceof Error ? e.message : String(e)}`;
    return res;
  }
  res.name = f.name;
  res.cross_core_checksum = f.crossCore;
  for (const wk of f.workers) {
    if (wk.kind === "writer") {
      res.writers += wk.count;
      res.writer_iters += wk.count * wk.iterations;
    }
  }

  const sequential = forceSequential || !CAN_THREAD;
  if (sequential && f.parallel === "required") {
    res.mode = "skipped";
    res.status = "skip";
    return res;
  }
  res.mode = sequential ? "sequential" : "threaded";

  try {
    const db = Database.newInMemory();
    setup(db, f.setup);
    res.invariant_checks = runSequential(db, f);
    res.duration_ms = Math.round(performance.now() - start);
    const { checksum, ok } = checkFinal(db, f);
    res.checksum = checksum;
    res.final_ok = ok;
    if (!ok) {
      res.status = "fail";
      res.error = "final state did not match [final].expect";
      return res;
    }
    res.status = "pass";
  } catch (e) {
    res.duration_ms = Math.round(performance.now() - start);
    res.status = "fail";
    res.error = e instanceof Error ? e.message : String(e);
  }
  return res;
}

function main(): void {
  const argv = process.argv.slice(2);
  let forceSequential = false;
  const positional: string[] = [];
  for (const a of argv) {
    if (a === "--sequential") forceSequential = true;
    else positional.push(a);
  }
  if (positional.length < 2) {
    process.stderr.write("usage: stress <stress_dir> <out_path> [name_filter] [--sequential]\n");
    process.exit(2);
  }
  const [stressDir, outPath] = positional;
  const filter = positional[2] ?? "";

  const files = readdirSync(stressDir)
    .filter((n) => n.endsWith(".stress.toml"))
    .filter((n) => filter === "" || n.includes(filter))
    .sort()
    .map((n) => join(stressDir, n));

  const lines: string[] = [];
  let exit = 0;
  for (const file of files) {
    const res = runFile(file, forceSequential);
    lines.push(JSON.stringify(res));
    if (res.status === "pass") {
      process.stderr.write(
        `  PASS  ${res.name.padEnd(36)} ${res.mode.padEnd(10)} checks=${res.invariant_checks} checksum=${res.checksum} (${res.duration_ms}ms)\n`,
      );
    } else if (res.status === "skip") {
      process.stderr.write(`  SKIP  ${res.name.padEnd(36)} ${res.mode}\n`);
    } else {
      exit = 1;
      process.stderr.write(`  FAIL  ${res.name.padEnd(36)} ${res.mode}: ${res.error}\n`);
    }
  }
  writeFileSync(outPath, `${lines.join("\n")}\n`);
  process.exit(exit);
}

main();
