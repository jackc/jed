// Shared test helpers (not a `*.test.ts` file, so the runner does not execute it).

import { Engine, EngineError, execute, render } from "../src/lib.ts";

// dbWith builds a database and runs the given setup statements, failing loudly.
export function dbWith(stmts: string[]): Engine {
  const db = new Engine();
  for (const s of stmts) {
    try {
      execute(db, s);
    } catch (e) {
      throw new Error(`setup ${JSON.stringify(s)}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return db;
}

// query runs a SELECT and returns its rows rendered as strings (NULL → "NULL").
export function query(db: Engine, sql: string): string[][] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows.map((r) => r.map(render));
}

// errCode runs fn and returns the SQLSTATE of the EngineError it throws; it fails if no
// error (or a non-EngineError) is thrown.
export function errCode(fn: () => void): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an EngineError, but no error was thrown");
}

// bytesToHex renders bytes as lowercase hex (no separators) — for encoding vectors.
export function bytesToHex(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += x.toString(16).padStart(2, "0");
  return s;
}

// bytesEqual reports whether two byte arrays are identical.
export function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32(seed "JEDB") mapped
// to a 64-char alphabet (text) or raw bytes (bytea hex literals). High-entropy, so the LZ4
// encoder never wins store-smaller and the value deterministically stays PLAIN. Mirrors
// verify.rb's filler_text/filler_bytes; each call restarts at the seed.
const FILLER_ALPHA64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function fillerStep(x: number): number {
  x = (x ^ (x << 13)) >>> 0;
  x = (x ^ (x >>> 17)) >>> 0;
  return (x ^ (x << 5)) >>> 0;
}

export function fillerText(n: number): string {
  let x = 0x4a454442;
  let out = "";
  for (let i = 0; i < n; i++) {
    x = fillerStep(x);
    out += FILLER_ALPHA64[x % 64]!;
  }
  return out;
}

export function fillerBytesHex(n: number): string {
  let x = 0x4a454442;
  let out = "";
  for (let i = 0; i < n; i++) {
    x = fillerStep(x);
    out += (x % 256).toString(16).padStart(2, "0");
  }
  return out;
}
