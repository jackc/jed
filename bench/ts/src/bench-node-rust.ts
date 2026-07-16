// bench-node-rust benchmarks the native Node-API package that wraps the Rust jed core
// (impl/node). It runs the same corpus as bench-jed.ts, including boundary encoding and
// JavaScript result decoding in measured query calls. concurrent_read delegates one timed call
// to the wrapper so Rust can exercise its real threaded reader path.

import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";

import {
  benchConcurrentRead,
  type BindValue,
  Database,
  type PreparedStatement,
} from "../../../impl/node/index.ts";

import {
  type Arg,
  type Checksum,
  type ConcurrentOutcome,
  type Engine,
  mainWith,
  readSidecar,
} from "./lib.ts";

class NodeRustEngine implements Engine {
  private readonly db: Database;
  private stmt: PreparedStatement | null = null;
  private readonly dataDir: string;
  private readonly dataset: string;
  private readonly scratch: string | null;

  constructor(db: Database, dataDir: string, dataset: string, scratch: string | null) {
    this.db = db;
    this.dataDir = dataDir;
    this.dataset = dataset;
    this.scratch = scratch;
  }

  async exec(sql: string): Promise<void> {
    this.db.execute(sql);
  }

  async prepare(sql: string): Promise<void> {
    this.stmt?.close();
    this.stmt = this.db.prepare(sql);
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    const rows = this.stmt!.query(bindArgs(args));
    for (const row of rows) {
      if (sum === null) continue;
      for (const value of row) {
        if (value === null) sum.null();
        else if (typeof value === "bigint") sum.int(value);
        else sum.text(value);
      }
      sum.endRow();
    }
    return rows.length;
  }

  async execPrepared(args: Arg[]): Promise<void> {
    this.stmt!.execute(bindArgs(args));
  }

  async queryInt(sql: string): Promise<bigint> {
    const value = this.db.query(sql)[0]?.[0];
    if (typeof value !== "bigint") throw new Error(`expected one integer from ${sql}`);
    return value;
  }

  async storedFingerprint(): Promise<string> {
    return readSidecar(this.dataDir, this.dataset, "jed");
  }

  async close(): Promise<void> {
    this.stmt?.close();
    this.db.close();
    if (this.scratch !== null) rmSync(this.scratch, { recursive: true, force: true });
  }

  async concurrentRead(
    sql: string,
    warm: Arg[][][],
    measured: Arg[][][],
    expectRows: number,
  ): Promise<ConcurrentOutcome> {
    return benchConcurrentRead(
      join(this.dataDir, `${this.dataset}.jed`),
      sql,
      warm.map((block) => block.map(bindArgs)),
      measured.map((block) => block.map(bindArgs)),
      expectRows,
    );
  }
}

function bindArgs(args: Arg[]): BindValue[] {
  return args;
}

await mainWith({
  engine: "jed",
  lang: "node",
  variant: "rust-wrap",
  async open(dataDir: string, dataset: string): Promise<Engine> {
    if (dataset === "scratch") {
      const dir = mkdtempSync(join(dataDir, "scratch-"));
      return new NodeRustEngine(Database.create(join(dir, "scratch.jed")), dataDir, dataset, dir);
    }
    return new NodeRustEngine(
      Database.open(join(dataDir, `${dataset}.jed`)),
      dataDir,
      dataset,
      null,
    );
  },
});
