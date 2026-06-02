// Statement executor (CLAUDE.md §10). Mirrors executor.go / executor.rs: dispatch a
// parsed statement; analyze (resolve types, predicates, projections against the
// catalog); run. Errors throw EngineError. Results are produced deterministically
// (CLAUDE.md §10): scan in primary-key order, three-valued WHERE (only TRUE keeps a
// row), stable ORDER BY with NULLs first.

import type {
  BinaryOp,
  CreateTable,
  Delete,
  Expr,
  Insert,
  Select,
  SelectItems,
  Statement,
  Update,
} from "./ast.ts";
import { type Column, type Table, columnIndex, primaryKeyIndex } from "./catalog.ts";
import { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { encodeInt } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { type Row, TableStore } from "./storage.ts";
import {
  type ScalarType,
  canonicalName,
  inRange,
  isBooleanTypeName,
  rank,
  scalarTypeFromName,
} from "./types.ts";
import {
  type Value,
  boolAnd,
  boolNot,
  boolOr,
  eq3,
  from3,
  gt3,
  intValue,
  isTrue,
  lt3,
  notDistinctFrom,
  nullValue,
} from "./value.ts";

// Outcome is the result of executing one statement: a bare statement (CREATE, INSERT,
// UPDATE, DELETE) or a query result set. cost is the deterministic execution cost accrued
// while running it (CLAUDE.md §13) — a DML statement accrues its scan + filter cost even
// though it returns no rows. It is a bigint for int64 parity across cores (§8).
export type Outcome =
  | { kind: "statement"; cost: bigint }
  | { kind: "query"; columnNames: string[]; rows: Value[][]; cost: bigint };

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
      const ty = resolveStorableType(def.typeName);
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
    // DDL touches no rows and evaluates no expressions: zero cost.
    return { kind: "statement", cost: 0n };
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
      } else if (lit.kind === "int") {
        if (!inRange(col.type, lit.int)) {
          throw engineError(
            "numeric_value_out_of_range",
            "value out of range for type " + canonicalName(col.type),
          );
        }
        row[i] = intValue(lit.int);
      } else {
        // boolean is expression-only: there are no boolean columns, so a boolean literal
        // can only target an integer column — a type error (42804).
        throw engineError(
          "datatype_mismatch",
          "cannot store a boolean value in integer column " + col.name,
        );
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
    // A single-row INSERT of literal values reads no rows and evaluates no expression
    // tree: zero cost (DEFAULT expressions, when added, will accrue here).
    return { kind: "statement", cost: 0n };
  }

  // executeDelete resolves the table and optional predicate, collects the keys of
  // matching rows (only a TRUE predicate matches — Kleene), then removes them. No WHERE
  // deletes every row. Keys are collected before mutating.
  private executeDelete(del: Delete): Outcome {
    const table = this.table(del.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + del.table);
    }
    const filter = del.filter ? resolveBooleanFilter(table, del.filter) : null;

    // Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13;
    // spec/design/cost.md §3). Keys are collected before mutating.
    const meter = new Meter();
    const store = this.stores.get(del.table.toLowerCase())!;
    const keys: Uint8Array[] = [];
    for (const e of store.entriesInKeyOrder()) {
      // A WHERE arithmetic can throw (22003/22012); the throw propagates naturally.
      meter.charge(COSTS.storageRowRead);
      if (filter === null || isTrue(evalExpr(filter, e.row, meter))) {
        keys.push(e.key);
      }
    }
    for (const k of keys) store.remove(k);
    return { kind: "statement", cost: meter.accrued };
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
      // The RHS is a general expression evaluated against the OLD row; a literal operand
      // adapts to the target column's type. The result must be an integer (or NULL) —
      // assigning a boolean to an integer column is a 42804.
      const { node, type } = resolve(table, a.value, col.type);
      requireAssignableInt(type, a.column);
      plans.push({ idx, name: col.name, target: col.type, notNull: col.notNull, source: node });
    }

    const filter = upd.filter ? resolveBooleanFilter(table, upd.filter) : null;

    // Phase 1: build + validate every matching row's new values; no writes yet. Each
    // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes do
    // not — they evaluate nothing; spec/design/cost.md §3).
    const meter = new Meter();
    const store = this.stores.get(upd.table.toLowerCase())!;
    const updates: { key: Uint8Array; row: Row }[] = [];
    for (const e of store.entriesInKeyOrder()) {
      meter.charge(COSTS.storageRowRead);
      if (filter !== null && !isTrue(evalExpr(filter, e.row, meter))) continue;
      const newRow = e.row.slice();
      for (const p of plans) {
        newRow[p.idx] = checkAssign(p, evalExpr(p.source, e.row, meter));
      }
      updates.push({ key: e.key, row: newRow });
    }

    // Phase 2: apply (keys unchanged — a PK column can't be assigned).
    for (const u of updates) store.replace(u.key, u.row);
    return { kind: "statement", cost: meter.accrued };
  }

  // executeSelect resolves projected columns and the WHERE/ORDER BY columns against the
  // catalog, scans in primary-key order, filters (three-valued — only TRUE keeps a
  // row), optionally re-sorts by ORDER BY, then projects.
  private executeSelect(sel: Select): Outcome {
    const table = this.table(sel.from);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + sel.from);
    }

    const { nodes: projections, names: columnNames } = resolveProjections(table, sel.items);
    const filter = sel.filter ? resolveBooleanFilter(table, sel.filter) : null;

    // Resolve each ORDER BY key to (column index, descending, nullsFirst). An unknown column
    // is the existing undefined_column (42703); nullsFirst was resolved at parse time (explicit
    // clause, else the direction default).
    const order: { idx: number; descending: boolean; nullsFirst: boolean }[] = [];
    for (const key of sel.orderBy) {
      const idx = columnIndex(table, key.column);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + key.column);
      }
      order.push({ idx, descending: key.descending, nullsFirst: key.nullsFirst });
    }

    // Scan in primary-key order, then filter. A WHERE arithmetic can throw
    // (22003/22012); the throw propagates naturally. Each scanned row and the filter
    // evaluation accrue cost; the row-produced charge is below, at projection
    // (CLAUDE.md §13; spec/design/cost.md §3).
    const meter = new Meter();
    const rows: Row[] = [];
    for (const row of this.rowsInKeyOrder(sel.from)) {
      meter.charge(COSTS.storageRowRead);
      if (filter === null || isTrue(evalExpr(filter, row, meter))) rows.push(row);
    }

    // ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
    // and a full tie keeps the primary-key scan order (JS Array#sort is stable, matching Go's
    // SliceStable). Each key's NULL placement is decoupled from its value-direction flip, so an
    // explicit NULLS FIRST|LAST overrides the default (spec/design/grammar.md §10).
    if (order.length > 0) {
      rows.sort((a, b) => {
        for (const key of order) {
          const c = keyCmp(a[key.idx]!, b[key.idx]!, key.descending, key.nullsFirst);
          if (c !== 0) return c;
        }
        return 0;
      });
    }

    // LIMIT / OFFSET: window the sorted rows BEFORE projection, so rows skipped by OFFSET
    // or excluded by LIMIT accrue no rowProduced/projection cost (they were still scanned
    // + filtered above). Clamp in the bigint domain against the row count, then index —
    // never let a huge count cross 2^53 via Number (CLAUDE.md §8; grammar.md §9). The
    // counts are already non-negative (parser).
    const n = BigInt(rows.length);
    const start = sel.offset === null ? 0n : sel.offset < n ? sel.offset : n;
    const end = sel.limit !== null && sel.limit < n - start ? start + sel.limit : n;
    const windowed = rows.slice(Number(start), Number(end));

    // Project each windowed row. Producing a row, and each projection-list evaluation,
    // accrue cost. (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
    const out: Value[][] = windowed.map((row) => {
      meter.charge(COSTS.rowProduced);
      return projections.map((p) => evalExpr(p, row, meter));
    });
    return { kind: "query", columnNames, rows: out, cost: meter.accrued };
  }
}

// ============================================================================
// Resolved expression layer (mirrors impl/rust executor.rs, impl/go executor.go).
//
// Parse → Expr (names) → resolve → RExpr (column indices, known result types, folded
// constants) → eval per row → Value. The resolver is where all type-checking and the
// literal range-check live; the evaluator is a pure tree-walk. eval throws on a 22003 /
// 22012 (the TS idiom), so callers need no Result type.
// ============================================================================

// ResolvedType is the static type of a resolved expression. "null" is an untyped NULL
// literal (its integer type, if needed, is settled by the surrounding operator/context).
type ResolvedType =
  | { kind: "int"; ty: ScalarType }
  | { kind: "bool" }
  | { kind: "null" };

// RExpr is a resolved expression over fixed column indices. Arithmetic/neg nodes carry
// their (promotion-tower) result type so the computed value can be range-checked.
type RExpr =
  | { kind: "column"; index: number }
  | { kind: "constInt"; value: bigint }
  | { kind: "constBool"; value: boolean }
  | { kind: "constNull" }
  | { kind: "cast"; target: ScalarType; operand: RExpr }
  | { kind: "neg"; result: ScalarType; operand: RExpr }
  | { kind: "not"; operand: RExpr }
  | { kind: "arith"; op: BinaryOp; result: ScalarType; lhs: RExpr; rhs: RExpr }
  | { kind: "compare"; op: BinaryOp; lhs: RExpr; rhs: RExpr }
  | { kind: "and"; lhs: RExpr; rhs: RExpr }
  | { kind: "or"; lhs: RExpr; rhs: RExpr }
  | { kind: "isNull"; operand: RExpr; negated: boolean }
  | { kind: "distinct"; lhs: RExpr; rhs: RExpr; negated: boolean };

// resolveProjections resolves SELECT items into evaluable projections (any result type
// is allowed in the select list, including boolean — SELECT a = b), each paired with its
// output column name (spec/design/grammar.md §8).
function resolveProjections(
  table: Table,
  items: SelectItems,
): { nodes: RExpr[]; names: string[] } {
  if (items.kind === "all") {
    return {
      nodes: table.columns.map((_c, i): RExpr => ({ kind: "column", index: i })),
      names: table.columns.map((c) => c.name),
    };
  }
  const nodes: RExpr[] = [];
  const names: string[] = [];
  for (const it of items.items) {
    nodes.push(resolve(table, it.expr, null).node);
    names.push(it.alias ?? outputName(table, it.expr));
  }
  return { nodes, names };
}

// outputName is the output column name of an un-aliased select item
// (spec/design/grammar.md §8): a bare column reference takes the catalog's canonical name
// (the CREATE TABLE spelling, not the SELECT spelling, so the user's casing never leaks);
// every other expression takes the fixed "?column?". The column is known to exist —
// resolve validated it.
function outputName(table: Table, e: Expr): string {
  if (e.kind === "column") {
    const idx = columnIndex(table, e.name);
    if (idx >= 0) return table.columns[idx]!.name;
    return e.name;
  }
  return "?column?";
}

// resolveBooleanFilter resolves a WHERE expression; it must resolve to boolean (or an
// untyped NULL, always unknown → no rows). An integer-valued WHERE is a 42804.
function resolveBooleanFilter(table: Table, e: Expr): RExpr {
  const { node, type } = resolve(table, e, null);
  if (type.kind === "int") {
    throw typeError("argument of WHERE must be boolean");
  }
  return node;
}

// resolve resolves one Expr into an RExpr plus its static type. ctx (non-null) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); null
// defaults a bare literal to int64.
function resolve(
  table: Table,
  e: Expr,
  ctx: ScalarType | null,
): { node: RExpr; type: ResolvedType } {
  switch (e.kind) {
    case "column": {
      const idx = columnIndex(table, e.name);
      if (idx < 0) throw engineError("undefined_column", "column does not exist: " + e.name);
      return { node: { kind: "column", index: idx }, type: { kind: "int", ty: table.columns[idx]!.type } };
    }
    case "literal":
      switch (e.literal.kind) {
        case "null":
          return { node: { kind: "constNull" }, type: { kind: "null" } };
        case "bool":
          return { node: { kind: "constBool", value: e.literal.value }, type: { kind: "bool" } };
        default: {
          const ty = ctx ?? "int64";
          if (!inRange(ty, e.literal.int)) throw overflow(ty);
          return { node: { kind: "constInt", value: e.literal.int }, type: { kind: "int", ty } };
        }
      }
    case "cast": {
      const target = resolveStorableType(e.typeName);
      const inner = resolve(table, e.inner, null);
      if (inner.type.kind === "bool") {
        throw typeError("cannot cast boolean to " + canonicalName(target));
      }
      return { node: { kind: "cast", target, operand: inner.node }, type: { kind: "int", ty: target } };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(table, e.operand, ctx);
        let result: ScalarType;
        if (type.kind === "int") result = type.ty;
        else if (type.kind === "null") result = "int64"; // -NULL = NULL
        else throw typeError("unary minus requires an integer operand");
        return { node: { kind: "neg", result, operand: node }, type: { kind: "int", ty: result } };
      }
      {
        const { node, type } = resolve(table, e.operand, null);
        requireBool(type, "NOT requires a boolean operand");
        return { node: { kind: "not", operand: node }, type: { kind: "bool" } };
      }
    case "isNull": {
      const { node } = resolve(table, e.operand, null);
      return { node: { kind: "isNull", operand: node, negated: e.negated }, type: { kind: "bool" } };
    }
    case "isDistinct": {
      // NULL-safe equality: the SAME integer operand contract as `=` (promote a
      // mixed-width pair, adapt a literal to the sibling's type and range-check it). The
      // result is always a definite boolean (functions.md §3).
      const p = resolveIntPair(table, e.lhs, e.rhs);
      return { node: { kind: "distinct", lhs: p.rl, rhs: p.rr, negated: e.negated }, type: { kind: "bool" } };
    }
    case "binary":
      return resolveBinary(table, e.op, e.lhs, e.rhs);
  }
}

function resolveBinary(
  table: Table,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
): { node: RExpr; type: ResolvedType } {
  switch (op) {
    case "add":
    case "sub":
    case "mul":
    case "div":
    case "mod": {
      const p = resolveIntPair(table, lhs, rhs);
      const result = promote(p.lt, p.rt);
      return { node: { kind: "arith", op, result, lhs: p.rl, rhs: p.rr }, type: { kind: "int", ty: result } };
    }
    case "eq":
    case "lt":
    case "gt":
    case "le":
    case "ge": {
      const p = resolveIntPair(table, lhs, rhs);
      return { node: { kind: "compare", op, lhs: p.rl, rhs: p.rr }, type: { kind: "bool" } };
    }
    default: {
      // "and" | "or"
      const l = resolve(table, lhs, null);
      const r = resolve(table, rhs, null);
      requireBool(l.type, "AND/OR requires boolean operands");
      requireBool(r.type, "AND/OR requires boolean operands");
      return { node: { kind: op === "and" ? "and" : "or", lhs: l.node, rhs: r.node }, type: { kind: "bool" } };
    }
  }
}

// resolveIntPair resolves the two operands of an arithmetic/comparison operator, giving
// a bare integer literal the OTHER operand's type as context (so `small + 1` types `1`
// as int16, and `small + 100000` traps 22003 at resolve). Both must be integer (or
// NULL); a boolean operand is a 42804 type error.
function resolveIntPair(
  table: Table,
  lhs: Expr,
  rhs: Expr,
): { rl: RExpr; lt: ResolvedType; rr: RExpr; rt: ResolvedType } {
  const lhsLit = isIntLiteral(lhs);
  const rhsLit = isIntLiteral(rhs);
  let l: { node: RExpr; type: ResolvedType };
  let r: { node: RExpr; type: ResolvedType };
  if (lhsLit && rhsLit) {
    l = resolve(table, lhs, "int64");
    r = resolve(table, rhs, "int64");
  } else if (lhsLit) {
    r = resolve(table, rhs, null);
    l = resolve(table, lhs, intTypeOf(r.type));
  } else if (rhsLit) {
    l = resolve(table, lhs, null);
    r = resolve(table, rhs, intTypeOf(l.type));
  } else {
    l = resolve(table, lhs, null);
    r = resolve(table, rhs, null);
  }
  requireIntOperand(l.type);
  requireIntOperand(r.type);
  return { rl: l.node, lt: l.type, rr: r.node, rt: r.type };
}

function isIntLiteral(e: Expr): boolean {
  return e.kind === "literal" && e.literal.kind === "int";
}

// intTypeOf returns the integer type of t as a sibling literal's context, or null.
function intTypeOf(t: ResolvedType): ScalarType | null {
  return t.kind === "int" ? t.ty : null;
}

// promote is the promotion-tower result type of two arithmetic operands: the
// higher-ranked integer type, or int64 when both are untyped NULLs.
function promote(a: ResolvedType, b: ResolvedType): ScalarType {
  const ax = intTypeOf(a);
  const bx = intTypeOf(b);
  if (ax !== null && bx !== null) return rank(ax) >= rank(bx) ? ax : bx;
  if (ax !== null) return ax;
  if (bx !== null) return bx;
  return "int64";
}

function requireIntOperand(t: ResolvedType): void {
  if (t.kind === "bool") {
    throw typeError("arithmetic and comparison operators require integer operands");
  }
}

function requireBool(t: ResolvedType, msg: string): void {
  if (t.kind === "int") throw typeError(msg);
}

// requireAssignableInt: a value assigned to an integer column must itself be integer (or
// NULL); a boolean expression is a 42804 type error.
function requireAssignableInt(t: ResolvedType, col: string): void {
  if (t.kind === "bool") {
    throw typeError("cannot assign a boolean value to integer column " + col);
  }
}

// resolveStorableType resolves a column-definition or CAST target type name. Only the
// storable integer types are valid; boolean is known-but-not-storable (→ 0A000),
// distinct from a genuinely unknown name (→ 42704).
function resolveStorableType(name: string): ScalarType {
  const ty = scalarTypeFromName(name);
  if (ty !== undefined) return ty;
  if (isBooleanTypeName(name)) {
    throw engineError("feature_not_supported", "boolean is not a storable type yet: " + name);
  }
  throw engineError("undefined_object", "type does not exist: " + name);
}

function overflow(ty: ScalarType): Error {
  return engineError("numeric_value_out_of_range", "value out of range for type " + canonicalName(ty));
}

function typeError(msg: string): Error {
  return engineError("datatype_mismatch", msg);
}

const I64_MIN = -9223372036854775808n;

// evalExpr evaluates against a row, accruing cost into m, and returns a Value (a boolean
// for comparisons / connectives). Arithmetic throws 22003 on overflow and 22012 on a zero
// divisor; NULL propagates through arithmetic; the connectives are Kleene; IS NULL is
// always definite.
//
// Cost: each INTERIOR node charges operator_eval once, pre-order (the node, then its
// operands LHS-before-RHS — JS evaluates arguments left-to-right); leaf nodes
// (column/constants) charge nothing. Both operands are always evaluated — there is no
// short-circuit, so the count never depends on operand values (spec/design/cost.md §3).
function evalExpr(e: RExpr, row: Row, m: Meter): Value {
  switch (e.kind) {
    case "column":
      return row[e.index]!;
    case "constInt":
      return intValue(e.value);
    case "constBool":
      return { kind: "bool", value: e.value };
    case "constNull":
      return nullValue();
    case "cast": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, m);
      if (v.kind === "null") return nullValue();
      if (v.kind !== "int") throw typeError("internal: boolean cast operand");
      if (!inRange(e.target, v.int)) throw overflow(e.target);
      return intValue(v.int);
    }
    case "neg": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, m);
      if (v.kind === "null") return nullValue();
      if (v.kind !== "int") throw typeError("internal: boolean unary minus");
      const n = -v.int;
      if (!inRange(e.result, n)) throw overflow(e.result);
      return intValue(n);
    }
    case "not": {
      m.charge(COSTS.operatorEval);
      return boolNot(evalExpr(e.operand, row, m));
    }
    case "arith": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, m);
      const b = evalExpr(e.rhs, row, m);
      if (a.kind === "null" || b.kind === "null") return nullValue();
      if (a.kind !== "int" || b.kind !== "int") throw typeError("internal: boolean arithmetic");
      return evalArith(e.op, a.int, b.int, e.result);
    }
    case "compare": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, m);
      const b = evalExpr(e.rhs, row, m);
      switch (e.op) {
        case "eq":
          return from3(eq3(a, b));
        case "lt":
          return from3(lt3(a, b));
        case "gt":
          return from3(gt3(a, b));
        case "le":
          return from3(or3(lt3(a, b), eq3(a, b)));
        default: // "ge"
          return from3(or3(gt3(a, b), eq3(a, b)));
      }
    }
    case "and": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, m);
      const b = evalExpr(e.rhs, row, m);
      return boolAnd(a, b);
    }
    case "or": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, m);
      const b = evalExpr(e.rhs, row, m);
      return boolOr(a, b);
    }
    case "isNull": {
      m.charge(COSTS.operatorEval);
      const isNull = evalExpr(e.operand, row, m).kind === "null";
      return { kind: "bool", value: isNull !== e.negated };
    }
    case "distinct": {
      m.charge(COSTS.operatorEval);
      const same = notDistinctFrom(evalExpr(e.lhs, row, m), evalExpr(e.rhs, row, m));
      // negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
      // the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
      // unknown (the null_safe discipline, functions.md §3).
      return { kind: "bool", value: same === e.negated };
    }
  }
}

// evalArith computes an integer op with exact bigint, throwing 22012 on a zero divisor
// and 22003 if the result falls outside the declared result type (the int16+int16 →
// int16 boundary — spec/design/functions.md §7). The MinInt64/-1 cases trap to match the
// Rust/Go checked-op behaviour (bigint would not overflow on its own).
function evalArith(op: BinaryOp, x: bigint, y: bigint, result: ScalarType): Value {
  let v: bigint;
  switch (op) {
    case "add":
      v = x + y;
      break;
    case "sub":
      v = x - y;
      break;
    case "mul":
      v = x * y;
      break;
    case "div":
      if (y === 0n) throw engineError("division_by_zero", "division by zero");
      if (x === I64_MIN && y === -1n) throw overflow(result);
      v = x / y; // bigint truncates toward zero
      break;
    default: // "mod"
      if (y === 0n) throw engineError("division_by_zero", "division by zero");
      if (x === I64_MIN && y === -1n) throw overflow(result);
      v = x % y; // bigint remainder takes the dividend's sign
      break;
  }
  if (!inRange(result, v)) throw overflow(result);
  return intValue(v);
}

// or3 is three-valued OR (Kleene): used to build <= / >= from < / > and =, so a NULL
// operand yields UNKNOWN rather than a wrong FALSE (CLAUDE.md §4).
function or3(a: "true" | "false" | "unknown", b: "true" | "false" | "unknown"): "true" | "false" | "unknown" {
  if (a === "true" || b === "true") return "true";
  if (a === "unknown" || b === "unknown") return "unknown";
  return "false";
}

// keyCmp is one ORDER BY key's total-order comparison, returning <0, 0, >0. NULL placement
// is governed by nullsFirst and applied INDEPENDENTLY of the value-direction flip
// (descending), so an explicit NULLS FIRST|LAST overrides the direction default
// (spec/design/grammar.md §10). The physical key order ratifies NULL as the smallest value,
// which surfaces as the parse-time default nullsFirst = !descending.
function keyCmp(a: Value, b: Value, descending: boolean, nullsFirst: boolean): number {
  if (a.kind === "null" && b.kind === "null") return 0;
  if (a.kind === "null") return nullsFirst ? -1 : 1;
  if (b.kind === "null") return nullsFirst ? 1 : -1;
  const base = valueCmp(a, b);
  return descending ? -base : base;
}

// valueCmp is the total order over NON-NULL values: signed-integer ascending, with the
// boolean ordering (false < true) defined only for totality — ORDER BY is over an integer
// column this slice. NULLs are handled by keyCmp before this is reached. Returns <0, 0, >0.
function valueCmp(a: Value, b: Value): number {
  const x = orderKey(a);
  const y = orderKey(b);
  if (x < y) return -1;
  if (x > y) return 1;
  return 0;
}

function orderKey(v: Value): bigint {
  if (v.kind === "bool") return v.value ? 1n : 0n;
  if (v.kind === "int") return v.int;
  return 0n;
}

// AssignPlan is a resolved UPDATE assignment: target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
type AssignPlan = {
  idx: number;
  name: string;
  target: ScalarType;
  notNull: boolean;
  source: RExpr;
};

// checkAssign type-checks a candidate value against a column: NULL into NOT NULL traps
// 23502; an integer outside the target range traps 22003 — mirrors INSERT's checks. The
// resolver proved the value is integer or NULL, never boolean.
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
  if (v.kind !== "int") throw typeError("internal: boolean assigned to integer column");
  if (!inRange(p.target, v.int)) throw overflow(p.target);
  return intValue(v.int);
}
