/// <reference lib="webworker" />
// The engine running inside a dedicated Web Worker (spec/design/hosts.md §5). OPFS sync access handles
// are only usable off the main thread, and the engine above the storage seam is synchronous, so the
// whole TS core runs HERE; the main thread drives it over postMessage — the async RPC client is
// client.ts. One worker owns at most one open database, i.e. one exclusive sync access handle, which is
// the single-writer guarantee (CLAUDE.md §3). Browser/worker only.
//
// Imports deliberately avoid lib.ts: that barrel re-exports the Node `fs` host (file.ts → node:fs),
// which a browser bundle cannot load. Everything here comes straight from the node-clean engine modules
// (opfs.ts / executor.ts / parser.ts / value.ts / errors.ts) — the graph the import trace proved free of
// `node:*`.

import { EngineError } from "../errors.ts";
import type { Engine, Outcome } from "../executor.ts";
import {
  closeOpfs,
  createOpfs,
  type DatabaseOptions,
  type OpenOptions,
  openOpfs,
} from "../opfs.ts";
import { parseSQL } from "../parser.ts";
import { render } from "../value.ts";

// Req is the client→worker wire protocol (client.ts is the other end). Each request carries a unique id
// the reply echoes.
type Req =
  | { id: number; op: "open"; name: string; opts?: OpenOptions }
  | { id: number; op: "create"; name: string; opts?: DatabaseOptions }
  | { id: number; op: "run"; sql: string }
  | { id: number; op: "commit" }
  | { id: number; op: "rollback" }
  | { id: number; op: "close" };

// RunResult is a serialized statement outcome: a query's rendered rows + column names, or a statement's
// affected-row count. Rows are rendered to strings (render, value.ts) because Value carries class
// instances (Decimal, …) that structured clone would not preserve across the worker boundary; cost is a
// decimal string for the same JSON-friendly reason. This mirrors the CLI/conformance display form; a
// typed-value wire form is a later enhancement.
type RunResult =
  | { kind: "query"; columnNames: string[]; rows: string[][]; cost: string }
  | { kind: "statement"; rowsAffected: number | null; cost: string };

// The single open database this worker owns (the exclusive OPFS handle, hosts.md §5). null until open/create.
let db: Engine | null = null;

function requireDb(): Engine {
  // Protocol misuse (a run/commit before open), not a SQL engine error — a plain Error, which the
  // message loop serializes with the generic XX000 code.
  if (db === null) {
    throw new Error("no database is open in this worker");
  }
  return db;
}

function serializeOutcome(out: Outcome): RunResult {
  if (out.kind === "query") {
    return {
      kind: "query",
      columnNames: out.columnNames,
      rows: out.rows.map((r) => r.map(render)),
      cost: out.cost.toString(),
    };
  }
  return { kind: "statement", rowsAffected: out.rowsAffected, cost: out.cost.toString() };
}

async function handle(req: Req): Promise<unknown> {
  switch (req.op) {
    case "open":
      if (db !== null) throw new Error("a database is already open in this worker");
      db = await openOpfs(req.name, req.opts);
      return null;
    case "create":
      if (db !== null) throw new Error("a database is already open in this worker");
      db = await createOpfs(req.name, req.opts);
      return null;
    case "run":
      // Parse + run one statement under autocommit (or the current explicit block); BEGIN/COMMIT/
      // ROLLBACK in the SQL drive the block directly (transactions.md §4.2). Bind params is a later
      // enhancement (Value is not trivially structured-cloneable) — params are empty for now.
      return serializeOutcome(requireDb().executeStmtParams(parseSQL(req.sql), []));
    case "commit":
      requireDb().commitTx();
      return null;
    case "rollback":
      requireDb().rollbackTx();
      return null;
    case "close":
      if (db !== null) {
        closeOpfs(db);
        db = null;
      }
      return null;
  }
}

// The message loop: run the request, post back {id, ok, result} or {id, ok:false, error:{code,message}}.
// Serializing EngineError to {code,message} is what keeps api.md §5/§7's structured-error contract
// intact across the worker boundary (the client rethrows a matching error).
addEventListener("message", (ev: MessageEvent<Req>) => {
  const req = ev.data;
  handle(req).then(
    (result) => postMessage({ id: req.id, ok: true, result }),
    (e: unknown) =>
      postMessage({
        id: req.id,
        ok: false,
        error: {
          code: e instanceof EngineError ? e.code() : "XX000",
          message: e instanceof Error ? e.message : String(e),
        },
      }),
  );
});
