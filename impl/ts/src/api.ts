// Host API surface for the TS core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Free functions taking a
// Engine (mirroring the existing `execute(db, sql)` style — and avoiding an executor.ts↔api.ts
// import cycle). Thin wrappers over the parser + executor — the conformance contract still binds.

import type { Statement } from "./ast.ts";
import { throwIfAborted } from "./cancel.ts";
import { Cursor } from "./cursor.ts";
import {
  drainRun,
  type JsParam,
  type Row as ErgoRow,
  type RunResult,
  Statement as ErgoStatement,
} from "./ergonomic.ts";
import {
  type Engine,
  type InsertCacheHolder,
  type Outcome,
  type ScanCacheHolder,
  stmtIsWrite,
} from "./executor.ts";
import type { Value } from "./value.ts";

// PreparedStatement is a parsed, reusable statement (spec/design/api.md §2.4): a standalone value
// holding only the parsed AST and its plan cache — no session or database reference (the converged
// cross-core shape; Rust arrived here via the borrow checker, api.md §6). It is run by handing it to
// a handle's queryPrepared / executePrepared (Database, Session, Transaction — or the low-level free
// functions over a bare Engine): the handle chosen at each call supplies the session the execute
// observes (privilege envelope, pinned snapshot, temp domain), so one statement outlives any session
// and may be shared across sessions. The cached plan is keyed to the database + committed catalog
// generation it was resolved against, so DDL, a different database, or a temp-shadowed name re-plans
// rather than serving a stale plan.
export class PreparedStatement {
  /** @internal The parsed AST — reached by the handle queryPrepared paths, never a public field. */
  readonly ast: Statement;
  /**
   * @internal The plan-cache slot: memoizes the resolved scan plan across executes so a repeated
   * query skips planning (spec/design/api.md §2.4). Populated lazily on the first cacheable query
   * execute; invalidated automatically when the catalog generation moves (ScanCache). Query-only —
   * a write / materialized shape never touches it.
   */
  readonly scHolder: ScanCacheHolder = { cache: null };
  /** @internal The separately typed prepared-INSERT slot (api.md §2.4). */
  readonly icHolder: InsertCacheHolder = { cache: null };

  constructor(ast: Statement) {
    this.ast = ast;
  }
}

// executePrepared runs a prepared statement on a bare Engine handle, binding params to its $N
// placeholders, and returns its command tag — exec-side sugar over the total queryPrepared seam
// (drain-and-discard the rows, keep the tag). The shared-core surface is
// Database/Session/Transaction.executePrepared (spec/design/api.md §2.4).
export function executePrepared(
  db: Engine,
  stmt: PreparedStatement,
  params: Value[] = [],
): RunResult {
  return drainRun(queryPrepared(db, stmt, params));
}

// queryPrepared runs a prepared query on a bare Engine handle, returning a row cursor. The prepared
// AST routes through the same lazy scan (streaming/buffered) then deferred lanes as the ad-hoc query
// (spec/design/streaming.md §3/§4/§7) — so a prepared query streams exactly like a one-shot one —
// but reuses its cached plan across executes (spec/design/api.md §2.4). The shared-core surface is
// Database/Session/Transaction.queryPrepared.
export function queryPrepared(db: Engine, stmt: PreparedStatement, params: Value[] = []): Rows {
  return queryStmt(db, stmt.ast, params, stmt.scHolder, stmt.icHolder);
}

// Rows is a cursor over a query's rows (spec/design/api.md §4). It is a thin wrapper over a Cursor
// pull source (cursor.ts) and is iterable (for..of yields one Value[] per row). As of S1 the
// cursor's only shape is buffered — the executor materializes the full result and Rows walks it —
// but Rows is now defined against the Cursor seam, so a future streaming source (streaming.md §4)
// plugs in without any caller change. It is SINGLE-PASS (matching the Rust/Go cores and the
// streaming contract): once iterated it is drained.
export class Rows implements Iterable<Value[]> {
  readonly columnNames: string[];
  // The canonical type name of each output column (parallel to columnNames), carried on the total query
  // seam so a streaming read exposes its types like the materialized Outcome did — i16/text/decimal/…,
  // or "unknown" for an untyped NULL column (spec/design/conformance.md §7). Empty for a non-query
  // statement.
  readonly columnTypes: string[];
  private readonly cursor: Cursor;
  // The command tag for a statement run through the now-total query seam (spec/design/api.md §4): how
  // many rows a DML statement (INSERT/UPDATE/DELETE without RETURNING) touched. null for a SELECT / DDL
  // / transaction control, which carry no count. This is how the exec-side path (Statement.run) reads
  // the tag off a drained Rows — "run is throw away the rows, keep the count."
  private readonly rowsAffectedValue: number | null;
  // The reader-liveness pin's deregister (the watermark, streaming.md §5) — set by Session.query for a
  // streaming cursor, called by close (JS has no destructor; the ergonomic iterators close on loop
  // exit). undefined for a buffered cursor or a bare single-handle stream.
  private onClose?: () => void;
  // Fired ONCE when iteration first hits a terminal error (a drain-time streaming/deferred fault). Set
  // by the query call site (attachBlockPoison) for a read inside an open block, to abort the block — the
  // open-time lane errors are poisoned at the query return, this covers the errors that surface only
  // during the caller's drain. undefined for an autocommit read or a buffered cursor.
  private onErr?: () => void;

  constructor(
    columnNames: string[],
    columnTypes: string[],
    cursor: Cursor,
    rowsAffected: number | null,
  ) {
    this.columnNames = columnNames;
    this.columnTypes = columnTypes;
    this.cursor = cursor;
    this.rowsAffectedValue = rowsAffected;
  }

  // attachPin records the reader-liveness pin's deregister for a streaming cursor (the watermark,
  // streaming.md §5); close calls it.
  attachPin(deregister: () => void): void {
    this.onClose = deregister;
  }

  // attachErrHook records a callback fired once when iteration first hits a terminal error (the query
  // call site's attachBlockPoison uses it to abort an open block on a drain-time read fault).
  attachErrHook(hook: () => void): void {
    this.onErr = hook;
  }

  // fireErr fires the drain-time error hook exactly once (the iterator short-circuits after, so it
  // cannot double-fire).
  private fireErr(): void {
    if (this.onErr) {
      const hook = this.onErr;
      this.onErr = undefined;
      hook();
    }
  }

  // cost is the deterministic execution cost accrued by the query (CLAUDE.md §13). Final once the
  // cursor is drained (streaming.md §6); for a buffered cursor it is final immediately.
  get cost(): bigint {
    return this.cursor.cost();
  }

  // rowsAffected is the command tag carried on a Rows: how many rows a DML statement (INSERT/UPDATE/
  // DELETE without RETURNING) touched; null for a SELECT / DDL / transaction control, which have no
  // count (spec/design/api.md §4). The exec-side Statement.run reads this off a drained Rows.
  get rowsAffected(): number | null {
    return this.rowsAffectedValue;
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
    const self = this;
    return {
      // nextRow may throw mid-drain for a streaming cursor (a 54P01 cost abort or an arithmetic trap);
      // the throw propagates out of the for..of as the statement's error (streaming.md §6). Before it
      // propagates, fire the block-poison hook once: a drain-time read fault inside an open block aborts
      // it, so the next statement is 25P02 rather than wrongly running (transactions.md §6).
      next(): IteratorResult<Value[]> {
        let row: Value[] | undefined;
        try {
          row = cursor.nextRow();
        } catch (e) {
          self.fireErr();
          throw e;
        }
        return row !== undefined
          ? { done: false, value: row }
          : { done: true, value: undefined as unknown as Value[] };
      },
    };
  }
}

// rowsFromOutcome wraps a materialized outcome as a Rows. It is TOTAL: a non-query statement is
// observably a Rows with no output columns — an empty buffered cursor seeded with the accrued cost,
// carrying the statement's command tag (rows-affected). This is the single exec/query seam: the
// exec-side path (Statement.run) drains-and-discards such a Rows and returns the tag, so "query on a
// statement that produces no rows" is valid, not a 42601 (the effect-then-error bug this removes — a
// write reached here after dispatch already committed it; spec/design/api.md §11).
export function rowsFromOutcome(out: Outcome): Rows {
  if (out.kind === "query") {
    return new Rows(out.columnNames, out.columnTypes, Cursor.buffered(out.rows, out.cost), null);
  }
  return new Rows([], [], Cursor.buffered([], out.cost), out.rowsAffected);
}

// prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4): a standalone
// value bound to no handle — db only supplies the parse (its 54000 input-size limit). Run it with
// queryPrepared / executePrepared on any handle. Parse errors (42601, …) surface here.
export function prepare(db: Engine, sql: string): PreparedStatement {
  return new PreparedStatement(db.parse(sql));
}

// query is a one-shot: parse + run a query sql, binding params, returning a row cursor. A single-table
// no-blocking-operator read is served by a lazy STREAMING cursor (spec/design/streaming.md §4, S3); a
// blocking read (ORDER BY/DISTINCT/aggregate/window/join) by a lazy BUFFERED cursor (S4) that buffers
// the input but yields the output one row at a time. Both pull over a pinned snapshot with bounded peak
// output memory and a caller early-exit; a top-level set operation / pure-query WITH is served by a lazy
// DEFERRED cursor (streaming.md §7) that defers the run to the first pull and yields the result one row
// at a time. (This is the bare single-handle Engine; the watermark pin lives on the shared-core
// Session.query path.)
export function query(db: Engine, sql: string, params: Value[] = []): Rows {
  return queryStmt(db, db.parse(sql), params, null, null);
}

// queryStmt routes an already-parsed query AST through the plan-once scan (streaming/buffered) then
// deferred lanes, falling back to the materialized executeStmtParams for a shape no lazy lane covers
// (a write, a nextval/setval SELECT, a data-modifying WITH). Shared by query (parse-then-route, holder
// null) and a prepared query (PreparedStatement.query passes its ScanCacheHolder), so a prepared query
// streams identically to an ad-hoc one but reuses its cached plan across executes. (This is the bare
// single-handle Engine; the watermark pin lives on the shared-core Session.query path.)
function queryStmt(
  db: Engine,
  stmt: Statement,
  params: Value[],
  holder: ScanCacheHolder | null,
  insertHolder: InsertCacheHolder | null,
): Rows {
  // attachBlockPoison: a DRAIN-time read fault inside an open block aborts it (open-time lane errors are
  // poisoned by the catch below); a no-op for an autocommit read. The hook re-checks the block at error
  // time, so poisoning an already-ended block is harmless (transactions.md §6).
  const attachPoison = (rows: Rows): Rows => {
    if (db.session.tx !== null) rows.attachErrHook(() => db.failOpenBlock());
    return rows;
  };
  // A read served by a lazy lane never reaches the materialized executeStmtParams, so enforce the
  // read-path admission gates (25P02 / 54P02 / 42501) here — reads only: transaction control must still
  // work in a failed block, and a write is gated inside executeStmtParams on the fall-through below (the
  // safe-total-query contract, CLAUDE.md §13). An open-time lane error poisons the block (the catch).
  const isRead =
    stmt.kind !== "begin" &&
    stmt.kind !== "commit" &&
    stmt.kind !== "rollback" &&
    !stmtIsWrite(stmt);
  try {
    if (isRead) db.gateReadLanes(stmt);
    const scanned = db.tryScanQuery(stmt, params, holder);
    if (scanned !== null) {
      return attachPoison(new Rows(scanned.columnNames, scanned.columnTypes, scanned.cursor, null));
    }
    const deferred = db.tryDeferredQuery(stmt, params);
    if (deferred !== null) {
      return attachPoison(
        new Rows(deferred.columnNames, deferred.columnTypes, deferred.cursor, null),
      );
    }
  } catch (e) {
    db.failOpenBlock();
    throw e;
  }
  // The materialized dispatch fall-through (a write / nextval / data-modifying WITH / transaction
  // control) self-poisons in executeStmtParams's block branch with the right nuance (a nested BEGIN's
  // 25001 must NOT poison), so it is left intact — only the lazy-lane reads above, which bypass it, are
  // poisoned by the catch.
  return rowsFromOutcome(db.executeStmtParams(stmt, params, insertHolder));
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

  // execute runs a (possibly mutating) statement within this transaction, binding params, and returns
  // its command tag (exec-side sugar over the total query seam — drain-and-discard, keep the tag). A
  // write in a READ ONLY transaction is 25006; a statement error aborts the block (every later
  // statement but commit/rollback is then 25P02).
  execute(sql: string, params: Value[] = []): RunResult {
    return drainRun(this.query(sql, params));
  }

  // query runs a query within this transaction, returning a row cursor over the total query seam (a
  // non-query statement is a Rows with no output columns, carrying the command tag).
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.db.executeStmtParams(this.db.parse(sql), params));
  }

  // prepareStatement parses sql once into a reusable PreparedStatement (spec/design/api.md §2.4): a
  // standalone value bound to no handle — run it with queryPrepared / executePrepared on any handle
  // over this database. (Named prepareStatement because prepare() is the better-sqlite3-style
  // ergonomic Statement, api.md §11 — a TS-only naming divergence, api.md §6.)
  prepareStatement(sql: string): PreparedStatement {
    return new PreparedStatement(this.db.parse(sql));
  }

  // executePrepared runs a prepared statement within this transaction, binding params, and returns
  // its command tag — the prepared analogue of execute (spec/design/api.md §2.4).
  executePrepared(stmt: PreparedStatement, params: Value[] = []): RunResult {
    return drainRun(this.queryPrepared(stmt, params));
  }

  // queryPrepared runs a prepared query within this transaction (against its working set), returning
  // a row cursor — the prepared analogue of query. The transaction path is materialized (no lazy
  // SELECT lane), but it still threads the independent INSERT cache so a row-only block whose visible
  // schema matches committed state can fill and reuse immutable DML resolution (api.md §2.4).
  queryPrepared(stmt: PreparedStatement, params: Value[] = []): Rows {
    return rowsFromOutcome(this.db.executeStmtParams(stmt.ast, params, stmt.icHolder));
  }

  // executeCancelable runs a statement within this transaction under an AbortSignal (spec/design/
  // api.md §11.4): an already-aborted signal throws 57014 before any work, which — like any error —
  // poisons the block (25P02 on the next statement). TS is synchronous, so the check is at this
  // boundary only (cancel.ts).
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): RunResult {
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
