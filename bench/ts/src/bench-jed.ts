// bench-jed benchmarks the TS jed core (spec/design/benchmarks.md §6/§7). The core is
// imported relatively — Node's native type-stripping runs it with no build step, and
// this package's dependencies never touch impl/ts.

import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";

import {
  type Engine as JedDb,
  type PreparedStatement,
  type Session,
  create,
  executeParams,
  executePrepared,
  open,
  openDatabase,
  prepare,
  query,
  queryPrepared,
} from "../../../impl/ts/src/tooling.ts";
import { intValue, textValue, type Value } from "../../../impl/ts/src/value.ts";

import {
  type Arg,
  Checksum,
  type ConcurrentOutcome,
  type Engine,
  mainWith,
  readSidecar,
} from "./lib.ts";

class JedEngine implements Engine {
  private readonly db: JedDb;
  private stmt: PreparedStatement | null = null;
  private readonly dataDir: string;
  private readonly dataset: string;
  private readonly scratch: string | null;

  constructor(db: JedDb, dataDir: string, dataset: string, scratch: string | null) {
    this.db = db;
    this.dataDir = dataDir;
    this.dataset = dataset;
    this.scratch = scratch;
  }

  async exec(sql: string): Promise<void> {
    executeParams(this.db, sql, []);
  }

  async prepare(sql: string): Promise<void> {
    this.stmt = prepare(this.db, sql);
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    const rows = queryPrepared(this.db, this.stmt!, bindArgs(args));
    let n = 0;
    for (const row of rows) {
      n++;
      if (sum === null) continue;
      for (const v of row) {
        switch (v.kind) {
          case "null":
            sum.null();
            break;
          case "int":
            sum.int(v.int);
            break;
          case "text":
            sum.text(v.text);
            break;
          default:
            throw new Error(`unexpected result kind ${v.kind}`);
        }
      }
      sum.endRow();
    }
    return n;
  }

  async execPrepared(args: Arg[]): Promise<void> {
    executePrepared(this.db, this.stmt!, bindArgs(args));
  }

  async queryInt(sql: string): Promise<bigint> {
    for (const row of query(this.db, sql, [])) {
      const v = row[0];
      if (v.kind === "int") return v.int;
    }
    throw new Error(`expected one integer from ${sql}`);
  }

  async storedFingerprint(): Promise<string> {
    return readSidecar(this.dataDir, this.dataset, "jed");
  }

  async close(): Promise<void> {
    if (this.scratch !== null) {
      rmSync(this.scratch, { recursive: true, force: true });
    }
  }

  // concurrentRead opens ONE Database over the dataset file and mints a reader Session per
  // block (the slice-7 convergence, session.md §2.4/§10) — every Session shares the one
  // Database's committed snapshot + buffer pool. The single-threaded TS core runs the
  // blocks SEQUENTIALLY (no parallel speedup, documented), but the per-block partition +
  // fold makes the answer checksum identical to the threaded Go/Rust cores (benchmarks.md §8.1).
  async concurrentRead(
    sql: string,
    warm: Arg[][][],
    meas: Arg[][][],
    expectRows: number,
  ): Promise<ConcurrentOutcome> {
    const db = openDatabase(join(this.dataDir, `${this.dataset}.jed`));
    const readers = meas.length;
    const sessions: Session[] = Array.from({ length: readers }, () => db.readSession());
    try {
      // Pass 1 — warmup, untimed: populate the shared buffer pool.
      for (let r = 0; r < readers; r++) {
        for (const args of warm[r]) runReaderQuery(sessions[r], sql, args, null);
      }
      // Pass 2 — measured (wall-clock).
      const blockHexes: string[] = [];
      const elapsed: bigint[] = [];
      let rowsTotal = 0;
      const start = process.hrtime.bigint();
      for (let r = 0; r < readers; r++) {
        const sum = new Checksum();
        for (const args of meas[r]) {
          const t0 = process.hrtime.bigint();
          const n = runReaderQuery(sessions[r], sql, args, sum);
          elapsed.push(process.hrtime.bigint() - t0);
          rowsTotal += n;
          if (expectRows > 0 && n !== expectRows) {
            throw new Error(`expected ${expectRows} rows per iteration, got ${n}`);
          }
        }
        blockHexes.push(sum.hex());
      }
      const wallNs = process.hrtime.bigint() - start;
      return { blockHexes, elapsed, rowsTotal, wallNs };
    } finally {
      for (const s of sessions) s.close();
      db.close();
    }
  }
}

function bindArgs(args: Arg[]): Value[] {
  return args.map((a) => (typeof a === "bigint" ? intValue(a) : textValue(a)));
}

// runReaderQuery runs one query through a reader Session, re-parsing the SQL each call — deliberate
// (benchmarks.md §8.1): a constant per-query parse cost is included, uniform across the jed cores —
// folding rows into sum.
function runReaderQuery(sess: Session, sql: string, args: Arg[], sum: Checksum | null): number {
  const rows = sess.query(sql, bindArgs(args));
  let n = 0;
  for (const row of rows) {
    n++;
    if (sum === null) continue;
    for (const v of row) {
      switch (v.kind) {
        case "null":
          sum.null();
          break;
        case "int":
          sum.int(v.int);
          break;
        case "text":
          sum.text(v.text);
          break;
        default:
          throw new Error(`unexpected result kind ${v.kind}`);
      }
    }
    sum.endRow();
  }
  return n;
}

await mainWith({
  engine: "jed",
  lang: "ts",
  variant: "core",
  async open(dataDir: string, dataset: string): Promise<Engine> {
    if (dataset === "scratch") {
      const dir = mkdtempSync(join(dataDir, "scratch-"));
      return new JedEngine(create(join(dir, "scratch.jed")), dataDir, dataset, dir);
    }
    return new JedEngine(open(join(dataDir, `${dataset}.jed`)), dataDir, dataset, null);
  },
});
