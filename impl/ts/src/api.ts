// Host API surface for the TS core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Free functions taking a
// Engine (mirroring the existing `execute(db, sql)` style — and avoiding an executor.ts↔api.ts
// import cycle). Thin wrappers over the parser + executor — the conformance contract still binds.

import type { Statement } from "./ast.ts";
import { throwIfAborted } from "./cancel.ts";
import { Cursor } from "./cursor.ts";
import {
  type JsParam,
  type Row as ErgoRow,
  type RunResult,
  Statement as ErgoStatement,
} from "./ergonomic.ts";
import type { Engine, Outcome } from "./executor.ts";
import { engineError } from "./errors.ts";
import type { Value } from "./value.ts";

// PreparedStatement is a parsed, reusable statement (spec/design/api.md §2.4). It holds the
// parsed AST and a reference to the database it was prepared against (JS is GC'd, so binding the
// database at prepare is safe — unlike Rust's borrow model, api.md §6).
export class PreparedStatement {
  private readonly db: Engine;
  private readonly ast: Statement;

  constructor(db: Engine, ast: Statement) {
    this.db = db;
    this.ast = ast;
  }

  // execute runs this statement, binding params to its $N placeholders (empty when it has none),
  // returning the materialized outcome.
  execute(params: Value[] = []): Outcome {
    return this.db.executeStmtParams(this.ast, params);
  }

  // query runs this query statement, returning a row cursor. A non-query statement is a 42601
  // (use execute).
  query(params: Value[] = []): Rows {
    return rowsFromOutcome(this.execute(params));
  }
}

// Rows is a cursor over a query's rows (spec/design/api.md §4). It is a thin wrapper over a Cursor
// pull source (cursor.ts) and is iterable (for..of yields one Value[] per row). As of S1 the
// cursor's only shape is buffered — the executor materializes the full result and Rows walks it —
// but Rows is now defined against the Cursor seam, so a future streaming source (streaming.md §4)
// plugs in without any caller change. It is SINGLE-PASS (matching the Rust/Go cores and the
// streaming contract): once iterated it is drained.
export class Rows implements Iterable<Value[]> {
  readonly columnNames: string[];
  private readonly cursor: Cursor;
  // The reader-liveness pin's deregister (the watermark, streaming.md §5) — set by Session.query for a
  // streaming cursor, called by close (JS has no destructor; the ergonomic iterators close on loop
  // exit). undefined for a buffered cursor or a bare single-handle stream.
  private onClose?: () => void;

  constructor(columnNames: string[], cursor: Cursor) {
    this.columnNames = columnNames;
    this.cursor = cursor;
  }

  // attachPin records the reader-liveness pin's deregister for a streaming cursor (the watermark,
  // streaming.md §5); close calls it.
  attachPin(deregister: () => void): void {
    this.onClose = deregister;
  }

  // cost is the deterministic execution cost accrued by the query (CLAUDE.md §13). Final once the
  // cursor is drained (streaming.md §6); for a buffered cursor it is final immediately.
  get cost(): bigint {
    return this.cursor.cost();
  }

  // close releases the read snapshot the cursor pins (streaming.md §5): it closes the underlying
  // cursor (returning its generator) and deregisters the reader-liveness watermark pin (if any),
  // advancing oldestLiveTxid. Idempotent; a no-op for a buffered cursor (it pins nothing). The
  // ergonomic iterators close on loop exit; a raw streaming Rows must be closed (JS has no destructor).
  close(): void {
    this.cursor.close();
    if (this.onClose) {
      this.onClose();
      this.onClose = undefined;
    }
  }

  [Symbol.iterator](): Iterator<Value[]> {
    const cursor = this.cursor;
    return {
      // nextRow may throw mid-drain for a streaming cursor (a 54P01 cost abort or an arithmetic trap);
      // the throw propagates out of the for..of as the statement's error (streaming.md §6).
      next(): IteratorResult<Value[]> {
        const row = cursor.nextRow();
        return row !== undefined
          ? { done: false, value: row }
          : { done: true, value: undefined as unknown as Value[] };
      },
    };
  }
}

export function rowsFromOutcome(out: Outcome): Rows {
  if (out.kind !== "query") {
    throw engineError(
      "syntax_error",
      "query called on a statement that produces no rows; use execute",
    );
  }
  return new Rows(out.columnNames, Cursor.buffered(out.rows, out.cost));
}

// prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4). Parse
// errors (42601, …) surface here.
export function prepare(db: Engine, sql: string): PreparedStatement {
  return new PreparedStatement(db, db.parse(sql));
}

// query is a one-shot: parse + run a query sql, binding params, returning a row cursor. A single-table
// no-blocking-operator read is served by a lazy STREAMING cursor (spec/design/streaming.md §4, S3); a
// blocking read (ORDER BY/DISTINCT/aggregate/window/join) by a lazy BUFFERED cursor (S4) that buffers
// the input but yields the output one row at a time. Both pull over a pinned snapshot with bounded peak
// output memory and a caller early-exit; a set-operation / WITH top level falls back to the materialized
// execute() path. (This is the bare single-handle Engine; the watermark pin lives on the shared-core
// Session.query path.)
export function query(db: Engine, sql: string, params: Value[] = []): Rows {
  const stmt = db.parse(sql);
  const streamed = db.tryStreamingQuery(stmt, params);
  if (streamed !== null) return new Rows(streamed.columnNames, streamed.cursor);
  const buffered = db.tryBufferedQuery(stmt, params);
  if (buffered !== null) return new Rows(buffered.columnNames, buffered.cursor);
  return rowsFromOutcome(db.executeStmtParams(stmt, params));
}

// querySql is an alias for query, symmetric with the Rust/Go QuerySQL naming (api.md §6).
export const querySql = query;

// Transaction is an open explicit transaction (spec/design/api.md §2.2, transactions.md §4.4).
// Statements run through execute/query; commit/rollback end it. JS has no destructor, so a raw
// `begin` caller must end it explicitly — `view`/`update` do that automatically (and are preferred).
// TxEnd lets a Transaction route commit/rollback through a host (a Session) that does more than the
// raw Engine swap — e.g. publishing the working set to the shared core and releasing the writer gate.
// When absent, commit/rollback fall back to the bare Engine swap (the one-handle Engine API).
export interface TxEnd {
  commit(): void;
  rollback(): void;
}

export class Transaction {
  private readonly db: Engine;
  private done: boolean;
  private readonly end?: TxEnd;

  constructor(db: Engine, end?: TxEnd) {
    this.db = db;
    this.done = false;
    this.end = end;
  }

  // execute runs a (possibly mutating) statement within this transaction, binding params. A write
  // in a READ ONLY transaction is 25006; a statement error aborts the block (every later statement
  // but commit/rollback is then 25P02).
  execute(sql: string, params: Value[] = []): Outcome {
    return this.db.executeStmtParams(this.db.parse(sql), params);
  }

  // query runs a query within this transaction, returning a row cursor.
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.execute(sql, params));
  }

  // executeCancelable runs a statement within this transaction under an AbortSignal (spec/design/
  // api.md §11.4): an already-aborted signal throws 57014 before any work, which — like any error —
  // poisons the block (25P02 on the next statement). TS is synchronous, so the check is at this
  // boundary only (cancel.ts).
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Outcome {
    throwIfAborted(signal);
    return this.execute(sql, params);
  }

  // queryCancelable is the query sibling of executeCancelable (spec/design/api.md §11.4).
  queryCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Rows {
    throwIfAborted(signal);
    return this.query(sql, params);
  }

  // --- better-sqlite3-style ergonomic methods (spec/design/api.md §11): a reusable prepared
  // Statement, or one-shot run/get/all over native JS params + rows-as-objects, within this
  // transaction (each statement joins the open block). ---

  // prepare returns a reusable Statement bound to this transaction (better-sqlite3's db.prepare).
  prepare(sql: string): ErgoStatement {
    return new ErgoStatement(this, sql);
  }
  // run is the one-shot Statement.run: execute a statement with native params, return its command tag.
  run(sql: string, ...params: JsParam[]): RunResult {
    return new ErgoStatement(this, sql).run(...params);
  }
  // get is the one-shot Statement.get: the first row of a query as an object, or undefined.
  get(sql: string, ...params: JsParam[]): ErgoRow | undefined {
    return new ErgoStatement(this, sql).get(...params);
  }
  // all is the one-shot Statement.all: every row of a query as an object.
  all(sql: string, ...params: JsParam[]): ErgoRow[] {
    return new ErgoStatement(this, sql).all(...params);
  }

  // commit publishes the transaction durably (per synchronous). Idempotent after the transaction
  // has ended.
  commit(): void {
    if (this.done) return;
    this.done = true;
    if (this.end) this.end.commit();
    else this.db.commitTx();
  }

  // rollback discards the transaction's working set. Idempotent after the transaction has ended, so
  // the view/update wrappers can roll back in a catch even after a commit.
  rollback(): void {
    if (this.done) return;
    this.done = true;
    if (this.end) this.end.rollback();
    else this.db.rollbackTx();
  }
}

// begin opens an explicit transaction (spec/design/api.md §2.2). writable false is READ ONLY (a
// write inside → 25006); true is READ WRITE — 25006 on a read-only handle (§2.1). A nested begin
// (a transaction is already open) is 25001. Prefer view/update, which cannot forget to end the
// transaction.
export function begin(db: Engine, writable: boolean): Transaction {
  db.beginTx(writable);
  return new Transaction(db);
}

// view runs fn in a READ ONLY transaction (bbolt-style): open it, run fn(tx), then auto-commit on
// success / auto-rollback on a thrown error. A write inside is 25006.
export function view<R>(db: Engine, fn: (tx: Transaction) => R): R {
  return withTx(db, false, fn);
}

// update runs fn in a READ WRITE transaction (bbolt-style): open it, run fn(tx), then auto-commit
// on success / auto-rollback on a thrown error — the safe default over a raw begin.
export function update<R>(db: Engine, fn: (tx: Transaction) => R): R {
  return withTx(db, true, fn);
}

function withTx<R>(db: Engine, writable: boolean, fn: (tx: Transaction) => R): R {
  const tx = begin(db, writable);
  let result: R;
  try {
    result = fn(tx);
  } catch (e) {
    tx.rollback();
    throw e;
  }
  tx.commit();
  return result;
}
