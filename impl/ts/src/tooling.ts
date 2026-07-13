// In-repo internal entry point — NOT the embedding API. Re-exports the public barrel (./lib.ts) PLUS
// the low-level single-threaded `Engine` handle, its functional one-shot/transaction helpers, and the
// golden/byte tooling (`loadEngine` / `toImage`). The conformance harness, benches, and unit tests
// import from here; embedders import from ./lib.ts (the converged Database/Session surface).
//
// TS cannot language-enforce the public/internal split, so this file IS the convention — the analogue
// of Rust's doc-hidden `tooling` module and Go's unexported internals (CLAUDE.md §2). The browser
// worker is the one other internal consumer; it deep-imports executor.ts/opfs.ts/parser.ts directly
// (the Node-`fs`-free seam, hosts.md §5) rather than through here.

import type { Rows } from "./api.ts";
import type { Engine, Outcome } from "./executor.ts";
import { FileSpillSink } from "./spillfile.ts";
import type { ScriptSummary } from "./split.ts";
import type { Value } from "./value.ts";

// The entire public embedding API.
export * from "./lib.ts";

// `Outcome` is dropped from the public ./lib.ts export (the public seam is `query -> Rows`), but the
// core's internal statement result stays nameable here — the conformance harness, benches, and unit
// tests render/assert against it (the analogue of Rust's `pub(crate) Outcome` reachable in-crate).
export type { Outcome } from "./executor.ts";

// The low-level handle + its functional API (every helper whose signature names `Engine`), plus the
// golden/byte tooling. Kept out of ./lib.ts so the public surface is the converged handles only.
export { Engine } from "./executor.ts";
export { loadEngine, toImage } from "./format.ts";
export { create, open, commit, rollback, close } from "./file.ts";
import { query } from "./api.ts";
export {
  begin,
  executePrepared,
  prepare,
  query,
  queryPrepared,
  querySql,
  update,
  view,
} from "./api.ts";

// drainOutcome pulls a total-`query` cursor to exhaustion and packages the result set + command tag as
// an Outcome — the shape the removed `execute -> Outcome` API returned, but built over the seam callers
// actually use (CLAUDE.md §10: tests assert on the real `query` seam, not a parallel exec path). A
// cursor carrying output columns is a query; a no-column cursor IS a non-query statement (the total-
// `query` contract). Cost + rows-affected are read after the drain (a streaming cursor accrues cost as
// it is pulled), and the pin is released via close().
export function drainOutcome(rows: Rows): Outcome {
  const columnNames = rows.columnNames;
  const columnTypes = rows.columnTypes;
  const collected: Value[][] = [];
  for (const row of rows) collected.push(row);
  const cost = rows.cost;
  const rowsAffected = rows.rowsAffected;
  rows.close();
  if (columnNames.length > 0) {
    return { kind: "query", columnNames, columnTypes, rows: collected, cost };
  }
  return { kind: "statement", cost, rowsAffected };
}

// queryOutcome runs sql through a handle's real `query` seam and materializes the cursor into an
// Outcome (the white-box test/tooling helper — see drainOutcome). Works for any handle exposing the
// total `query` seam (Database / Session / Transaction).
export function queryOutcome(
  handle: { query(sql: string, params: Value[]): Rows },
  sql: string,
  params: Value[] = [],
): Outcome {
  return drainOutcome(handle.query(sql, params));
}

// execute parses and runs one SQL statement against a low-level `Engine` (no bind parameters) and
// materializes it into an Outcome, draining the total `query` seam.
export function execute(db: Engine, sql: string): Outcome {
  return drainOutcome(query(db, sql));
}

// executeParams parses and runs one SQL statement against a low-level `Engine`, binding params to its
// $N placeholders (spec/design/api.md §5), materialized into an Outcome via the total `query` seam. A
// count mismatch is 42601; a parameter whose type cannot be inferred is 42P18; a bound value out of
// range / of the wrong family fails like a literal.
export function executeParams(db: Engine, sql: string, params: Value[]): Outcome {
  return drainOutcome(query(db, sql, params));
}

// executeScript runs a multi-statement SQL script against a low-level `Engine`'s default session
// (spec/design/session.md §4.2): split it, run each statement in order, discard the result rows, and
// return the O(1) ScriptSummary. All-or-nothing when the session is Idle (the migration/import path).
export function executeScript(db: Engine, sql: string): ScriptSummary {
  return db.executeScript(sql);
}

// setSpillDirForTest overrides a Database's host scratch directory for per-core spill tests. The
// public embedding surface deliberately has no configurable spill-target knob yet; tooling.ts is the
// white-box test barrel, so reaching the runtime-private core here does not widen lib.ts.
export function setSpillDirForTest(db: object, dir: string): void {
  const internal = db as { core: { storage: Engine } };
  internal.core.storage.spillSink = new FileSpillSink(dir);
}
