// bench-pg benchmarks PostgreSQL via the porsager `postgres` driver
// (spec/design/benchmarks.md §6/§7). The corpus's raw $N SQL runs through sql.unsafe
// with prepare:true (a named server-side prepared statement); .values() returns rows as
// arrays so the canonical rendering follows column order. Connection settings come from
// the PG* env (the devcontainer points PGHOST at the Unix socket).

import postgres from "postgres";

import { type Arg, type Checksum, type Engine, mainWith } from "./lib.ts";

// PG type OIDs for the canonical rendering (benchmarks.md §3 common-subset rule).
const INT_OIDS = new Set([20, 21, 23]); // int8, int2, int4
const TEXT_OIDS = new Set([25, 1043]); // text, varchar

type Sql = ReturnType<typeof postgres>;

class PgEngine implements Engine {
  private readonly sql: Sql;
  private query = "";

  constructor(sql: Sql) {
    this.sql = sql;
  }

  async exec(sql: string): Promise<void> {
    await this.sql.unsafe(sql);
  }

  async prepare(sql: string): Promise<void> {
    this.query = sql; // postgres.js prepares (and caches) on first execution
  }

  async queryPrepared(args: Arg[], sum: Checksum | null): Promise<number> {
    const result = await this.sql.unsafe(this.query, args as never[], { prepare: true }).values();
    if (sum !== null) {
      const columns = result.columns ?? [];
      for (const row of result) {
        for (let i = 0; i < row.length; i++) {
          const v = row[i];
          if (v === null) {
            sum.null();
          } else if (INT_OIDS.has(columns[i].type)) {
            sum.intLike(v as number | string | bigint);
          } else if (TEXT_OIDS.has(columns[i].type)) {
            sum.text(v as string);
          } else {
            throw new Error(`unexpected result type oid ${columns[i].type}`);
          }
        }
        sum.endRow();
      }
    }
    return result.length;
  }

  async execPrepared(args: Arg[]): Promise<void> {
    await this.sql.unsafe(this.query, args as never[], { prepare: true });
  }

  async queryInt(sql: string): Promise<bigint> {
    const result = await this.sql.unsafe(sql).values();
    return BigInt(result[0][0] as number | string | bigint);
  }

  async storedFingerprint(): Promise<string> {
    try {
      const result = await this.sql.unsafe("SELECT value FROM _bench_meta WHERE key = 'fingerprint'").values();
      return result.length > 0 ? (result[0][0] as string) : "";
    } catch {
      return ""; // absent table reads as no fingerprint → stale
    }
  }

  async close(): Promise<void> {
    await this.sql.end();
  }
}

await mainWith({
  engine: "postgres",
  lang: "ts",
  variant: "porsager",
  async open(_dataDir: string, dataset: string): Promise<Engine> {
    const sql = postgres({
      host: process.env.PGHOST ?? "localhost",
      user: process.env.PGUSER ?? "postgres",
      database: `jed_bench_${dataset}`,
      max: 1,
    });
    if (dataset === "scratch") {
      // bench-setup created the empty scratch database once; reset it per run.
      await sql.unsafe("DROP TABLE IF EXISTS scratch");
    }
    return new PgEngine(sql);
  },
});
