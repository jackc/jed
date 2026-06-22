// bench-sqlite benchmarks SQLite via node:sqlite — built into Node, zero dependency
// (spec/design/benchmarks.md §7). $N is rewritten to ?N at prepare time (§3); integers
// are read as BigInt so i64 stays exact.

import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync, type StatementSync } from "node:sqlite";

import {
  type Arg,
  type Checksum,
  type Engine,
  mainWith,
  readSidecar,
  rewritePlaceholders,
  staleErr,
} from "./lib.ts";

class SqliteEngine implements Engine {
  private readonly db: DatabaseSync;
  private stmt: StatementSync | null = null;
  private cols: string[] = [];
  private readonly dataDir: string;
  private readonly dataset: string;
  private readonly scratch: string | null;

  constructor(db: DatabaseSync, dataDir: string, dataset: string, scratch: string | null) {
    this.db = db;
    this.dataDir = dataDir;
    this.dataset = dataset;
    this.scratch = scratch;
  }

  async exec(sql: string): Promise<void> {
    this.db.exec(sql);
  }

  async prepare(sql: string): Promise<void> {
    this.stmt = this.db.prepare(rewritePlaceholders(sql));
    this.stmt.setReadBigInts(true);
    this.cols = this.stmt.columns().map((c) => c.column ?? c.name);
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    const rows = this.stmt!.all(...args) as Record<string, unknown>[];
    if (sum !== null) {
      for (const row of rows) {
        for (const name of this.cols) {
          const v = row[name];
          if (v === null) sum.null();
          else if (typeof v === "bigint") sum.int(v);
          else if (typeof v === "string") sum.text(v);
          else throw new Error(`unexpected result type ${typeof v}`);
        }
        sum.endRow();
      }
    }
    return rows.length;
  }

  async execPrepared(args: Arg[]): Promise<void> {
    this.stmt!.run(...args);
  }

  async queryInt(sql: string): Promise<bigint> {
    const stmt = this.db.prepare(sql);
    stmt.setReadBigInts(true);
    const row = stmt.get() as Record<string, unknown>;
    return Object.values(row)[0] as bigint;
  }

  async storedFingerprint(): Promise<string> {
    return readSidecar(this.dataDir, this.dataset, "sqlite");
  }

  async close(): Promise<void> {
    this.db.close();
    if (this.scratch !== null) {
      rmSync(this.scratch, { recursive: true, force: true });
    }
  }
}

await mainWith({
  engine: "sqlite",
  lang: "ts",
  variant: "node-sqlite",
  async open(dataDir: string, dataset: string): Promise<Engine> {
    let scratch: string | null = null;
    let path: string;
    if (dataset === "scratch") {
      scratch = mkdtempSync(join(dataDir, "scratch-"));
      path = join(scratch, "scratch.sqlite");
    } else {
      path = join(dataDir, `${dataset}.sqlite`);
      if (!existsSync(path)) throw staleErr(dataset, "sqlite");
    }
    const db = new DatabaseSync(path);
    // The classic durable configuration (benchmarks.md §8); only matters for write benches.
    db.exec("PRAGMA journal_mode=DELETE");
    db.exec("PRAGMA synchronous=FULL");
    return new SqliteEngine(db, dataDir, dataset, scratch);
  },
});
