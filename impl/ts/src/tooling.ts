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
import { createCodec, type Credential, DEFAULT_ARGON2_PARAMS } from "./crypto.ts";
import type { Engine, Outcome, Snapshot } from "./executor.ts";
import { toImage as toImageBytes } from "./format.ts";
import { seededRandomSource } from "./seam.ts";
import { FileSpillSink } from "./spillfile.ts";
import type { ScriptSummary } from "./split.ts";
import type { Value } from "./value.ts";
import type { Steps } from "./execution.ts";

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
export { create, open, openWithTransformSteps, commit, rollback, close } from "./file.ts";
// Query-memory accounting underflows (spec/design/memory.md) — the conformance harness's check.
export { queryMemoryPeak, queryMemoryUnderflows } from "./cost.ts";
// Recount every file commit's live pages by reachability (spec/design/memory.md §8.7).
export { verifyLivePages } from "./format.ts";
// Session-local temp domains spilled to a scratch file (spec/design/temp-tables.md §6).
export { tempSpills } from "./persist.ts";
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
// it is pulled), and the pin is released via close() — on a mid-drain error too, so a failed read
// never leaves a reader pinned in the watermark (JS has no destructor to release it later).
export function drainOutcome(rows: Rows): Outcome {
  const collected: Value[][] = [];
  try {
    for (const row of rows) collected.push(row);
  } catch (error) {
    rows.close();
    throw error;
  }
  return finishOutcome(rows, collected);
}

// drainOutcomeSteps is drainOutcome pulling through the cursor's retained continuation, so the
// asynchronous driver awaits every page read the drain makes (the async-codec conformance mode).
export function* drainOutcomeSteps(rows: Rows): Steps<Outcome> {
  const collected: Value[][] = [];
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) break;
      collected.push(step.value);
    }
  } catch (error) {
    rows.close();
    throw error;
  }
  return finishOutcome(rows, collected);
}

// finishOutcome packages a drained cursor, reading its final cost and command tag, then closes it.
function finishOutcome(rows: Rows, collected: Value[][]): Outcome {
  const columnNames = rows.columnNames;
  const columnTypes = rows.columnTypes;
  let cost: bigint;
  let rowsAffected: number | null;
  try {
    cost = rows.cost;
    rowsAffected = rows.rowsAffected;
  } finally {
    rows.close();
  }
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

// setSpillDirForTest overrides a Database's host scratch directory for per-core spill tests and the
// conformance harness's forced temp-spill mode (an in-memory database otherwise has none — temp-tables.md
// §6, spill.md §2). Sessions minted afterwards inherit it. The public embedding surface deliberately has
// no configurable spill-target knob yet; tooling.ts is the white-box test barrel, so reaching the
// runtime-private core here does not widen lib.ts.
export function setSpillDirForTest(db: object, dir: string): void {
  const internal = db as { core: { storage: Engine } };
  const storage = internal.core.storage;
  // An encrypted database has no scratch target (spec/fileformat/encrypted.md §10).
  if (storage.paging !== null && storage.paging.codec() !== null) return;
  storage.spillSink = new FileSpillSink(dir);
}

// encryptedImageForTesting is the from-scratch image of a Database's committed snapshot in the
// encrypted variant, under a fresh key block for credential whose key material — and every page nonce
// after it — comes from the insecure stream seeded with seed (spec/fileformat/encrypted.md §8): the
// bytes an encrypted_* golden pins. Test-only.
export function encryptedImageForTesting(
  db: object,
  seed: bigint,
  credential: Credential,
): Uint8Array {
  const internal = db as { core: { committed: Snapshot; storage: Engine } };
  const codec = createCodec(credential, DEFAULT_ARGON2_PARAMS, seededRandomSource(seed));
  return toImageBytes(internal.core.committed, internal.core.storage.pageSize, 1n, codec);
}

// hangUpNextPageServiceAckForTesting makes a serving page-service owner drop the connection instead of
// acknowledging its next client commit, after the commit is durable — the case commit_status resolves
// (spec/pageservice/README.md §6.4). Test-only; no effect on a handle that is not a page-service owner.
export function hangUpNextPageServiceAckForTesting(db: object): void {
  const internal = db as { core: { service: { hangUpNextAck(): void } | null } };
  internal.core.service?.hangUpNextAck();
}

// crashBeforeNextPageServiceAckForTesting makes a serving page-service owner end its process instead of
// acknowledging its next client commit, after the commit is durable, so the client resolves the commit
// under the next owner — itself, when it is the only other process (spec/pageservice/README.md §6.5).
// Test-only; no effect on a handle that is not a page-service owner.
export function crashBeforeNextPageServiceAckForTesting(db: object): void {
  const internal = db as { core: { service: { crashBeforeNextAck(): void } | null } };
  internal.core.service?.crashBeforeNextAck();
}

// pageServiceRoleForTesting reports a handle's page-service role: "owner", "client", or "none" (not
// under the page service, or between roles). Test-only (spec/pageservice/README.md §6.5).
export function pageServiceRoleForTesting(db: object): "owner" | "client" | "none" {
  const internal = db as {
    core: { handoff: object | null; roleKind: "owner" | "client" | "none" };
  };
  return internal.core.handoff === null ? "none" : internal.core.roleKind;
}

// pageServiceAttachmentRoleForTesting reports the page-service role of attachment name: "owner",
// "client", or "none" (not under the page service, between roles, or not attached). Test-only
// (spec/pageservice/README.md §6.6).
export function pageServiceAttachmentRoleForTesting(
  db: object,
  name: string,
): "owner" | "client" | "none" {
  const internal = db as {
    core: { attachments: Map<string, { svc: { db: object } | null }> };
  };
  const svc = internal.core.attachments.get(name)?.svc ?? null;
  return svc === null ? "none" : pageServiceRoleForTesting(svc.db);
}

// backupForTesting backs up a handle's main database to a new file, page-identical or compacted
// (spec/design/page-service.md §9). The host API's name and shape are not yet decided; test-only.
export { backupForTesting } from "./backup.ts";
