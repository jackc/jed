// The main-thread client for the Browser/OPFS host (spec/design/hosts.md §5): a thin async wrapper over
// the Web Worker that runs the engine (worker.ts). The engine and the storage seam stay synchronous; the
// ONLY async boundary is here — postMessage RPC to the worker — which is also why the browser entry
// points are async (a documented per-platform divergence from the synchronous file create/open, api.md
// §6). One client drives one worker drives one open database (the exclusive OPFS handle, single-writer —
// CLAUDE.md §3). Browser only (Worker / MessageEvent).
//
// Errors raised in the worker are rethrown here as OpfsError carrying the engine SQLSTATE, so api.md
// §5/§7's structured-error contract survives the worker boundary.

import type { DatabaseOptions, OpenOptions } from "../opfs.ts";

// A query's result, as it crosses the worker boundary: rendered string rows + column names + cost (the
// worker renders Values to strings since Value carries class instances structured clone can't preserve —
// worker.ts). A statement (DML/DDL) reports its affected-row count instead.
export type QueryResult = { columnNames: string[]; rows: string[][]; cost: string };
export type StatementResult = { rowsAffected: number | null; cost: string };

// OpfsError mirrors an engine EngineError across the worker boundary: the SQLSTATE plus its message.
export class OpfsError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "OpfsError";
    this.code = code;
  }
}

type Reply = { id: number; ok: true; result: unknown } | { id: number; ok: false; error: { code: string; message: string } };
type Pending = { resolve: (v: unknown) => void; reject: (e: unknown) => void };

// OpfsDatabase is an open OPFS-backed database, driven through the worker. Create one with
// OpfsDatabase.create / .open; run statements with query / execute; end with close.
export class OpfsDatabase {
  private worker: Worker;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private closed = false;

  private constructor(worker: Worker) {
    this.worker = worker;
    this.worker.addEventListener("message", (ev: MessageEvent<Reply>) => this.onReply(ev.data));
  }

  // create makes a new OPFS-backed database named `name` (58P02 if it exists); open opens an existing one
  // (58P01 if absent). Each spins up its own worker (which acquires the exclusive sync access handle) and
  // resolves once the worker confirms the database is open.
  static async create(name: string, opts?: DatabaseOptions): Promise<OpfsDatabase> {
    const db = new OpfsDatabase(OpfsDatabase.spawnWorker());
    await db.call("create", { name, opts });
    return db;
  }

  static async open(name: string, opts?: OpenOptions): Promise<OpfsDatabase> {
    const db = new OpfsDatabase(OpfsDatabase.spawnWorker());
    await db.call("open", { name, opts });
    return db;
  }

  // spawnWorker constructs the engine worker. The new URL(..., import.meta.url) form is what lets a
  // bundler (Vite) emit the worker as its own module chunk.
  private static spawnWorker(): Worker {
    return new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
  }

  // query runs a SELECT (or RETURNING / set operation) and returns its rendered rows + columns + cost.
  async query(sql: string): Promise<QueryResult> {
    const r = (await this.call("run", { sql })) as { kind: string } & QueryResult;
    if (r.kind !== "query") throw new OpfsError("XX000", "query() called on a statement that produces no rows; use execute");
    return { columnNames: r.columnNames, rows: r.rows, cost: r.cost };
  }

  // execute runs a (possibly mutating) statement and returns its affected-row count + cost.
  async execute(sql: string): Promise<StatementResult> {
    const r = (await this.call("run", { sql })) as { kind: string } & StatementResult;
    if (r.kind === "query") throw new OpfsError("XX000", "execute() called on a query; use query");
    return { rowsAffected: r.rowsAffected, cost: r.cost };
  }

  // commit / rollback drive the current explicit transaction (a no-op under autocommit, where each
  // statement already committed — transactions.md §4.2).
  async commit(): Promise<void> {
    await this.call("commit", {});
  }

  async rollback(): Promise<void> {
    await this.call("rollback", {});
  }

  // close releases the worker (and with it the exclusive OPFS handle). Idempotent.
  async close(): Promise<void> {
    if (this.closed) return;
    await this.call("close", {});
    this.closed = true;
    this.worker.terminate();
    // Any still-pending calls can never resolve once the worker is gone.
    for (const p of this.pending.values()) p.reject(new OpfsError("XX000", "database closed"));
    this.pending.clear();
  }

  // call posts one request and returns a promise resolved by the matching reply id.
  private call(op: string, args: Record<string, unknown>): Promise<unknown> {
    const id = this.nextId++;
    return new Promise<unknown>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, op, ...args });
    });
  }

  private onReply(reply: Reply): void {
    const p = this.pending.get(reply.id);
    if (p === undefined) return;
    this.pending.delete(reply.id);
    if (reply.ok) {
      p.resolve(reply.result);
    } else {
      p.reject(new OpfsError(reply.error.code, reply.error.message));
    }
  }
}
