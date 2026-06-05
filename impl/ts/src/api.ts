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
