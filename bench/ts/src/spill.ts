// Standalone spill benchmark worker; the shared Ruby driver owns the workload.
import { readFileSync } from "node:fs";
import { createDatabase, openDatabase, render } from "../../../impl/ts/src/tooling.ts";

const [mode, path, workMem, workload, cacheBytes] = process.argv.slice(2);
if (!path || !workMem || !workload || !cacheBytes)
  throw new Error("usage: spill create|open database work_mem sql.tsv cache_bytes");
const budget = Number(workMem);
if (!Number.isSafeInteger(budget) || budget < 0) throw new Error("invalid work_mem");
const cache = Number(cacheBytes);
if (!Number.isSafeInteger(cache) || cache <= 0) throw new Error("invalid cache_bytes");
const db =
  mode === "create"
    ? createDatabase({ path, skipFsync: true, locking: "none" })
    : openDatabase(path, { cacheBytes: cache, locking: "none" });
const session = db.session();
session.setWorkMem(budget);
try {
  for (const line of readFileSync(workload, "utf8").trimEnd().split("\n")) {
    const split = line.indexOf("\t");
    if (split < 0) throw new Error("invalid workload line");
    const name = line.slice(0, split);
    const start = performance.now();
    const rows = session.query(line.slice(split + 1));
    let hash = 14695981039346656037n;
    let count = 0;
    const encoder = new TextEncoder();
    const add = (bytes: Uint8Array) => {
      for (const byte of bytes) hash = BigInt.asUintN(64, (hash ^ BigInt(byte)) * 1099511628211n);
    };
    try {
      for (const row of rows) {
        count++;
        for (const value of row) {
          const bytes = encoder.encode(render(value));
          add(encoder.encode(`${bytes.length}:`));
          add(bytes);
        }
        add(new Uint8Array([10]));
      }
      if (name !== "-")
        console.log(
          JSON.stringify({
            name,
            rows: count,
            cost: Number(rows.cost),
            checksum: hash.toString(16).padStart(16, "0"),
            ms: performance.now() - start,
          }),
        );
    } finally {
      rows.close();
    }
  }
} finally {
  session.close();
  db.close();
}
