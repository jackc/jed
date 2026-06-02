// Statement executor (CLAUDE.md §10). Mirrors executor.go / executor.rs: dispatch a
// parsed statement; analyze (resolve types, predicates, projections against the
// catalog); run. Errors throw EngineError. Results are produced deterministically
// (CLAUDE.md §10): scan in primary-key order, three-valued WHERE (only TRUE keeps a
// row), stable ORDER BY with NULLs first.

import type {
  CompareOp,
  CreateTable,
  Delete,
  Insert,
  Predicate,
  Select,
  SelectExpr,
  SelectItems,
  Statement,
  Update,
} from "./ast.ts";
import { type Column, type Table, columnIndex, primaryKeyIndex } from "./catalog.ts";
import { encodeInt } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { type Row, TableStore } from "./storage.ts";
import { type ScalarType, canonicalName, inRange, scalarTypeFromName } from "./types.ts";
import {
  type ThreeValued,
  type Value,
  eq3,
  gt3,
  intValue,
  isTrue,
  lt3,
  nullValue,
} from "./value.ts";

// Outcome is the result of executing one statement: a bare statement (CREATE, INSERT,
// UPDATE, DELETE) or a query result set.
export type Outcome =
  | { kind: "statement" }
  | { kind: "query"; columnCount: number; rows: Value[][] };

// Database is the whole database: catalog + per-table in-memory stores. Single
// committed state (CLAUDE.md §3); the staging-buffer commit model lands later. Names
// are keyed case-insensitively (lowercased).
export class Database {
  readonly tables: Map<string, Table>;
  readonly stores: Map<string, TableStore>;

  constructor() {
    this.tables = new Map();
    this.stores = new Map();
  }

  // table looks up a table definition by name (case-insensitive).
  table(name: string): Table | undefined {
    return this.tables.get(name.toLowerCase());
  }

  // putTable registers a new table and its empty store.
  putTable(t: Table): void {
    const key = t.name.toLowerCase();
    this.stores.set(key, new TableStore());
    this.tables.set(key, t);
  }

  // executeStmt executes one parsed statement.
  executeStmt(stmt: Statement): Outcome {
    switch (stmt.kind) {
      case "createTable":
        return this.executeCreateTable(stmt);
      case "insert":
        return this.executeInsert(stmt);
      case "select":
        return this.executeSelect(stmt);
      case "update":
        return this.executeUpdate(stmt);
      case "delete":
        return this.executeDelete(stmt);
    }
  }

  // rowsInKeyOrder returns a table's rows in primary-key (encoded byte) order, or [] if
  // the table does not exist.
  rowsInKeyOrder(name: string): Row[] {
    const store = this.stores.get(name.toLowerCase());
    return store ? store.iterInKeyOrder() : [];
  }

  // executeCreateTable resolves each column's type name, enforces a single primary key
  // (implicitly NOT NULL), rejects duplicate table and column names, then registers it.
  private executeCreateTable(ct: CreateTable): Outcome {
    if (this.table(ct.name)) {
      throw engineError("duplicate_table", "table already exists: " + ct.name);
    }

    const columns: Column[] = [];
    let pkSeen = false;
    for (const def of ct.columns) {
      for (const c of columns) {
        if (c.name.toLowerCase() === def.name.toLowerCase()) {
          throw engineError("duplicate_column", "duplicate column name: " + def.name);
        }
      }
      const ty = scalarTypeFromName(def.typeName);
      if (ty === undefined) {
        throw engineError("undefined_object", "type does not exist: " + def.typeName);
      }
      if (def.primaryKey) {
        if (pkSeen) {
          throw engineError(
            "invalid_table_definition",
            "a table may have at most one primary key",
          );
        }
        pkSeen = true;
      }
      columns.push({
        name: def.name,
        type: ty,
        primaryKey: def.primaryKey,
        notNull: def.primaryKey, // PRIMARY KEY ⇒ NOT NULL
      });
    }

    this.putTable({ name: ct.name, columns });
    return { kind: "statement" };
  }

  // executeInsert maps the literal values positionally to columns, type-checks each
  // (NULL into NOT NULL traps 23502; an integer outside the column type's range traps
  // 22003 — CLAUDE.md §8), then stores the row keyed by its encoded primary key
  // (duplicate key traps 23505) or a monotonic synthetic rowid.
  private executeInsert(ins: Insert): Outcome {
    const table = this.table(ins.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ins.table);
    }
    if (ins.values.length !== table.columns.length) {
      throw engineError(
        "syntax_error",
        `INSERT has ${ins.values.length} values but table ${table.name} has ${table.columns.length} columns`,
      );
    }

    const row: Row = new Array(table.columns.length);
    for (let i = 0; i < table.columns.length; i++) {
      const col = table.columns[i]!;
      const lit = ins.values[i]!;
      if (lit.kind === "null") {
        if (col.notNull) {
          throw engineError(
            "not_null_violation",
            "null value in column " + col.name + " violates not-null constraint",
          );
        }
        row[i] = nullValue();
      } else {
        if (!inRange(col.type, lit.int)) {
          throw engineError(
            "numeric_value_out_of_range",
            "value out of range for type " + canonicalName(col.type),
          );
        }
        row[i] = intValue(lit.int);
      }
    }

    // The storage key is the encoded primary key, or — for a table without one — a
    // monotonic synthetic rowid: never reused, so DELETE then INSERT cannot collide
    // with a freed key (spec/fileformat/format.md).
    const store = this.stores.get(ins.table.toLowerCase())!;
    const pk = primaryKeyIndex(table);
    let key: Uint8Array;
    if (pk >= 0) {
      const pkv = row[pk]!; // non-null: a PK column is NOT NULL and was checked above
      key = encodeInt(table.columns[pk]!.type, pkv.kind === "int" ? pkv.int : 0n);
    } else {
      key = encodeInt("int64", store.allocRowid());
    }

    if (!store.insert(key, row)) {
      throw engineError(
        "unique_violation",
        "duplicate key value violates primary key uniqueness",
      );
    }
    return { kind: "statement" };
  }

  // executeDelete resolves the table and optional predicate, collects the keys of
  // matching rows (only a TRUE predicate matches — Kleene), then removes them. No WHERE
  // deletes every row. Keys are collected before mutating.
  private executeDelete(del: Delete): Outcome {
    const table = this.table(del.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + del.table);
    }
    const filter = del.filter ? resolvePredicate(table, del.filter) : null;

    const store = this.stores.get(del.table.toLowerCase())!;
    const keys: Uint8Array[] = [];
    for (const e of store.entriesInKeyOrder()) {
      if (filter === null || isTrue(evalPredicate(filter, e.row))) {
        keys.push(e.key);
      }
    }
    for (const k of keys) store.remove(k);
    return { kind: "statement" };
  }

  // executeUpdate is two-phase / all-or-nothing: phase 1 builds and type-checks every
  // matching row's new values (assignments evaluate against the OLD row, so
  // `SET a = b, b = a` swaps); a 22003/23502 aborts with no writes. Phase 2 applies.
  // Assigning a PRIMARY KEY column traps 0A000 (the storage key must not change this
  // slice); a duplicate target column traps 42701. No WHERE updates every row.
  private executeUpdate(upd: Update): Outcome {
    const table = this.table(upd.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + upd.table);
    }

    // Resolve assignments up front (fail fast, deterministic).
    const pkIdx = primaryKeyIndex(table);
    const plans: AssignPlan[] = [];
    for (const a of upd.assignments) {
      const idx = columnIndex(table, a.column);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + a.column);
      }
      if (idx === pkIdx) {
        throw engineError(
          "feature_not_supported",
          "updating a primary key column is not supported",
        );
      }
      for (const p of plans) {
        if (p.idx === idx) {
          throw engineError(
            "duplicate_column",
            "column " + a.column + " assigned more than once",
          );
        }
      }
      const col = table.columns[idx]!;
      let source: AssignSource;
      if (a.value.kind === "literal") {
        source = {
          kind: "const",
          value: a.value.literal.kind === "int" ? intValue(a.value.literal.int) : nullValue(),
        };
      } else {
        const j = columnIndex(table, a.value.name);
        if (j < 0) {
          throw engineError("undefined_column", "column does not exist: " + a.value.name);
        }
        source = { kind: "column", index: j };
      }
      plans.push({ idx, name: col.name, target: col.type, notNull: col.notNull, source });
    }

    const filter = upd.filter ? resolvePredicate(table, upd.filter) : null;

    // Phase 1: build + validate every matching row's new values; no writes yet.
    const store = this.stores.get(upd.table.toLowerCase())!;
    const updates: { key: Uint8Array; row: Row }[] = [];
    for (const e of store.entriesInKeyOrder()) {
      if (filter !== null && !isTrue(evalPredicate(filter, e.row))) continue;
      const newRow = e.row.slice();
      for (const p of plans) {
        const raw = p.source.kind === "column" ? e.row[p.source.index]! : p.source.value;
        newRow[p.idx] = checkAssign(p, raw);
      }
      updates.push({ key: e.key, row: newRow });
    }

    // Phase 2: apply (keys unchanged — a PK column can't be assigned).
    for (const u of updates) store.replace(u.key, u.row);
    return { kind: "statement" };
  }

  // executeSelect resolves projected columns and the WHERE/ORDER BY columns against the
  // catalog, scans in primary-key order, filters (three-valued — only TRUE keeps a
  // row), optionally re-sorts by ORDER BY, then projects.
  private executeSelect(sel: Select): Outcome {
    const table = this.table(sel.from);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + sel.from);
    }

    const projections = resolveProjections(table, sel.items);
    const filter = sel.filter ? resolvePredicate(table, sel.filter) : null;

    let orderIdx = -1;
    let orderDesc = false;
    if (sel.orderBy) {
      orderIdx = columnIndex(table, sel.orderBy.column);
      if (orderIdx < 0) {
        throw engineError("undefined_column", "column does not exist: " + sel.orderBy.column);
      }
      orderDesc = sel.orderBy.descending;
    }

    // Scan in primary-key order, then filter.
    const rows: Row[] = [];
    for (const row of this.rowsInKeyOrder(sel.from)) {
      if (filter === null || isTrue(evalPredicate(filter, row))) rows.push(row);
    }

    // ORDER BY: stable sort by the key column's value, NULLs first ascending
    // (spec/design/encoding.md §4); descending reverses, NULLs last. JS Array#sort is
    // stable, matching Go's SliceStable.
    if (orderIdx >= 0) {
      const oi = orderIdx;
      rows.sort((a, b) => {
        const c = nullFirstCmp(a[oi]!, b[oi]!);
        return orderDesc ? -c : c;
      });
    }

    // Project each surviving row.
    const out: Value[][] = rows.map((row) => projections.map((p) => evalProjection(p, row)));
    return { kind: "query", columnCount: projections.length, rows: out };
  }
}

// --- resolution + evaluation (projection-independent; shared by the statements) ---

// Projection is a resolved output column: how to produce one value from a row.
type Projection =
  | { kind: "column"; index: number }
  | { kind: "literalInt"; value: bigint }
  | { kind: "null" }
  | { kind: "cast"; target: ScalarType; inner: Projection };

function resolveProjections(table: Table, items: SelectItems): Projection[] {
  if (items.kind === "all") {
    return table.columns.map((_c, i): Projection => ({ kind: "column", index: i }));
  }
  return items.items.map((e) => resolveExpr(table, e));
}

function resolveExpr(table: Table, e: SelectExpr): Projection {
  switch (e.kind) {
    case "cast": {
      const target = scalarTypeFromName(e.typeName);
      if (target === undefined) {
        throw engineError("undefined_object", "type does not exist: " + e.typeName);
      }
      return { kind: "cast", target, inner: resolveExpr(table, e.inner) };
    }
    case "literal":
      if (e.literal.kind === "null") return { kind: "null" };
      return { kind: "literalInt", value: e.literal.int };
    case "column": {
      const idx = columnIndex(table, e.name);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + e.name);
      }
      return { kind: "column", index: idx };
    }
  }
}

function evalProjection(p: Projection, row: Row): Value {
  switch (p.kind) {
    case "column":
      return row[p.index]!;
    case "literalInt":
      return intValue(p.value);
    case "null":
      return nullValue();
    case "cast": {
      const v = evalProjection(p.inner, row);
      if (v.kind === "null") return nullValue();
      if (!inRange(p.target, v.int)) {
        throw engineError(
          "numeric_value_out_of_range",
          "value out of range for type " + canonicalName(p.target),
        );
      }
      return intValue(v.int);
    }
  }
}

// Rhs is a comparison's right-hand side after resolution.
type Rhs = { kind: "const"; value: Value } | { kind: "column"; index: number };

// ResolvedPredicate is a WHERE predicate over fixed column indices.
type ResolvedPredicate =
  | { kind: "isNull"; index: number; negated: boolean }
  | { kind: "compare"; index: number; op: CompareOp; rhs: Rhs };

function resolvePredicate(table: Table, p: Predicate): ResolvedPredicate {
  const resolveCol = (name: string): number => {
    const idx = columnIndex(table, name);
    if (idx < 0) {
      throw engineError("undefined_column", "column does not exist: " + name);
    }
    return idx;
  };
  if (p.kind === "compare") {
    const index = resolveCol(p.column);
    let rhs: Rhs;
    if (p.rhs.kind === "literal") {
      const lit = p.rhs.literal;
      if (lit.kind === "int") {
        // Context-adaptive literal (spec/design/types.md): the literal adapts to the
        // compared column's type; a value that does not fit traps 22003 here, before any
        // row is scanned (deterministic).
        const ty = table.columns[index]!.type;
        if (!inRange(ty, lit.int)) {
          throw engineError(
            "numeric_value_out_of_range",
            "value out of range for type " + canonicalName(ty),
          );
        }
        rhs = { kind: "const", value: intValue(lit.int) };
      } else {
        rhs = { kind: "const", value: nullValue() };
      }
    } else {
      rhs = { kind: "column", index: resolveCol(p.rhs.name) };
    }
    return { kind: "compare", index, op: p.op, rhs };
  }
  return { kind: "isNull", index: resolveCol(p.column), negated: p.negated };
}

// evalPredicate returns a three-valued result; a WHERE clause keeps a row only on True.
function evalPredicate(p: ResolvedPredicate, row: Row): ThreeValued {
  if (p.kind === "isNull") {
    const got = (row[p.index]!.kind === "null") !== p.negated;
    return got ? "true" : "false";
  }
  const lhs = row[p.index]!;
  const rhs = p.rhs.kind === "const" ? p.rhs.value : row[p.rhs.index]!;
  switch (p.op) {
    case "eq":
      return eq3(lhs, rhs);
    case "lt":
      return lt3(lhs, rhs);
    case "gt":
      return gt3(lhs, rhs);
    case "le":
      return or3(lt3(lhs, rhs), eq3(lhs, rhs));
    case "ge":
      return or3(gt3(lhs, rhs), eq3(lhs, rhs));
  }
}

// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a NULL
// operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
function or3(a: ThreeValued, b: ThreeValued): ThreeValued {
  if (a === "true" || b === "true") return "true";
  if (a === "unknown" || b === "unknown") return "unknown";
  return "false";
}

// nullFirstCmp is a total order for ORDER BY with NULLs first (ascending), matching the
// key encoding's physical order (spec/design/encoding.md §4). Returns <0, 0, >0.
function nullFirstCmp(a: Value, b: Value): number {
  if (a.kind === "null" && b.kind === "null") return 0;
  if (a.kind === "null") return -1;
  if (b.kind === "null") return 1;
  if (a.int < b.int) return -1;
  if (a.int > b.int) return 1;
  return 0;
}

// AssignSource is an UPDATE assignment's value source.
type AssignSource = { kind: "const"; value: Value } | { kind: "column"; index: number };

// AssignPlan is a resolved UPDATE assignment: target column index, its type and
// nullability for re-checking, and the value source.
type AssignPlan = {
  idx: number;
  name: string;
  target: ScalarType;
  notNull: boolean;
  source: AssignSource;
};

// checkAssign type-checks a candidate value against a column: NULL into NOT NULL traps
// 23502; an integer outside the target range traps 22003 — mirrors INSERT's checks.
function checkAssign(p: AssignPlan, v: Value): Value {
  if (v.kind === "null") {
    if (p.notNull) {
      throw engineError(
        "not_null_violation",
        "null value in column " + p.name + " violates not-null constraint",
      );
    }
    return nullValue();
  }
  if (!inRange(p.target, v.int)) {
    throw engineError(
      "numeric_value_out_of_range",
      "value out of range for type " + canonicalName(p.target),
    );
  }
  return intValue(v.int);
}
