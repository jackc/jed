// Host API surface for the TS core (spec/design/api.md §1): prepare a statement, execute or
// query it (with optional $N bind parameters), and iterate result rows. Free functions taking a
// Database (mirroring the existing `execute(db, sql)` style — and avoiding an executor.ts↔api.ts
// import cycle). Thin wrappers over the parser + executor — the conformance contract still binds.

import type { Statement } from "./ast.ts";
import { Database, type Outcome } from "./executor.ts";
import { engineError } from "./errors.ts";
import { parseSQL } from "./parser.ts";
import type { Value } from "./value.ts";

// PreparedStatement is a parsed, reusable statement (spec/design/api.md §2.4). It holds the
// parsed AST and a reference to the database it was prepared against (JS is GC'd, so binding the
// database at prepare is safe — unlike Rust's borrow model, api.md §6).
export class PreparedStatement {
  private readonly db: Database;
  private readonly ast: Statement;

  constructor(db: Database, ast: Statement) {
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

// Rows is a cursor over a query's rows (spec/design/api.md §4). It is iterable (for..of yields one
// Value[] per row) over the MATERIALIZED result — true streaming is deferred per CLAUDE.md §9; the
// iterable contract is the seam that lets the source become lazy later without a caller change —
// and exposes the column names and the accrued execution cost.
export class Rows implements Iterable<Value[]> {
  readonly columnNames: string[];
  private readonly rows: Value[][];
  readonly cost: bigint;

  constructor(columnNames: string[], rows: Value[][], cost: bigint) {
    this.columnNames = columnNames;
    this.rows = rows;
    this.cost = cost;
  }

  [Symbol.iterator](): Iterator<Value[]> {
    let i = 0;
    const rows = this.rows;
    return {
      next(): IteratorResult<Value[]> {
        return i < rows.length
          ? { done: false, value: rows[i++]! }
          : { done: true, value: undefined as unknown as Value[] };
      },
    };
  }
}

function rowsFromOutcome(out: Outcome): Rows {
  if (out.kind !== "query") {
    throw engineError("syntax_error", "query called on a statement that produces no rows; use execute");
  }
  return new Rows(out.columnNames, out.rows, out.cost);
}

// prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4). Parse
// errors (42601, …) surface here.
export function prepare(db: Database, sql: string): PreparedStatement {
  return new PreparedStatement(db, parseSQL(sql));
}

// query is a one-shot: parse + run a query sql, binding params, returning a row cursor.
export function query(db: Database, sql: string, params: Value[] = []): Rows {
  return rowsFromOutcome(db.executeStmtParams(parseSQL(sql), params));
}

// querySql is an alias for query, symmetric with the Rust/Go QuerySQL naming (api.md §6).
export const querySql = query;

// Transaction is an open explicit transaction (spec/design/api.md §2.2, transactions.md §4.4).
// Statements run through execute/query; commit/rollback end it. JS has no destructor, so a raw
// `begin` caller must end it explicitly — `view`/`update` do that automatically (and are preferred).
export class Transaction {
  private readonly db: Database;
  private done: boolean;

  constructor(db: Database) {
    this.db = db;
    this.done = false;
  }

  // execute runs a (possibly mutating) statement within this transaction, binding params. A write
  // in a READ ONLY transaction is 25006; a statement error aborts the block (every later statement
  // but commit/rollback is then 25P02).
  execute(sql: string, params: Value[] = []): Outcome {
    return this.db.executeStmtParams(parseSQL(sql), params);
  }

  // query runs a query within this transaction, returning a row cursor.
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.execute(sql, params));
  }

  // commit publishes the transaction durably (per synchronous). Idempotent after the transaction
  // has ended.
  commit(): void {
    if (this.done) return;
    this.done = true;
    this.db.commitTx();
  }

  // rollback discards the transaction's working set. Idempotent after the transaction has ended, so
  // the view/update wrappers can roll back in a catch even after a commit.
  rollback(): void {
    if (this.done) return;
    this.done = true;
    this.db.rollbackTx();
  }
}

// begin opens an explicit transaction (spec/design/api.md §2.2). writable false is READ ONLY (a
// write inside → 25006); true is READ WRITE. A nested begin (a transaction is already open) is
// 25001. Prefer view/update, which cannot forget to end the transaction.
export function begin(db: Database, writable: boolean): Transaction {
  db.beginTx(writable);
  return new Transaction(db);
}

// view runs fn in a READ ONLY transaction (bbolt-style): open it, run fn(tx), then auto-commit on
// success / auto-rollback on a thrown error. A write inside is 25006.
export function view<R>(db: Database, fn: (tx: Transaction) => R): R {
  return withTx(db, false, fn);
}

// update runs fn in a READ WRITE transaction (bbolt-style): open it, run fn(tx), then auto-commit
// on success / auto-rollback on a thrown error — the safe default over a raw begin.
export function update<R>(db: Database, fn: (tx: Transaction) => R): R {
  return withTx(db, true, fn);
}

function withTx<R>(db: Database, writable: boolean, fn: (tx: Transaction) => R): R {
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
