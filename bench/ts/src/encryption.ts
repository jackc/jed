// Standalone encryption-at-rest benchmark worker for the TS core (spec/design/encryption.md §9,
// *Integrated performance*); mise-tasks/bench/encryption alternates the variants and reports. Mirrors
// bench/rust/src/bin/encryption.rs and bench/go/cmd/encryption: the same table, phases, lookup
// sequence and checksums.
//
// `node src/encryption.ts plain|encrypted DIR ROWS` builds a table of ROWS rows (`id i64 PRIMARY KEY`,
// a 100-byte text) in batches of 1000 committed rows, then reopens it with a 1 MiB page cache and runs
// a full scan and random point lookups whose leaves mostly miss the cache, then the same lookups with a
// cache that holds the whole table. Each phase prints one `phase\tmillis\tchecksum` line. Commits skip
// fsync, so the commit phase measures sealing rather than the disk.

import { rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { performance } from "node:perf_hooks";

import { createDatabase, openDatabase } from "../../../impl/ts/src/lib.ts";
import { intValue, type Value } from "../../../impl/ts/src/value.ts";

const KEY = new Uint8Array(32).fill(0x4a);
const LOOKUPS = 20_000;
const MASK = (1n << 64n) - 1n;

const [variant, dir, rowsText] = process.argv.slice(2);
if (
  (variant !== "plain" && variant !== "encrypted") ||
  dir === undefined ||
  rowsText === undefined
) {
  throw new Error("usage: encryption plain|encrypted DIR ROWS");
}
const credential = variant === "encrypted" ? { key: KEY } : undefined;
const path = join(dir, `${variant}.jed`);
rmSync(path, { force: true });
const rows = Number(rowsText);

function report(phase: string, start: number, checksum: bigint | number): void {
  console.log(`${phase}\t${(performance.now() - start).toFixed(3)}\t${checksum}`);
}

function int(v: Value): bigint {
  if (v.kind !== "int") throw new Error(`expected an int, got ${v.kind}`);
  return v.int;
}

const db = createDatabase({ path, skipFsync: true, locking: "none", credential });
const s = db.session();
s.execute("CREATE TABLE t (id i64 PRIMARY KEY, v text)");
let start = performance.now();
for (let lo = 1; lo <= rows; ) {
  const hi = Math.min(lo + 999, rows);
  s.execute(
    `INSERT INTO t SELECT g, repeat(md5(g::text), 3) || 'abcd' FROM generate_series(${lo}, ${hi}) g`,
  );
  lo = hi + 1;
}
report("commit_1000_row_batches", start, rows);
s.close();
db.close();

for (const [phase, cache] of [
  ["cold", 1 << 20],
  ["hot", 1 << 30],
] as const) {
  const handle = openDatabase(path, {
    skipFsync: true,
    locking: "none",
    cacheBytes: cache,
    credential,
  });
  const session = handle.session();
  if (phase === "cold") {
    start = performance.now();
    const q = session.query("SELECT count(*), sum(length(v)) FROM t");
    const [row] = [...q];
    q.close();
    report("full_scan_cold", start, int(row![1]!));
  } else {
    // Warm the cache so every lookup is a hit.
    const q = session.query("SELECT count(*) FROM t");
    for (const _ of q) {
      /* drain */
    }
    q.close();
  }
  const stmt = session.prepareStatement("SELECT length(v) FROM t WHERE id = $1");
  start = performance.now();
  let checksum = 0n;
  let x = 0x9e37_79b9_7f4a_7c15n;
  for (let i = 0; i < LOOKUPS; i++) {
    x ^= (x << 13n) & MASK;
    x ^= x >> 7n;
    x ^= (x << 17n) & MASK;
    const id = (x % BigInt(rows)) + 1n;
    const q = session.queryPrepared(stmt, [intValue(id)]);
    for (const r of q) checksum += int(r[0]!) + id;
    q.close();
  }
  report(`point_lookups_${phase}`, start, checksum);
  session.close();
  handle.close();
}
console.log(`file_bytes\t0\t${statSync(path).size}`);
rmSync(path, { force: true });
