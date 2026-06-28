// bench-jed benchmarks the TS jed core (spec/design/benchmarks.md §6/§7). The core is
// imported relatively — Node's native type-stripping runs it with no build step, and
// this package's dependencies never touch impl/ts.

import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";

import {
  type Engine as JedDb,
  type PreparedStatement,
  create,
  executeParams,
  open,
  prepare,
  query,
} from "../../../impl/ts/src/lib.ts";
import { intValue, textValue, type Value } from "../../../impl/ts/src/value.ts";

import { type Arg, type Checksum, type Engine, mainWith, readSidecar } from "./lib.ts";

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
    const rows = this.stmt!.query(bindArgs(args));
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
    this.stmt!.execute(bindArgs(args));
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
}

function bindArgs(args: Arg[]): Value[] {
  return args.map((a) => (typeof a === "bigint" ? intValue(a) : textValue(a)));
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
