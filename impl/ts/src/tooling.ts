// In-repo internal entry point — NOT the embedding API. Re-exports the public barrel (./lib.ts) PLUS
// the low-level single-threaded `Engine` handle, its functional one-shot/transaction helpers, and the
// golden/byte tooling (`loadEngine` / `toImage`). The conformance harness, benches, and unit tests
// import from here; embedders import from ./lib.ts (the converged Database/Session surface).
//
// TS cannot language-enforce the public/internal split, so this file IS the convention — the analogue
// of Rust's doc-hidden `tooling` module and Go's unexported internals (CLAUDE.md §2). The browser
// worker is the one other internal consumer; it deep-imports executor.ts/opfs.ts/parser.ts directly
// (the Node-`fs`-free seam, hosts.md §5) rather than through here.

import type { Outcome } from "./executor.ts";
import type { Engine } from "./executor.ts";
import type { ScriptSummary } from "./split.ts";
import type { Value } from "./value.ts";

// The entire public embedding API.
export * from "./lib.ts";

// The low-level handle + its functional API (every helper whose signature names `Engine`), plus the
// golden/byte tooling. Kept out of ./lib.ts so the public surface is the converged handles only.
export { Engine } from "./executor.ts";
export { loadEngine, toImage } from "./format.ts";
export { create, open, commit, rollback, close } from "./file.ts";
export { begin, prepare, query, querySql, update, view } from "./api.ts";

// execute parses and executes one SQL statement against a low-level `Engine` (no bind parameters).
export function execute(db: Engine, sql: string): Outcome {
  return db.executeStmt(db.parse(sql));
}

// executeParams parses and executes one SQL statement against a low-level `Engine`, binding params to
// its $N placeholders (spec/design/api.md §5). A count mismatch is 42601; a parameter whose type cannot
// be inferred is 42P18; a bound value out of range / of the wrong family fails like a literal.
export function executeParams(db: Engine, sql: string, params: Value[]): Outcome {
  return db.executeStmtParams(db.parse(sql), params);
}

// executeScript runs a multi-statement SQL script against a low-level `Engine`'s default session
// (spec/design/session.md §4.2): split it, run each statement in order, discard the result rows, and
// return the O(1) ScriptSummary. All-or-nothing when the session is Idle (the migration/import path).
export function executeScript(db: Engine, sql: string): ScriptSummary {
  return db.executeScript(sql);
}
