// Shared plumbing for the TS benchmark harness binaries (spec/design/benchmarks.md).
// Mirrors bench/go/internal/bench: the splitmix64 param stream (over BigInt, masked to
// 64 bits), the FNV-1a answer checksum, corpus/dataset parsing, fingerprint checks, and
// the engine-agnostic run loop. Each src/bench-*.ts contributes only its driver. The
// runner is async because the PG driver is; the jed and SQLite engines are sync inside.

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { parse as parseToml } from "smol-toml";

const MASK = 0xffffffffffffffffn;

// --- splitmix64 (benchmarks.md §4; vectors pinned in tests/prng.test.ts) ---

export class Prng {
  private z: bigint;

  constructor(seed: bigint) {
    this.z = seed & MASK;
  }

  next(): bigint {
    this.z = (this.z + 0x9e3779b97f4a7c15n) & MASK;
    let x = this.z;
    x = ((x ^ (x >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK;
    x = ((x ^ (x >> 27n)) * 0x94d049bb133111ebn) & MASK;
    return x ^ (x >> 31n);
  }

  // Bounded draw in [lo, hi] — modulo bias accepted, identical everywhere.
  intUniform(lo: bigint, hi: bigint): bigint {
    const span = hi - lo + 1n;
    return lo + (this.next() % span);
  }

  // Lowercase ASCII string, length in [minLen, maxLen].
  text(minLen: bigint, maxLen: bigint): string {
    const n = Number(this.intUniform(minLen, maxLen));
    let s = "";
    for (let i = 0; i < n; i++) {
      s += String.fromCharCode(97 + Number(this.next() % 26n));
    }
    return s;
  }
}

// --- FNV-1a 64 answer checksum (benchmarks.md §6) ---

const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const encoder = new TextEncoder();

export class Checksum {
  private h = FNV_OFFSET;

  private bytes(s: string): void {
    let h = this.h;
    for (const b of encoder.encode(s)) {
      h = ((h ^ BigInt(b)) * FNV_PRIME) & MASK;
    }
    this.h = h;
  }

  private sep(b: bigint): void {
    this.h = ((this.h ^ b) * FNV_PRIME) & MASK;
  }

  null(): void {
    this.bytes("NULL");
    this.sep(0x1fn);
  }

  int(n: bigint): void {
    this.bytes(n.toString());
    this.sep(0x1fn);
  }

  // intLike renders an integer that a driver delivered as number/string/bigint —
  // String() of each is the same canonical decimal form.
  intLike(v: number | string | bigint): void {
    this.bytes(String(v));
    this.sep(0x1fn);
  }

  text(s: string): void {
    this.bytes(s);
    this.sep(0x1fn);
  }

  endRow(): void {
    this.sep(0x1en);
  }

  hex(): string {
    return this.h.toString(16).padStart(16, "0");
  }
}

// --- corpus (benchmarks.toml — benchmarks.md §3) ---

export interface Param {
  gen: string;
  min: bigint;
  max: bigint;
  start: bigint;
  minLen: bigint;
  maxLen: bigint;
  // int_window: base is the 0-based index of an EARLIER param; the value is that param's value +
  // intUniform(offMin, offMax). Lets a bench express a selective fixed-width range around a base
  // param (both endpoints const-sources).
  base: bigint;
  offMin: bigint;
  offMax: bigint;
}

export interface Bench {
  name: string;
  dataset: string;
  kind: string;
  sql: string;
  warmup: number;
  iterations: number;
  seed: bigint;
  expectRowsPerIter: number; // 0 = unchecked
  engines: string[];
  batch: number;
  readers: number; // concurrent_read: reader Sessions
  setupSql: string[];
  sqlOverride: Record<string, string>;
  setupSqlOverride: Record<string, string[]>;
  params: Param[];
}

export function sqlFor(b: Bench, engine: string): string {
  return b.sqlOverride[engine] ?? b.sql;
}

export function setupSqlFor(b: Bench, engine: string): string[] {
  return b.setupSqlOverride[engine] ?? b.setupSql;
}

export function runsOn(b: Bench, engine: string): boolean {
  return b.engines.length === 0 || b.engines.includes(engine);
}

// smol-toml parses integers as number (or bigint when large); normalize.
function big(v: unknown): bigint {
  if (typeof v === "bigint") return v;
  if (typeof v === "number") return BigInt(v);
  return 0n;
}

function num(v: unknown): number {
  if (typeof v === "number") return v;
  if (typeof v === "bigint") return Number(v);
  return 0;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function strList(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

export function loadCorpus(corpusDir: string): Bench[] {
  const doc = parseToml(readFileSync(`${corpusDir}/benchmarks.toml`, "utf8")) as Record<
    string,
    unknown
  >;
  if (num(doc.schema_version) !== 1) {
    throw new Error("benchmarks.toml: unsupported schema_version");
  }
  const benches: Bench[] = [];
  for (const raw of doc.bench as Record<string, unknown>[]) {
    const params: Param[] = [];
    for (const p of (raw.param as Record<string, unknown>[] | undefined) ?? []) {
      params.push({
        gen: str(p.gen),
        min: big(p.min),
        max: big(p.max),
        start: big(p.start),
        minLen: big(p.min_len),
        maxLen: big(p.max_len),
        base: big(p.base),
        offMin: big(p.off_min),
        offMax: big(p.off_max),
      });
    }
    benches.push({
      name: str(raw.name),
      dataset: str(raw.dataset),
      kind: str(raw.kind),
      sql: str(raw.sql),
      warmup: num(raw.warmup),
      iterations: num(raw.iterations),
      seed: big(raw.seed),
      expectRowsPerIter: num(raw.expect_rows_per_iter),
      engines: strList(raw.engines),
      batch: num(raw.batch),
      readers: num(raw.readers),
      setupSql: strList(raw.setup_sql),
      sqlOverride: (raw.sql_override as Record<string, string> | undefined) ?? {},
      setupSqlOverride: (raw.setup_sql_override as Record<string, string[]> | undefined) ?? {},
      params,
    });
  }
  return benches;
}

// --- datasets (datasets.toml — only the committed row counts the harness needs) ---

export function datasetTableRows(corpusDir: string, dataset: string, table: string): bigint {
  const doc = parseToml(readFileSync(`${corpusDir}/datasets.toml`, "utf8")) as Record<
    string,
    unknown
  >;
  for (const ds of doc.dataset as Record<string, unknown>[]) {
    if (str(ds.name) !== dataset) continue;
    for (const t of (ds.table as Record<string, unknown>[] | undefined) ?? []) {
      if (str(t.name) === table) return big(t.rows);
    }
  }
  throw new Error(`datasets.toml: no table ${table} in dataset ${dataset}`);
}

// --- fingerprint (benchmarks.md §5) ---

export function corpusFingerprint(corpusDir: string): string {
  return createHash("sha256")
    .update(readFileSync(`${corpusDir}/datasets.toml`))
    .digest("hex");
}

export function readSidecar(dataDir: string, dataset: string, engine: string): string {
  try {
    return readFileSync(`${dataDir}/${dataset}.${engine}.fingerprint`, "utf8").trim();
  } catch {
    return "";
  }
}

export function staleErr(dataset: string, engine: string): Error {
  return new Error(`stale benchmark data for ${dataset}/${engine}: run 'rake bench:setup'`);
}

// --- param stream (benchmarks.md §3: one stream across warmup + measured) ---

export type Arg = bigint | string;

export class ParamStream {
  private readonly params: Param[];
  private readonly prng: Prng;
  private readonly serials: bigint[];

  constructor(b: Bench) {
    this.params = b.params;
    this.prng = new Prng(b.seed);
    this.serials = b.params.map((p) => p.start);
  }

  next(): Arg[] {
    // Sequential (not .map) so int_window can reference an EARLIER arg in the same row.
    const args: Arg[] = [];
    this.params.forEach((p, i) => {
      switch (p.gen) {
        case "serial":
          args.push(this.serials[i]++);
          break;
        case "int_uniform":
          args.push(this.prng.intUniform(p.min, p.max));
          break;
        case "int_window": {
          const base = args[Number(p.base)];
          if (typeof base !== "bigint") throw new Error("int_window base must be an int param");
          args.push(base + this.prng.intUniform(p.offMin, p.offMax));
          break;
        }
        case "text":
          args.push(this.prng.text(p.minLen, p.maxLen));
          break;
        default:
          throw new Error(`unknown param gen ${p.gen}`);
      }
    });
    return args;
  }
}

// --- the engine contract ---

// One open handle onto one dataset's database. prepare() takes the corpus's $N SQL as
// authored (a driver that needs ?N rewrites it itself) and stores the statement
// internally — the runner uses one bench statement at a time.
export interface Engine {
  exec(sql: string): Promise<void>;
  prepare(sql: string): Promise<void>;
  queryPrepared(args: Arg[], sum: Checksum | null): Promise<number>;
  execPrepared(args: Arg[]): Promise<void>;
  queryInt(sql: string): Promise<bigint>;
  storedFingerprint(): Promise<string>;
  close(): Promise<void>;
  // OPTIONAL concurrent_read support (spec/design/benchmarks.md §8.1): open one reader per
  // block over the same committed data and run the blocks (parallel on a threaded core; the
  // single-threaded TS core runs them sequentially). warm/meas are pre-partitioned into
  // `readers` contiguous blocks. A driver that omits it (the wasm wrap) is skipped.
  concurrentRead?(
    sql: string,
    warm: Arg[][][],
    meas: Arg[][][],
    expectRows: number,
  ): Promise<ConcurrentOutcome>;
}

// ConcurrentOutcome is what a driver's concurrentRead returns (benchmarks.md §8.1):
// blockHexes are the per-reader-block FNV checksums in reader-index order (the runner folds
// them in that order into the one partition-invariant answer checksum), elapsed the merged
// per-query latencies, wallNs the wall clock of the timed phase.
export interface ConcurrentOutcome {
  blockHexes: string[];
  elapsed: bigint[];
  rowsTotal: number;
  wallNs: bigint;
}

export interface Config {
  engine: string; // jed | postgres | sqlite
  lang: string; // ts
  variant: string; // core | porsager | node-sqlite
  // dataset is "small" | "large" | "scratch"; "scratch" must yield a fresh, empty database.
  open(dataDir: string, dataset: string): Promise<Engine>;
}

// --- $N → ?N for SQLite (benchmarks.md §3) ---

export function rewritePlaceholders(sql: string): string {
  return sql.replace(/\$(\d+)/g, "?$1");
}

// --- the run loop (mirrors bench/go/internal/bench/runner.go) ---

interface ResultLine {
  schema: number;
  bench: string;
  dataset: string;
  engine: string;
  lang: string;
  variant: string;
  iterations: number;
  warmup: number;
  readers: number;
  total_ns: number;
  ns_per_op: number;
  min_ns: number;
  p50_ns: number;
  rows_total: number;
  checksum: string;
  fingerprint: string;
  started_at: string;
}

// partition tiles items into n contiguous blocks (the first len%n get one extra) — the
// deterministic per-reader split the concurrent_read checksum folds over (benchmarks.md §6).
function partition<T>(items: T[], n: number): T[][] {
  const blocks: T[][] = [];
  const base = Math.floor(items.length / n);
  const extra = items.length % n;
  let idx = 0;
  for (let r = 0; r < n; r++) {
    const size = base + (r < extra ? 1 : 0);
    blocks.push(items.slice(idx, idx + size));
    idx += size;
  }
  return blocks;
}

export async function run(
  cfg: Config,
  corpusDir: string,
  dataDir: string,
  filter: string,
): Promise<ResultLine[]> {
  const benches = loadCorpus(corpusDir);
  const want = corpusFingerprint(corpusDir);
  const results: ResultLine[] = [];
  for (const b of benches) {
    if (filter !== "" && !b.name.includes(filter)) continue;
    if (!runsOn(b, cfg.engine)) continue;
    process.stderr.write(
      `${cfg.engine}/${cfg.lang}/${cfg.variant}: ${b.name} (${b.dataset}) ...\n`,
    );
    try {
      const line = await runOne(cfg, b, corpusDir, dataDir, want);
      if (line !== null) results.push(line);
    } catch (e) {
      throw new Error(`bench ${b.name}: ${e instanceof Error ? e.message : e}`);
    }
  }
  return results;
}

async function runOne(
  cfg: Config,
  b: Bench,
  corpusDir: string,
  dataDir: string,
  want: string,
): Promise<ResultLine | null> {
  const startedAt = new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
  const eng = await cfg.open(dataDir, b.dataset);
  try {
    if (b.dataset !== "scratch") {
      const stored = await eng.storedFingerprint();
      if (stored !== want) throw staleErr(b.dataset, cfg.engine);
    }
    for (const sql of setupSqlFor(b, cfg.engine)) {
      await eng.exec(sql);
    }

    if (b.kind === "concurrent_read") {
      if (eng.concurrentRead === undefined) {
        process.stderr.write(
          `  skip: ${cfg.engine}/${cfg.lang}/${cfg.variant} has no concurrent_read support\n`,
        );
        return null;
      }
      const cstream = new ParamStream(b);
      const warm: Arg[][] = [];
      for (let i = 0; i < b.warmup; i++) warm.push(cstream.next());
      const meas: Arg[][] = [];
      for (let i = 0; i < b.iterations; i++) meas.push(cstream.next());
      const out = await eng.concurrentRead(
        sqlFor(b, cfg.engine),
        partition(warm, b.readers),
        partition(meas, b.readers),
        b.expectRowsPerIter,
      );
      const combined = new Checksum();
      for (const h of out.blockHexes) combined.text(h);
      const cel = out.elapsed.slice().sort((a, z) => (a < z ? -1 : a > z ? 1 : 0));
      return {
        schema: 1,
        bench: b.name,
        dataset: b.dataset,
        engine: cfg.engine,
        lang: cfg.lang,
        variant: cfg.variant,
        iterations: b.iterations,
        warmup: b.warmup,
        readers: b.readers,
        total_ns: Number(out.wallNs),
        ns_per_op: Number(out.wallNs / BigInt(b.iterations)),
        min_ns: Number(cel[0]),
        p50_ns: Number(cel[(cel.length - 1) >> 1]),
        rows_total: out.rowsTotal,
        checksum: combined.hex(),
        fingerprint: want,
        started_at: startedAt,
      };
    }

    await eng.prepare(sqlFor(b, cfg.engine));

    const stream = new ParamStream(b);
    const sum = new Checksum();
    const elapsed: bigint[] = [];
    let rowsTotal = 0;

    for (let i = 0; i < b.warmup + b.iterations; i++) {
      const measured = i >= b.warmup;
      switch (b.kind) {
        case "query": {
          const args = stream.next();
          const start = process.hrtime.bigint();
          const n = await eng.queryPrepared(args, measured ? sum : null);
          const d = process.hrtime.bigint() - start;
          if (measured) {
            elapsed.push(d);
            rowsTotal += n;
            if (b.expectRowsPerIter > 0 && n !== b.expectRowsPerIter) {
              throw new Error(`expected ${b.expectRowsPerIter} rows per iteration, got ${n}`);
            }
          }
          break;
        }
        case "write_rollback": {
          const start = process.hrtime.bigint();
          await eng.exec("BEGIN");
          for (let j = 0; j < b.batch; j++) {
            await eng.execPrepared(stream.next());
          }
          await eng.exec("ROLLBACK");
          if (measured) elapsed.push(process.hrtime.bigint() - start);
          break;
        }
        case "write_durable": {
          const args = stream.next();
          const start = process.hrtime.bigint();
          await eng.execPrepared(args);
          if (measured) elapsed.push(process.hrtime.bigint() - start);
          break;
        }
        default:
          throw new Error(`unknown bench kind ${b.kind}`);
      }
    }

    // Write kinds: the checksum is the post-run sanity count(*) (benchmarks.md §6).
    if (b.kind !== "query") {
      const table = writeTable(b.sql);
      const n = await eng.queryInt(`SELECT count(*) FROM ${table}`);
      const expect =
        b.kind === "write_rollback"
          ? datasetTableRows(corpusDir, b.dataset, table)
          : BigInt(b.warmup + b.iterations);
      if (n !== expect) {
        throw new Error(`post-run count(*) of ${table}: got ${n}, want ${expect}`);
      }
      sum.int(n);
      sum.endRow();
    }

    elapsed.sort((a, z) => (a < z ? -1 : a > z ? 1 : 0));
    let totalNs = 0n;
    for (const d of elapsed) totalNs += d;
    return {
      schema: 1,
      bench: b.name,
      dataset: b.dataset,
      engine: cfg.engine,
      lang: cfg.lang,
      variant: cfg.variant,
      iterations: b.iterations,
      warmup: b.warmup,
      readers: 0,
      total_ns: Number(totalNs),
      ns_per_op: Number(totalNs / BigInt(b.iterations)),
      min_ns: Number(elapsed[0]),
      p50_ns: Number(elapsed[(elapsed.length - 1) >> 1]),
      rows_total: rowsTotal,
      checksum: sum.hex(),
      fingerprint: want,
      started_at: startedAt,
    };
  } finally {
    await eng.close();
  }
}

// The target table of a write statement — the word after INTO (INSERT), UPDATE, or FROM (DELETE) —
// for the post-run count.
export function writeTable(sql: string): string {
  const fields = sql.split(/\s+/);
  for (let i = 0; i < fields.length - 1; i++) {
    const kw = fields[i].toUpperCase();
    if (kw === "INTO" || kw === "UPDATE" || kw === "FROM") {
      return fields[i + 1].split("(")[0];
    }
  }
  throw new Error(`write bench SQL has no INSERT / UPDATE / DELETE target table: ${sql}`);
}

// Uniform binary entrypoint: bench-<engine> <corpus_dir> <data_dir> <out_path> [filter].
export async function mainWith(cfg: Config): Promise<void> {
  const args = process.argv.slice(2);
  if (args.length < 3 || args.length > 4) {
    process.stderr.write(
      `usage: bench-${cfg.engine} <corpus_dir> <data_dir> <out_path> [name_filter]\n`,
    );
    process.exit(2);
  }
  try {
    const results = await run(cfg, args[0], args[1], args[3] ?? "");
    writeFileSync(args[2], results.map((r) => JSON.stringify(r) + "\n").join(""));
  } catch (e) {
    process.stderr.write(`error: ${e instanceof Error ? e.message : e}\n`);
    process.exit(1);
  }
}
