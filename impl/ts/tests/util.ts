// Shared test helpers (not a `*.test.ts` file, so the runner does not execute it).

import { Database, EngineError, execute, render } from "../src/lib.ts";

// dbWith builds a database and runs the given setup statements, failing loudly.
export function dbWith(stmts: string[]): Database {
  const db = new Database();
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
export function query(db: Database, sql: string): string[][] {
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
