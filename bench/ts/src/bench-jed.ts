// bench-jed benchmarks the TS jed core (spec/design/benchmarks.md §6/§7). The core is
// imported relatively — Node's native type-stripping runs it with no build step, and
// this package's dependencies never touch impl/ts.
//
// It drives the public Database/Session API (createDatabase/openDatabase + one persistent
// Session), mirroring the Go and Rust harnesses, so a commit takes the same path an embedder's
// does — including the post-commit residency flip (spec/design/bplus-reshape.md B4).

import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";

import {
  type Database,
  type PreparedStatement,
  type Rows,
  type Session,
  createDatabase,
  openDatabase,
} from "../../../impl/ts/src/lib.ts";
import { ExtensionRegistry } from "../../../impl/ts/src/extension.ts";
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
  readonly hostFunctions = true; // every handle carries benchExtensions() (benchmarks.md §8.2)
  private readonly db: Database;
  // The persistent connection the bench drives (BEGIN/COMMIT/ROLLBACK span calls).
  private readonly sess: Session;
  private stmt: PreparedStatement | null = null;
  private readonly dataDir: string;
  private readonly dataset: string;
  private readonly scratch: string | null;

  constructor(db: Database, dataDir: string, dataset: string, scratch: string | null) {
    this.db = db;
    this.sess = db.session();
    this.dataDir = dataDir;
    this.dataset = dataset;
    this.scratch = scratch;
  }

  async exec(sql: string): Promise<void> {
    this.sess.execute(sql, []);
  }

  async prepare(sql: string): Promise<void> {
    this.stmt = this.sess.prepareStatement(sql);
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    return drainRows(this.sess.queryPrepared(this.stmt!, bindArgs(args)), sum);
  }

  async execPrepared(args: Arg[]): Promise<void> {
    this.sess.executePrepared(this.stmt!, bindArgs(args));
  }

  async queryInt(sql: string): Promise<bigint> {
    const rows = this.sess.query(sql, []);
    try {
      for (const row of rows) {
        const v = row[0];
        if (v.kind === "int") return v.int;
      }
    } finally {
      rows.close();
    }
    throw new Error(`expected one integer from ${sql}`);
  }

  async storedFingerprint(): Promise<string> {
    return readSidecar(this.dataDir, this.dataset, "jed");
  }

  async close(): Promise<void> {
    // Release the file descriptor and buffer pool: the cold_open kind opens a fresh handle per
    // iteration (benchmarks.md §8.3), so a handle must not outlive its bench.
    this.sess.close();
    this.db.close();
    if (this.scratch !== null) {
      rmSync(this.scratch, { recursive: true, force: true });
    }
  }

  // concurrentRead mints a reader Session per block from the Database already opened for this run
  // (the slice-7 convergence, session.md §2.4/§10) — every Session shares the one Database's
  // committed snapshot + buffer pool. The single-threaded TS core runs the blocks SEQUENTIALLY (no
  // parallel speedup, documented), but the per-block partition + fold makes the answer checksum
  // identical to the threaded Go/Rust cores (benchmarks.md §8.1).
  async concurrentRead(
    sql: string,
    warm: Arg[][][],
    meas: Arg[][][],
    expectRows: number,
  ): Promise<ConcurrentOutcome> {
    const readers = meas.length;
    const sessions: Session[] = Array.from({ length: readers }, () => this.db.readSession());
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
    }
  }
}

// benchMix is the bench host functions' arithmetic (benchmarks.md §8.2) — cheap on purpose, so the
// timings expose per-call overhead.
function benchMix(v: Value): Value {
  return intValue(((v as { kind: "int"; int: bigint }).int * 2654435761n) % 1000003n);
}

// benchExtensions registers the bench host functions on every handle: the same arithmetic as a
// single-row kernel (bench_mix_row) and a batch kernel (bench_mix_batch).
function benchExtensions(): ExtensionRegistry {
  const reg = new ExtensionRegistry();
  reg.registerFunction({
    name: "bench_mix_row",
    argTypes: ["i32"],
    result: "i64",
    volatility: "immutable",
    kernel: (args) => benchMix(args[0]),
  });
  reg.registerFunction({
    name: "bench_mix_batch",
    argTypes: ["i32"],
    result: "i64",
    volatility: "immutable",
    batchKernel: (args, out) => {
      for (const v of args[0]) out.push(benchMix(v));
    },
  });
  return reg;
}

function bindArgs(args: Arg[]): Value[] {
  return args.map((a) => (typeof a === "bigint" ? intValue(a) : textValue(a)));
}

// drainRows pulls a cursor to exhaustion, folding each row into sum (when non-null), and returns the
// row count. The cursor is closed after the drain — on a mid-drain error too — releasing its
// reader-liveness pin (JS has no destructor to release it later).
function drainRows(rows: Rows, sum: Checksum | null): number {
  try {
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
  } finally {
    rows.close();
  }
}

// runReaderQuery runs one query through a reader Session, re-parsing the SQL each call — deliberate
// (benchmarks.md §8.1): a constant per-query parse cost is included, uniform across the jed cores.
function runReaderQuery(sess: Session, sql: string, args: Arg[], sum: Checksum | null): number {
  return drainRows(sess.query(sql, bindArgs(args)), sum);
}

await mainWith({
  engine: "jed",
  lang: "ts",
  variant: "core",
  async open(dataDir: string, dataset: string): Promise<Engine> {
    if (dataset === "scratch") {
      const dir = mkdtempSync(join(dataDir, "scratch-"));
      const db = createDatabase({ path: join(dir, "scratch.jed"), extensions: benchExtensions() });
      return new JedEngine(db, dataDir, dataset, dir);
    }
    const db = openDatabase(join(dataDir, `${dataset}.jed`), { extensions: benchExtensions() });
    return new JedEngine(db, dataDir, dataset, null);
  },
});
