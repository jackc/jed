// Worker half of resealed_corruption.test.ts: runs every resealed mutation case and reports each
// non-engine failure to the parent. It runs in a worker so the parent can detect a hang (a synchronous
// infinite loop cannot be interrupted in-thread) from the progress cell it publishes before each case.

import { readdirSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { isMainThread, parentPort, workerData } from "node:worker_threads";
import { pageCrc } from "../src/format.ts";
import { EngineError, close, execute, loadEngine, loadUnicodeData } from "../src/tooling.ts";
import { specPath } from "./tomlmini.ts";

const PAGE_HEADER = 16;
const FORMAT_VERSION = 42;
const MASK64 = (1n << 64n) - 1n;

// splitmix64, so a failing case is reproducible from its seed (the Go test's resealedRNG).
export class Rng {
  s: bigint;
  constructor(seed: bigint) {
    this.s = seed & MASK64;
  }
  next(): bigint {
    this.s = (this.s + 0x9e3779b97f4a7c15n) & MASK64;
    let z = this.s;
    z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK64;
    z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK64;
    return z ^ (z >> 31n);
  }
  intn(n: number): number {
    return Number(this.next() % BigInt(n));
  }
  // interestingU32 are the directory values most likely to land on an unchecked bound.
  interestingU32(ps: number, pages: number): number {
    switch (this.intn(11)) {
      case 9:
      case 10:
        return 2 + this.intn(pages - 2); // an in-range body page: a misdirected but readable pointer
      case 0:
        return 0;
      case 1:
        return 1;
      case 2:
        return ps;
      case 3:
        return ps - PAGE_HEADER;
      case 4:
        return pages + this.intn(4);
      case 5:
        return 0xffffffff;
      case 6:
        return 0x7fffffff;
      case 7:
        return this.intn(64);
      default:
        return Number(this.next() & 0xffffffffn);
    }
  }
  byte(): number {
    return Number(this.next() & 0xffn);
  }
}

// mutatePage applies one seeded structural mutation to page p of image and reseals its CRC.
export function mutatePage(
  image: Uint8Array,
  ps: number,
  p: number,
  pages: number,
  r: Rng,
): string {
  const pg = image.subarray(p * ps, (p + 1) * ps);
  const dv = new DataView(pg.buffer, pg.byteOffset, pg.byteLength);
  let what: string;
  switch (r.intn(8)) {
    case 0: {
      let off = r.intn(ps - 4);
      if (off >= 12) off += 4;
      pg[off] = r.byte();
      what = `byte ${off}`;
      break;
    }
    case 1: {
      const off = PAGE_HEADER + r.intn(ps - PAGE_HEADER - 4);
      dv.setUint32(off, r.interestingU32(ps, pages), false);
      what = `u32 at ${off}`;
      break;
    }
    case 2: {
      const off = PAGE_HEADER + r.intn(ps - PAGE_HEADER - 2);
      dv.setUint16(off, r.interestingU32(ps, pages) & 0xffff, false);
      what = `u16 at ${off}`;
      break;
    }
    case 3:
      dv.setUint32(4, r.interestingU32(ps, pages), false);
      what = "item count";
      break;
    case 4:
      dv.setUint32(8, r.interestingU32(ps, pages), false);
      what = "next page";
      break;
    case 5:
      pg[0] = r.intn(10);
      what = "page type";
      break;
    case 6: {
      const src = 2 + r.intn(pages - 2);
      pg.set(image.slice(src * ps, (src + 1) * ps));
      return `copy of page ${src}`;
    }
    default:
      // The bound is re-drawn each iteration, as in the Go loop.
      for (let k = 0; k < 2 + r.intn(6); k++) pg[PAGE_HEADER + r.intn(ps - PAGE_HEADER)] = r.byte();
      what = "byte burst";
  }
  dv.setUint32(12, pageCrc(pg), false);
  return what;
}

// trimToHighWater cuts an image to its committed page high-water (max pageCount over the valid meta
// slots) × page size, dropping trailing preallocation slack the loader never reads.
export function trimToHighWater(image: Uint8Array): Uint8Array {
  if (image.length < 36) return image;
  const dv = new DataView(image.buffer, image.byteOffset, image.byteLength);
  const ps = dv.getUint32(8, false);
  if (ps < 36 || image.length < 2 * ps) return image;
  let high = 0;
  for (let slot = 0; slot < 2; slot++) {
    const base = slot * ps;
    if (dv.getUint16(base + 4, false) !== FORMAT_VERSION) continue;
    const pc = dv.getUint32(base + 24, false);
    if (pc > high) high = pc;
  }
  if (high === 0 || high * ps > image.length) return image;
  return image.slice(0, high * ps);
}

export function quote(name: string): string {
  return `"${name.replaceAll('"', '""')}"`;
}

// runCase opens image and runs stmts: "ok" when everything succeeded, "closed" when some step
// failed with an engine error, otherwise the first non-engine thrown value.
export function runCase(image: Uint8Array, stmts: string[]): unknown {
  let outcome = "ok";
  try {
    const db = loadEngine(image);
    try {
      for (const s of stmts) {
        try {
          execute(db, s);
        } catch (e) {
          if (!(e instanceof EngineError)) return e;
          outcome = "closed";
        }
      }
    } finally {
      close(db);
    }
  } catch (e) {
    if (!(e instanceof EngineError)) return e;
    outcome = "closed";
  }
  return outcome;
}

export function describe(e: unknown): string {
  if (e instanceof Error) {
    const frames = (e.stack ?? "")
      .split("\n")
      .slice(1, 5)
      .map((l) =>
        l
          .trim()
          .replace(/^at /, "")
          .replace(/\(.*\/(src|tests)\//, "("),
      )
      .join(" < ");
    return `${e.name}: ${e.message} at ${frames}`;
  }
  return String(e);
}

if (!isMainThread) runAll();

function runAll(): void {
  const { per, progress } = workerData as { per: number; progress: SharedArrayBuffer };
  const cell = new Int32Array(progress);
  loadUnicodeData(new Uint8Array(readFileSync(specPath("collation/fixtures/unicode.jucd"))));
  const dir = dirname(specPath("fileformat/fixtures/empty_db.jed"));
  const paths = readdirSync(dir)
    .filter((f) => f.endsWith(".jed"))
    .sort()
    .map((f) => join(dir, f));
  let cases = 0;
  let failedClosed = 0;
  let skipped = 0;
  for (let fi = 0; fi < paths.length; fi++) {
    const path = paths[fi]!;
    const base = basename(path);
    const raw = new Uint8Array(readFileSync(path));
    let names: string[];
    const updates = new Map<string, string>();
    try {
      const clean = loadEngine(raw);
      names = [...clean.committed.tables.keys()].sort();
      for (const n of names) {
        const t = clean.committed.tables.get(n)!;
        const c = t.columns.find((_, i) => !t.pk.includes(i));
        if (c !== undefined) updates.set(n, c.name);
      }
      close(clean);
    } catch {
      skipped++; // a fixture that is malformed by design
      continue;
    }
    // The UPDATE rewrites every row through the mutation path (re-encoding each value and moving its
    // index entries); it is skipped for a table whose every column is in its primary key.
    const stmts = names.flatMap((n) => {
      const col = updates.get(n);
      const update =
        col === undefined ? [] : [`UPDATE ${quote(n)} SET ${quote(col)} = ${quote(col)}`];
      return [
        `SELECT * FROM ${quote(n)}`,
        `SELECT count(*) FROM ${quote(n)}`,
        ...update,
        `DELETE FROM ${quote(n)}`,
      ];
    });
    const image = trimToHighWater(raw);
    const ps = new DataView(image.buffer, image.byteOffset, image.byteLength).getUint32(8, false);
    const pages = Math.floor(image.length / ps);
    for (let p = 2; p < pages; p++) {
      for (let m = 0; m < per; m++) {
        const seed = (BigInt(p) << 32n) | BigInt(m);
        const r = new Rng(seed ^ (BigInt(Buffer.byteLength(base)) << 48n));
        const mutated = image.slice();
        const what = mutatePage(mutated, ps, p, pages, r);
        Atomics.store(cell, 0, fi);
        Atomics.store(cell, 1, p);
        Atomics.store(cell, 2, m);
        Atomics.add(cell, 3, 1);
        cases++;
        const outcome = runCase(mutated, stmts);
        if (outcome === "closed") failedClosed++;
        else if (outcome !== "ok") {
          parentPort!.postMessage({
            kind: "failure",
            text: `${base} page ${p} mutation ${m} (${what}): ${describe(outcome)}`,
          });
        }
      }
    }
  }
  parentPort!.postMessage({
    kind: "done",
    cases,
    failedClosed,
    skipped,
    files: paths.map((p) => basename(p)),
  });
}
