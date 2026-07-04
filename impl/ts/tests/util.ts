// Shared test helpers (not a `*.test.ts` file, so the runner does not execute it).

import {
  drainOutcome,
  EngineError,
  type Outcome,
  queryOutcome,
  render,
  type Session,
} from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

// Re-export the white-box query-seam drain helpers so a converted test can import them alongside the
// other shared helpers here (they live in tooling.ts, the internal surface — the public seam is
// `query -> Rows`; these drain it into an Outcome for assertion, exactly as the removed `execute ->
// Outcome` API returned; CLAUDE.md §10).
export { drainOutcome, queryOutcome };
export type { Outcome };

// Handle is the structural surface converted test helpers drive on a `db` parameter — satisfied by
// both the public Session (a converted feature test's handle) and Database (a from-image / file-backed
// handle). Feature tests route through the Database/Session envelope, never the low-level Engine.
// (Session-only knobs like setMaxCost/status are called on concrete `const db = ...` vars, not through
// a Handle-typed helper param, so they stay off this surface.)
export type Handle = Pick<
  Session,
  | "execute"
  | "query"
  | "executeScript"
  | "tableNames"
  | "table"
  | "compositeType"
  | "rowsInKeyOrder"
  | "collations"
  | "loadedCollations"
  | "defaultCollation"
  | "txid"
  | "pageSize"
  | "pageCount"
  | "path"
  | "readOnly"
>;

// dbWith builds an in-memory database and runs the given setup statements on a fresh session, failing
// loudly. The returned Session is stateful across calls (an autocommit handle over the shared core).
export function dbWith(stmts: string[], pageSize?: number): Session {
  // A page-backed B-tree's fan-out tracks the page size, so an in-memory tree must be built at the
  // size it will serialize to (format.md) — a test that round-trips through toImage(pageSize) must
  // pass that pageSize here (matching how the Rust/Go tests create the DB), or a PAX leaf's directory
  // overhead can overflow the smaller serialize target. Default page size otherwise.
  const db = memDb(pageSize).session();
  for (const s of stmts) {
    try {
      db.execute(s);
    } catch (e) {
      throw new Error(`setup ${JSON.stringify(s)}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return db;
}

// query runs a SELECT and returns its rows rendered as strings (NULL → "NULL"). It drains the real
// total-`query` seam (queryOutcome), not a parallel exec path.
export function query(db: Handle, sql: string): string[][] {
  const o = queryOutcome(db, sql);
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
