import { readFileSync } from "node:fs";
import { createDatabase } from "../../impl/ts/src/file.ts";
import { nullValue, render, intValue } from "../../impl/ts/src/value.ts";

const cycles = Number(process.env.JOIN_CYCLES ?? 1),
  warmCycles = Number(process.env.JOIN_WARM_CYCLES ?? 1);
const db = createDatabase();
const session = db.session();
let repetitions = 0,
  validation = 0;
const verify = process.argv.includes("--verify-only");
const utf8 = new TextEncoder();
for (const line of readFileSync(process.argv[2], "utf8").trimEnd().split("\n")) {
  const f = line.split("\t");
  if (f[0] === "config") {
    repetitions = Number(f[1]);
    validation = Number(f[2]);
  } else if (f[0] === "setup") {
    session.execute(f[1], []);
  } else if (f[0] === "workload") {
    const name = f[1],
      iterations = Number(f[2]),
      warmup = Number(f[3]);
    const offset = Number(f[4]),
      step = Number(f[5]),
      span = Number(f[6]);
    const statement = session.prepareStatement(f[7]);
    const parameters = Array.from({ length: Math.max(validation, warmup, iterations) }, (_, i) =>
      span === 0 ? [] : [intValue(BigInt(offset + ((i * step) % span)))],
    );
    if (process.argv.includes("--diagnostics")) {
      for (const binding of [0, 17, 51, 999, 1000, null])
        for (const prefix of ["", "EXPLAIN (ANALYZE, VERBOSE) "]) {
          const q = prefix ? session.prepareStatement(prefix + f[7]) : statement;
          const rows = session.queryPrepared(q, [
            binding === null ? nullValue() : intValue(BigInt(binding)),
          ]);
          const values = [...rows].map((r) => r.map(render));
          console.log(
            JSON.stringify({
              workload: name,
              binding,
              explain: !!prefix,
              types: rows.columnTypes,
              rows: values,
              cost: Number(rows.cost),
            }),
          );
          rows.close();
        }
      continue;
    }
    let hash = 0xcbf29ce484222325n;
    const add = (byte: number) => {
      hash = BigInt.asUintN(64, (hash ^ BigInt(byte)) * 0x100000001b3n);
    };
    const run = (i: number, checksum: boolean): number => {
      const rows = session.queryPrepared(statement, parameters[i]);
      let count = 0;
      try {
        for (const row of rows) {
          count++;
          if (checksum) {
            for (const value of row) {
              let text: string;
              if (value.kind === "null") text = "NULL";
              else if (value.kind === "int") text = value.int.toString();
              else if (value.kind === "text") text = value.text;
              else throw new Error(`unexpected value kind ${value.kind}`);
              for (const byte of utf8.encode(text)) add(byte);
              add(0x1f);
            }
            add(0x1e);
          }
        }
      } finally {
        rows.close();
      }
      return count;
    };
    let validationRows = 0;
    for (let i = 0; i < validation; i++) validationRows += run(i, true);
    const samples: number[] = [];
    let totalRows = 0;
    if (!verify) {
      for (let i = 0; i < warmup * warmCycles; i++) run(i % warmup, false);
      for (let rep = 0; rep < repetitions; rep++) {
        let count = 0;
        const start = process.hrtime.bigint();
        for (let i = 0; i < iterations * cycles; i++) count += run(i % iterations, false);
        samples.push(Number(process.hrtime.bigint() - start));
        totalRows += count;
      }
    }
    console.log(
      JSON.stringify({
        workload: name,
        checksum: hash.toString(16).padStart(16, "0"),
        validation_rows: validationRows,
        samples_ns: samples,
        rows_total: totalRows,
        iterations: iterations * cycles,
        warmup,
        runtime: process.version,
      }),
    );
  }
}
session.close();
db.close();
