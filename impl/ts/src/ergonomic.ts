// better-sqlite3-style ergonomic host bindings for the TS core (spec/design/api.md §11).
//
// The raw handle surface (Database/Session/Transaction) speaks `Value[]` parameters and yields
// `Value[]` rows — full fidelity, but the caller hand-builds `intValue(1n)` and indexes columns
// positionally. This module layers the better-sqlite3 idiom on top, *additively*: the raw
// execute/query stay exactly as they are, and a `db.prepare(sql)` returns a Statement with
// `.run() / .get() / .all() / .iterate()` over plain JS values and rows-as-objects.
//
// This is a per-impl surface, NOT the shared conformance corpus (api.md §1): each core spells the
// ergonomics in its own idiom (Go: database/sql Scan; Rust: rusqlite traits; here: better-sqlite3),
// unit-tested per core. The conformance contract is untouched — every method funnels through the
// same parser + executor the raw path uses.
//
// Value mapping (the one place TS-native types meet the engine's strict types):
//
//   param (JS → Value):  bigint→int, boolean→bool, string→text, Uint8Array→bytea, Decimal→decimal,
//                        null/undefined→NULL, a raw Value passes through. A JS `number` maps to int
//                        when it is integer-valued (so `run(1)` binds an integer — JS cannot tell 1
//                        from 1.0) and to f64 otherwise. The binder then coerces to the column type.
//   result (Value → JS): int→bigint (i64 is exact — jed's identity), bool→boolean, f32/f64→number,
//                        text→string, bytea→Uint8Array. Every other type (decimal, uuid, the
//                        temporal types, array/range/json/composite) returns its canonical TEXT
//                        (render) — lossless and predictable; the raw `query` path stays for the
//                        engine Value itself. A richer structured mapping is a documented follow-up.

import type { Rows } from "./api.ts";
import { engineError } from "./errors.ts";
import { Decimal } from "./decimal.ts";
import {
  type Value,
  boolValue,
  byteaValue,
  decimalValue,
  float64Value,
  intValue,
  nullValue,
  render,
  textValue,
} from "./value.ts";

// JsParam is a native bind value accepted by the ergonomic methods (plus a raw engine Value).
export type JsParam =
  | null
  | undefined
  | bigint
  | boolean
  | number
  | string
  | Uint8Array
  | Decimal
  | Value;

// JsValue is a native column value the ergonomic methods return. Non-"clean-scalar" engine types
// (decimal/uuid/temporal/array/range/json/composite) arrive as their canonical text (a string).
export type JsValue = null | bigint | boolean | number | string | Uint8Array;

// Row is one result row as a plain object keyed by column name (better-sqlite3's default). On a
// duplicate column name the last column wins (use `.raw()`/the low-level query for positional rows).
export type Row = Record<string, JsValue>;

// RunResult is the command tag of a non-query statement (better-sqlite3's RunResult shape). `changes`
// is the affected-row count (0 for DDL / transaction control, which carry none — like PostgreSQL);
// `cost` is the deterministic execution cost accrued (CLAUDE.md §13). jed has no lastInsertRowid —
// use RETURNING to read generated columns back.
export type RunResult = { changes: number; cost: bigint };

// drainRun drains a total-query cursor and returns its command tag as a RunResult ({changes, cost}) —
// the shared exec-side lowering behind Statement.run AND every handle .execute* method: run the query
// seam, drain-and-discard the rows, keep the tag (a SELECT / DDL / transaction control carries no count
// → changes 0). The full drain surfaces a mid-drain streaming error (a 54P01 cost abort, 57014
// cancellation, or arithmetic trap) rather than dropping it. A raw streaming Rows is closed after
// draining — JS has no destructor and the iterator does not auto-close — so its reader-liveness pin is
// released in `finally` (spec/design/api.md §11, streaming.md §5).
export function drainRun(rows: Rows): RunResult {
  try {
    for (const _row of rows) {
      // drain-and-discard
    }
    return { changes: rows.rowsAffected ?? 0, cost: rows.cost };
  } finally {
    rows.close();
  }
}

// ErgonomicHandle is the raw surface a Statement runs on — satisfied structurally by Database,
// Session, and Transaction (all expose the total `query` seam over Value[]). run/get/all/iterate all
// route through `query` (run drains-and-discards for its command tag), the one internal exec/query seam.
export interface ErgonomicHandle {
  query(sql: string, params: Value[]): Rows;
}

// toValue converts one native JS bind value to an engine Value (the param mapping above).
export function toValue(arg: JsParam): Value {
  if (arg === null || arg === undefined) return nullValue();
  switch (typeof arg) {
    case "bigint":
      return intValue(arg);
    case "boolean":
      return boolValue(arg);
    case "number":
      // A JS number is binary64; bind an integer-valued one as an integer (so `run(1)` is an int),
      // anything fractional/non-finite as f64. The binder re-coerces to the inferred column type.
      return Number.isInteger(arg) ? intValue(BigInt(arg)) : float64Value(arg);
    case "string":
      return textValue(arg);
    case "object":
      if (arg instanceof Uint8Array) return byteaValue(arg);
      if (arg instanceof Decimal) return decimalValue(arg);
      if ("kind" in arg) return arg as Value; // a raw engine Value passes through
      break;
  }
  throw engineError("datatype_mismatch", `cannot use ${describe(arg)} as a bind parameter`);
}

// valueToJs converts one engine Value to a native JS value (the result mapping above).
export function valueToJs(v: Value): JsValue {
  switch (v.kind) {
    case "null":
      return null;
    case "int":
      return v.int; // bigint — i64 exact
    case "bool":
      return v.value;
    case "f32":
    case "f64":
      return v.value;
    case "text":
      return v.text;
    case "bytea":
      return v.bytes;
    default:
      // decimal / uuid / timestamp[tz] / date / interval / array / range / json / jsonb / jsonpath:
      // the canonical text — lossless and predictable. (A structured mapping is an api.md §11 follow-up.)
      return render(v);
  }
}

function describe(arg: unknown): string {
  if (Array.isArray(arg)) return "an array";
  return typeof arg === "object" ? "an object" : `a ${typeof arg}`;
}

function bindParams(params: JsParam[]): Value[] {
  return params.map(toValue);
}

function rowObject(values: Value[], names: string[]): Row {
  const out: Row = {};
  for (let i = 0; i < names.length; i++) out[names[i]!] = valueToJs(values[i]!);
  return out;
}

// Statement is a prepared statement bound to a handle (better-sqlite3's Statement). The SQL is held
// and re-parsed per call (jed's parser is cheap; parse caching is a future optimization), so every
// run routes through the handle's full session envelope — privileges, cost, transaction state.
export class Statement {
  private readonly handle: ErgonomicHandle;
  readonly source: string;

  constructor(handle: ErgonomicHandle, sql: string) {
    this.handle = handle;
    this.source = sql;
  }

  // run executes a statement, binding native params, and returns its command tag — exec-side sugar over
  // the total query seam: run, drain-and-discard the rows, read the tag off the drained cursor (a
  // SELECT / DDL / transaction control carries no count → `changes` 0). The full drain surfaces a
  // mid-drain streaming error (a 54P01 cost abort, 57014 cancellation, or arithmetic trap) rather than
  // dropping it. A raw streaming Rows must be closed after draining — JS has no destructor and the
  // iterator does not auto-close — so its reader-liveness pin is released in `finally` (api.md §11,
  // streaming.md §5); this is stricter than get/all/iterate, which drain without closing.
  run(...params: JsParam[]): RunResult {
    return drainRun(this.handle.query(this.source, bindParams(params)));
  }

  // get runs a query, binding native params, and returns its FIRST row as an object — or undefined
  // when the query produced no rows. Extra rows are not materialized beyond the first.
  get(...params: JsParam[]): Row | undefined {
    const rows = this.handle.query(this.source, bindParams(params));
    for (const values of rows) return rowObject(values, rows.columnNames);
    return undefined;
  }

  // all runs a query, binding native params, and returns every row as an object.
  all(...params: JsParam[]): Row[] {
    const rows = this.handle.query(this.source, bindParams(params));
    const out: Row[] = [];
    for (const values of rows) out.push(rowObject(values, rows.columnNames));
    return out;
  }

  // iterate runs a query, binding native params, and yields each row object lazily (over the
  // materialized result — true streaming is deferred per CLAUDE.md §9, but the contract is the seam).
  *iterate(...params: JsParam[]): IterableIterator<Row> {
    const rows = this.handle.query(this.source, bindParams(params));
    for (const values of rows) yield rowObject(values, rows.columnNames);
  }
}
