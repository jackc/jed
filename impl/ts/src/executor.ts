// Statement executor (CLAUDE.md §10). Mirrors executor.go / executor.rs: dispatch a
// parsed statement; analyze (resolve types, predicates, projections against the
// catalog); run. Errors throw EngineError. Results are produced deterministically
// (CLAUDE.md §10): scan in primary-key order, three-valued WHERE (only TRUE keeps a
// row), stable ORDER BY with NULLs last (the PostgreSQL model).

import type {
  BinaryOp,
  CreateTable,
  Delete,
  DropTable,
  Expr,
  Insert,
  Select,
  SelectItems,
  Statement,
  TypeMod,
  Update,
} from "./ast.ts";
import { type Column, type Table, columnIndex, primaryKeyIndex } from "./catalog.ts";
import { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { Decimal, MAX_PRECISION, MAX_SCALE } from "./decimal.ts";
import { encodeInt } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { type Row, TableStore } from "./storage.ts";
import {
  type DecimalTypmod,
  type ScalarType,
  canonicalName,
  inRange,
  isBool,
  isDecimal,
  isInteger,
  isText,
  rank,
  scalarTypeFromName,
} from "./types.ts";
import {
  type Value,
  boolAnd,
  boolNot,
  boolOr,
  boolValue,
  compareTextC,
  decimalValue,
  eq3,
  from3,
  gt3,
  intValue,
  isTrue,
  lt3,
  notDistinctFrom,
  nullValue,
  textValue,
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
      case "dropTable":
        return this.executeDropTable(stmt);
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
      const [ty, decimal] = resolveTypeAndTypmod(def.typeName, def.typeMod);
      if (def.primaryKey) {
        // Only integers may be a key this slice. The order-preserving text and decimal key
        // encodings (spec/design/encoding.md §2.4/§2.5) are authored but unexercised, so a
        // text or decimal PRIMARY KEY is a documented 0A000 narrowing (types.md §11/§12).
        if (!isInteger(ty)) {
          throw engineError(
            "feature_not_supported",
            "a " + canonicalName(ty) + " primary key is not supported yet",
          );
        }
        // Likewise boolean: the bool-byte key encoding rule is authored but unexercised, so a
        // boolean PRIMARY KEY is a documented 0A000 narrowing (spec/design/types.md §9).
        if (isBool(ty)) {
          throw engineError("feature_not_supported", "a boolean primary key is not supported yet");
        }
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
        decimal,
        primaryKey: def.primaryKey,
        notNull: def.primaryKey, // PRIMARY KEY ⇒ NOT NULL
      });
    }

    this.putTable({ name: ct.name, columns });
    // DDL touches no rows and evaluates no expressions: zero cost.
    return { kind: "statement", cost: 0n };
  }

  // executeDropTable removes the table's definition and its row store from the catalog
  // (both keyed by the lower-cased name). A table that does not exist is the same 42P01
  // the DML paths raise — there is no IF EXISTS this slice (spec/design/grammar.md §13).
  // Like CREATE TABLE it touches no rows and evaluates no expression tree (the store is
  // discarded wholesale), so it accrues zero cost.
  private executeDropTable(dt: DropTable): Outcome {
    if (!this.table(dt.name)) {
      throw engineError("undefined_table", "table does not exist: " + dt.name);
    }
    const key = dt.name.toLowerCase();
    this.tables.delete(key);
    this.stores.delete(key);
    return { kind: "statement", cost: 0n };
  }

  // executeInsert maps each row's literal values positionally to columns and type-checks
  // them (NULL into NOT NULL traps 23502; an integer outside the column type's range traps
  // 22003 — CLAUDE.md §8); a duplicate primary key traps 23505. A multi-row INSERT is
  // two-phase / all-or-nothing (grammar.md §12), mirroring UPDATE: every row is validated —
  // including its storage key checked against both the stored rows and earlier rows in the
  // same statement — before any row is inserted, so a mid-batch failure stores nothing.
  private executeInsert(ins: Insert): Outcome {
    const table = this.table(ins.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ins.table);
    }
    const store = this.stores.get(ins.table.toLowerCase())!;
    const pk = primaryKeyIndex(table);

    // Phase 1 — validate every row and compute its key. Nothing is stored yet. For a
    // table with a primary key, the encoded key is checked for a duplicate (within the
    // batch via seenKeys, and against the store) up front; for a table with none, key is
    // null and a fresh monotonic rowid is allocated in phase 2.
    const prepared: { key: Uint8Array | null; row: Row }[] = [];
    const seenKeys = new Set<string>();
    for (const lits of ins.rows) {
      if (lits.length !== table.columns.length) {
        throw engineError(
          "syntax_error",
          `INSERT row has ${lits.length} values but table ${table.name} has ${table.columns.length} columns`,
        );
      }

      const row: Row = new Array(table.columns.length);
      for (let i = 0; i < table.columns.length; i++) {
        const col = table.columns[i]!;
        // The literal adapts/coerces to its target column: an integer literal into a decimal
        // column widens (int→decimal, then to the typmod); a decimal literal into a decimal
        // column rounds to its scale; a cross-family pair is 42804 (decimal.md §6, types.md §5).
        row[i] = storeValue(literalToValue(lits[i]!), col.type, col.decimal, col.notNull, col.name);
      }

      let key: Uint8Array | null = null;
      if (pk >= 0) {
        const pkv = row[pk]!; // non-null: a PK column is NOT NULL and was checked above
        key = encodeInt(table.columns[pk]!.type, pkv.kind === "int" ? pkv.int : 0n);
        const seen = key.join(",");
        if (seenKeys.has(seen) || store.get(key) !== undefined) {
          throw engineError(
            "unique_violation",
            "duplicate key value violates primary key uniqueness",
          );
        }
        seenKeys.add(seen);
      }
      prepared.push({ key, row });
    }

    // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
    // rowid is allocated here, in row order, so a failed validation pass burns none
    // (spec/fileformat/format.md, grammar.md §12).
    for (const pr of prepared) {
      const key = pr.key ?? encodeInt("int64", store.allocRowid());
      if (!store.insert(key, pr.row)) {
        throw new Error("pre-validated INSERT key must be unique");
      }
    }
    // INSERT of literal rows reads no rows and evaluates no expression tree: zero cost
    // (DEFAULT expressions, when added, will accrue here).
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
      // adapts to the target column's type. The result must be assignable to the column's
      // family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
      const { node, type } = resolve(table, a.value, col.type);
      requireAssignable(type, col.type, a.column);
      plans.push({ idx, name: col.name, target: col.type, decimal: col.decimal, notNull: col.notNull, source: node });
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

    // SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates are
    // collapsed, an ORDER BY key not in the projected output has no single value per row,
    // so each key must appear as a bare column in the select list (or the list is `*`).
    // Matches PostgreSQL (42P10). Aliases are invisible to ORDER BY (§8), so an aliased
    // bare column still counts as projecting its underlying column.
    if (sel.distinct && order.length > 0 && sel.items.kind === "list") {
      const projected = new Set<number>();
      for (const it of sel.items.items) {
        if (it.expr.kind === "column") {
          const idx = columnIndex(table, it.expr.name);
          if (idx >= 0) projected.add(idx);
        }
      }
      for (const key of order) {
        if (!projected.has(key.idx)) {
          throw engineError(
            "invalid_column_reference",
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
          );
        }
      }
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

    // LIMIT / OFFSET window bounds over a result of `len` rows. Clamp in the bigint domain
    // against the row count, then index — never let a huge count cross 2^53 via Number
    // (CLAUDE.md §8; grammar.md §9). The counts are already non-negative (parser).
    const windowBounds = (len: number): [number, number] => {
      const n = BigInt(len);
      const start = sel.offset === null ? 0n : sel.offset < n ? sel.offset : n;
      const end = sel.limit !== null && sel.limit < n - start ? start + sel.limit : n;
      return [Number(start), Number(end)];
    };

    // Build the output rows. The two paths differ in pipeline order
    // (spec/design/grammar.md §11): without DISTINCT the window slices the sorted source
    // rows and ONLY the windowed rows are projected; with DISTINCT every (sorted) filtered
    // row is projected — dedup must see them all — duplicates drop by first occurrence, and
    // the window then slices the DISTINCT rows.
    let out: Value[][];
    if (sel.distinct) {
      // Project every filtered row (charging projection cost per row, the §3 asymmetry),
      // keeping first occurrences. `seen` is membership-only: output order comes from the
      // deterministic source iteration, never from Set iteration (no order leak — §8/§10).
      const seen = new Set<string>();
      const distinctRows: Value[][] = [];
      for (const row of rows) {
        const tuple = projections.map((p) => evalExpr(p, row, meter));
        const key = distinctRowKey(tuple);
        if (!seen.has(key)) {
          seen.add(key);
          distinctRows.push(tuple);
        }
      }
      // LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge
      // rowProduced (spec/design/cost.md §3).
      const [start, end] = windowBounds(distinctRows.length);
      out = distinctRows.slice(start, end).map((tuple) => {
        meter.charge(COSTS.rowProduced);
        return tuple;
      });
    } else {
      // Window the sorted rows BEFORE projection, so rows skipped by OFFSET or excluded by
      // LIMIT accrue no rowProduced/projection cost (they were still scanned + filtered
      // above). Producing a row, and each projection-list evaluation, accrue cost.
      // (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
      const [start, end] = windowBounds(rows.length);
      out = rows.slice(start, end).map((row) => {
        meter.charge(COSTS.rowProduced);
        return projections.map((p) => evalExpr(p, row, meter));
      });
    }
    return { kind: "query", columnNames, rows: out, cost: meter.accrued };
  }
}

// distinctRowKey encodes a projected row into a collision-free string key for DISTINCT
// dedup. Each field carries a type tag (n/i/b) and a payload, joined by a separator no
// field can contain, so e.g. (1,23) and (12,3) do not collide (spec/design/grammar.md §11).
// NULL == NULL falls out (both encode "n"), matching the NULL-safe DISTINCT rule. Ints use
// bigint.toString() — exact, never the lossy `number` path (CLAUDE.md §8).
function distinctRowKey(row: Value[]): string {
  return row
    .map((v) => {
      switch (v.kind) {
        case "null":
          return "n";
        case "int":
          return "i" + v.int.toString();
        case "bool":
          return v.value ? "b1" : "b0";
        case "text":
          // Length-prefix the content so the separator byte cannot be confused with a text
          // value that contains it (the value is arbitrary UTF-8).
          return "t" + v.text.length.toString() + ":" + v.text;
        case "decimal":
          // Value-canonical key so 1.5 and 1.50 collapse to one DISTINCT bucket (decimal.md §5).
          return "d" + v.dec.canonicalString();
      }
    })
    .join("|");
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
  | { kind: "text" } // the text family (one collation, C); does not promote
  | { kind: "decimal" } // the decimal family (one type; the per-column typmod is separate)
  | { kind: "null" };

// RExpr is a resolved expression over fixed column indices. Arithmetic/neg nodes carry
// their (promotion-tower) result type so the computed value can be range-checked.
type RExpr =
  | { kind: "column"; index: number }
  | { kind: "constInt"; value: bigint }
  | { kind: "constBool"; value: boolean }
  | { kind: "constText"; value: string }
  | { kind: "constDecimal"; value: Decimal }
  | { kind: "constNull" }
  | { kind: "cast"; target: ScalarType; typmod: DecimalTypmod | null; operand: RExpr }
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
// untyped NULL, always unknown → no rows). An integer- or text-valued WHERE is a 42804.
function resolveBooleanFilter(table: Table, e: Expr): RExpr {
  const { node, type } = resolve(table, e, null);
  if (type.kind !== "bool" && type.kind !== "null") {
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
      const colTy = table.columns[idx]!.type;
      const type: ResolvedType = isText(colTy)
        ? { kind: "text" }
        : isBool(colTy)
          ? { kind: "bool" }
          : isDecimal(colTy)
            ? { kind: "decimal" }
            : { kind: "int", ty: colTy };
      return { node: { kind: "column", index: idx }, type };
    }
    case "literal":
      switch (e.literal.kind) {
        case "null":
          return { node: { kind: "constNull" }, type: { kind: "null" } };
        case "bool":
          return { node: { kind: "constBool", value: e.literal.value }, type: { kind: "bool" } };
        case "text":
          // A text literal is always text (collation C); it does not adapt to context.
          return { node: { kind: "constText", value: e.literal.text }, type: { kind: "text" } };
        case "decimal":
          // A decimal literal is always decimal; it does not adapt to context (like text).
          // Cap-check it here (an over-long coefficient/scale traps 22003 at resolve).
          return { node: { kind: "constDecimal", value: e.literal.dec.checkCap() }, type: { kind: "decimal" } };
        default: {
          // An integer literal adapts only to an integer context; a non-integer context (a
          // text/decimal column or assignment target) does not apply — it defaults to int64,
          // and the surrounding check then reports the family mismatch (42804) or widens it
          // (int→decimal), never a wrong range check on a non-integer type.
          const ty = ctx !== null && isInteger(ctx) ? ctx : "int64";
          if (!inRange(ty, e.literal.int)) throw overflow(ty);
          return { node: { kind: "constInt", value: e.literal.int }, type: { kind: "int", ty } };
        }
      }
    case "cast": {
      const [target, typmod] = resolveTypeAndTypmod(e.typeName, e.typeMod);
      // Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11):
      // casting TO text is a 0A000 this slice.
      if (isText(target)) {
        throw engineError("feature_not_supported", "casting to text is not supported yet");
      }
      // Boolean casts are likewise deferred (boolean⇄integer is a later cast slice —
      // spec/types/casts.toml): casting TO boolean is a 0A000 this slice. Without this guard
      // resolveTypeAndTypmod now returns boolean, so it must be caught here.
      if (isBool(target)) {
        throw engineError("feature_not_supported", "casting to boolean is not supported yet");
      }
      const inner = resolve(table, e.inner, null);
      if (inner.type.kind === "bool") {
        throw typeError("cannot cast boolean to " + canonicalName(target));
      }
      // Casting FROM text is likewise deferred (0A000).
      if (inner.type.kind === "text") {
        throw engineError("feature_not_supported", "casting from text is not supported yet");
      }
      // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
      // decimal→decimal (re-scale), and NULL are all castable.
      const resultType: ResolvedType = isDecimal(target) ? { kind: "decimal" } : { kind: "int", ty: target };
      return { node: { kind: "cast", target, typmod, operand: inner.node }, type: resultType };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(table, e.operand, ctx);
        if (type.kind === "decimal") {
          return { node: { kind: "neg", result: "decimal", operand: node }, type: { kind: "decimal" } };
        }
        let result: ScalarType;
        if (type.kind === "int") result = type.ty;
        else if (type.kind === "null") result = "int64"; // -NULL = NULL
        else throw typeError("unary minus requires a numeric operand");
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
      // NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a literal
      // adapts to its sibling; a text literal stays text), then require the operands be
      // comparable (both integer-ish or both text-ish; a mixed pair is 42804). The result
      // is always a definite boolean (functions.md §3).
      const p = resolveOperandPair(table, e.lhs, e.rhs);
      classifyComparable(p.lt, p.rt);
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
      // Arithmetic is overloaded across integer and decimal. Resolve the operand pair (an
      // integer literal adapts to an integer sibling), then pick the family: both integer →
      // integer arithmetic; at least one decimal → decimal arithmetic (the integer operand
      // widens at eval); a text/boolean operand is a 42804 (spec/design/decimal.md §4).
      const p = resolveOperandPair(table, lhs, rhs);
      requireNumericOperand(p.lt);
      requireNumericOperand(p.rt);
      if (p.lt.kind === "decimal" || p.rt.kind === "decimal") {
        return { node: { kind: "arith", op, result: "decimal", lhs: p.rl, rhs: p.rr }, type: { kind: "decimal" } };
      }
      const result = promote(p.lt, p.rt);
      return { node: { kind: "arith", op, result, lhs: p.rl, rhs: p.rr }, type: { kind: "int", ty: result } };
    }
    case "eq":
    case "lt":
    case "gt":
    case "le":
    case "ge": {
      // Comparison is overloaded across families: integer×integer or text×text. Resolve the
      // operands (a literal adapts to its sibling; text literals stay text), then require
      // they be comparable — a mixed integer/text pair is 42804. The runtime comparison
      // (eq3/lt3/gt3) dispatches on the value kinds.
      const p = resolveOperandPair(table, lhs, rhs);
      classifyComparable(p.lt, p.rt);
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

// resolveOperandPair resolves the two operands of a binary operator, giving a bare
// *integer* literal the other operand's integer type as context (so `small + 1` types `1`
// as int16, and `small + 100000` traps 22003 at resolve). A text literal needs no context
// (it is always text); when the sibling is text, an integer literal gets no integer context
// (intTypeOf returns null) and defaults to int64 — the caller's family check then reports
// the mismatch. This does NOT enforce a family — resolveIntPair (arithmetic) and
// classifyComparable (comparison) layer that on top.
function resolveOperandPair(
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
  return { rl: l.node, lt: l.type, rr: r.node, rt: r.type };
}

// resolveIntPair resolves the two operands of an *arithmetic* operator: both must be
// integer (or NULL); a boolean or text operand is a 42804 type error.
// classifyComparable requires that a comparison operand pair is comparable
// (spec/types/compare.toml): both numeric (integer and/or decimal — the integer promotes to
// decimal), both text, or both boolean (NULL counts as either). A mixed numeric/text pair, or
// a boolean with a non-boolean, is a 42804 type error — comparison is overloaded across these
// families but never compares across them.
function classifyComparable(lt: ResolvedType, rt: ResolvedType): void {
  // Boolean compares only with boolean (or NULL); boolean with a number/text is a mismatch.
  const boolL = lt.kind === "bool";
  const boolR = rt.kind === "bool";
  if (boolL !== boolR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a boolean value with a non-boolean value");
  }
  const lNum = lt.kind === "int" || lt.kind === "decimal";
  const rNum = rt.kind === "int" || rt.kind === "decimal";
  if ((lNum && rt.kind === "text") || (lt.kind === "text" && rNum)) {
    throw typeError("cannot compare a text value with a numeric value");
  }
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

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
function requireNumericOperand(t: ResolvedType): void {
  if (t.kind === "bool" || t.kind === "text") {
    throw typeError("arithmetic operators require numeric operands");
  }
}

function requireBool(t: ResolvedType, msg: string): void {
  if (t.kind === "int" || t.kind === "text" || t.kind === "decimal") throw typeError(msg);
}

// requireAssignable: a value assigned to a column must match its family — an integer column
// takes an integer (or NULL); a decimal column takes an integer (int→decimal implicit) or
// decimal (or NULL); a text column takes a text (or NULL); a boolean column takes a boolean
// (or NULL). A decimal value into an integer column is NOT assignable (decimal→int is
// explicit-CAST only). Any cross-family pair is a 42804 type error. Mirrors the INSERT literal
// type-check, generalized to expressions.
function requireAssignable(t: ResolvedType, colTy: ScalarType, col: string): void {
  let ok: boolean;
  if (isInteger(colTy)) ok = t.kind === "int" || t.kind === "null";
  else if (isDecimal(colTy)) ok = t.kind === "int" || t.kind === "decimal" || t.kind === "null";
  else if (isBool(colTy)) ok = t.kind === "bool" || t.kind === "null";
  else ok = t.kind === "text" || t.kind === "null";
  if (!ok) {
    throw typeError("cannot assign a value to column " + col + " of type " + canonicalName(colTy));
  }
}

// resolveTypeAndTypmod resolves a column-definition or CAST target type name + optional type
// modifier. All canonical names and aliases (including boolean/bool and numeric/decimal/dec)
// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
// decimal (validated to numeric(p,s) — 22023); on any other type it is 0A000 (varchar(n) and
// other parameterized types are deferred — spec/design/grammar.md §14). Type-specific narrowings
// (a text/boolean/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the call site.
function resolveTypeAndTypmod(name: string, typeMod: TypeMod | null): [ScalarType, DecimalTypmod | null] {
  const ty = scalarTypeFromName(name);
  if (ty === undefined) {
    throw engineError("undefined_object", "type does not exist: " + name);
  }
  if (typeMod === null) return [ty, null];
  if (!isDecimal(ty)) {
    throw engineError("feature_not_supported", "a type modifier is not supported for type " + canonicalName(ty));
  }
  return [ty, validateDecimalTypmod(typeMod)];
}

// validateDecimalTypmod validates a decimal numeric(p[,s]) type modifier: 1 <= p <= 1000,
// 0 <= s <= p; else trap 22023 (spec/design/decimal.md §2). numeric(p) means scale 0.
function validateDecimalTypmod(tm: TypeMod): DecimalTypmod {
  const p = tm.precision;
  if (p < 1n || p > BigInt(MAX_PRECISION)) {
    throw engineError("invalid_parameter_value", `NUMERIC precision ${p} must be between 1 and ${MAX_PRECISION}`);
  }
  const s = tm.scale ?? 0n;
  if (s > p || s > BigInt(MAX_SCALE)) {
    throw engineError("invalid_parameter_value", `NUMERIC scale ${s} must be between 0 and precision ${p}`);
  }
  return { precision: Number(p), scale: Number(s) };
}

// storeValue coerces a value into a column for storage (shared by INSERT and UPDATE). NULL
// honours NOT NULL (23502); an integer into an integer column is range-checked (22003); an
// integer into a decimal column widens (int→decimal) then coerces to the typmod; a decimal into
// a decimal column coerces to the typmod (rounds, precision-checks → 22003); a boolean into a
// boolean column is accepted as-is; a cross-family value (decimal→int, text→int, etc.) is 42804.
function storeValue(v: Value, colTy: ScalarType, typmod: DecimalTypmod | null, notNull: boolean, colName: string): Value {
  switch (v.kind) {
    case "null":
      if (notNull) {
        throw engineError("not_null_violation", "null value in column " + colName + " violates not-null constraint");
      }
      return nullValue();
    case "int":
      if (isInteger(colTy)) {
        if (!inRange(colTy, v.int)) throw overflow(colTy);
        return intValue(v.int);
      }
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(Decimal.fromBigInt(v.int), typmod));
      throw typeError("cannot store an integer value in " + canonicalName(colTy) + " column " + colName);
    case "decimal":
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(v.dec, typmod));
      throw typeError("cannot store a decimal value in " + canonicalName(colTy) + " column " + colName);
    case "text":
      if (isText(colTy)) return v;
      throw typeError("cannot store a text value in " + canonicalName(colTy) + " column " + colName);
    default: // bool
      if (isBool(colTy)) return v;
      throw typeError("cannot store a boolean value in " + canonicalName(colTy) + " column " + colName);
  }
}

// coerceDecimal coerces a decimal into a column's typmod: round to the declared scale and
// precision-check (22003) for numeric(p,s); for an unconstrained numeric column just cap-check.
function coerceDecimal(d: Decimal, typmod: DecimalTypmod | null): Decimal {
  return typmod !== null ? d.coerceToTypmod(typmod.precision, typmod.scale) : d.checkCap();
}

// literalToValue wraps a parsed literal as a runtime value (type-check/coercion is storeValue).
function literalToValue(lit: Insert["rows"][number][number]): Value {
  switch (lit.kind) {
    case "null":
      return nullValue();
    case "int":
      return intValue(lit.int);
    case "bool":
      return { kind: "bool", value: lit.value };
    case "text":
      return textValue(lit.text);
    default: // decimal
      return decimalValue(lit.dec);
  }
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
    case "constText":
      return textValue(e.value);
    case "constDecimal":
      return decimalValue(e.value);
    case "constNull":
      return nullValue();
    case "cast": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, m);
      if (v.kind === "null") return nullValue();
      return evalCast(v, e.target, e.typmod);
    }
    case "neg": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, m);
      if (v.kind === "null") return nullValue();
      if (isDecimal(e.result)) {
        return decimalValue((v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec).negate());
      }
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
      if (isDecimal(e.result)) {
        // Decimal arithmetic: widen any integer operand to decimal, then apply the op with
        // PG's scale rules (spec/design/decimal.md §4).
        return decimalValue(evalDecimalArith(e.op, toDecimal(a), toDecimal(b)));
      }
      if (a.kind !== "int" || b.kind !== "int") throw typeError("internal: non-integer arithmetic");
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

// evalCast evaluates a (non-NULL) CAST to target. int→int range-checks (22003); int→decimal
// widens then coerces to the typmod; decimal→int rounds half-away to scale 0 then range-checks
// (22003); decimal→decimal re-scales to the typmod (spec/design/decimal.md §6).
function evalCast(v: Value, target: ScalarType, typmod: DecimalTypmod | null): Value {
  if (v.kind === "int") {
    if (isDecimal(target)) return decimalValue(coerceDecimal(Decimal.fromBigInt(v.int), typmod));
    if (!inRange(target, v.int)) throw overflow(target);
    return intValue(v.int);
  }
  if (v.kind !== "decimal") throw typeError("internal: non-numeric cast operand");
  if (isDecimal(target)) return decimalValue(coerceDecimal(v.dec, typmod));
  const n = v.dec.toBigIntRound();
  if (n === null || !inRange(target, n)) throw overflow(target);
  return intValue(n);
}

// toDecimal widens a numeric value to Decimal (an integer operand of decimal arithmetic).
function toDecimal(v: Value): Decimal {
  if (v.kind === "decimal") return v.dec;
  if (v.kind === "int") return Decimal.fromBigInt(v.int);
  throw typeError("internal: non-numeric decimal operand");
}

// evalDecimalArith evaluates decimal arithmetic with PG's result-scale rules
// (spec/design/decimal.md §4), throwing 22003 at the cap and 22012 on a zero divisor/modulus.
function evalDecimalArith(op: BinaryOp, a: Decimal, b: Decimal): Decimal {
  switch (op) {
    case "add":
      return a.add(b);
    case "sub":
      return a.sub(b);
    case "mul":
      return a.mul(b);
    case "div":
      return a.div(b);
    default: // "mod"
      return a.rem(b);
  }
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
// (spec/design/grammar.md §10). The physical key order ratifies NULL as the largest value
// (the PostgreSQL model), which surfaces as the parse-time default nullsFirst = descending.
function keyCmp(a: Value, b: Value, descending: boolean, nullsFirst: boolean): number {
  if (a.kind === "null" && b.kind === "null") return 0;
  if (a.kind === "null") return nullsFirst ? -1 : 1;
  if (b.kind === "null") return nullsFirst ? 1 : -1;
  const base = valueCmp(a, b);
  return descending ? -base : base;
}

// valueCmp is the total order over NON-NULL values: signed-integer ascending, text by
// the C collation — UTF-8 byte / code-point order (compareTextC, NOT JS `<` — the §8 trap;
// spec/design/types.md §11) — and boolean by value, false < true (orderKey maps false→0,
// true→1; types.md §9). The cross-family arms are defined only for totality — ORDER BY is
// over a single typed column, so a mixed pair is unreachable from SELECT. NULLs are handled
// by keyCmp before this is reached. Returns <0, 0, >0.
function valueCmp(a: Value, b: Value): number {
  if (a.kind === "int" && b.kind === "int") return a.int < b.int ? -1 : a.int > b.int ? 1 : 0;
  if (a.kind === "decimal" && b.kind === "decimal") return a.dec.cmpValue(b.dec);
  if (a.kind === "text" && b.kind === "text") return compareTextC(a.text, b.text);
  if (a.kind === "bool" && b.kind === "bool") {
    return a.value === b.value ? 0 : a.value ? 1 : -1;
  }
  // Cross-family arms exist only for totality — ORDER BY is over a single typed column, so a
  // mixed pair is unreachable. A fixed family order keeps the comparator total.
  const fr = familyRank(a) - familyRank(b);
  return fr < 0 ? -1 : fr > 0 ? 1 : 0;
}

// familyRank is a fixed total order across value families, for the unreachable cross-family
// case of valueCmp (ORDER BY is single-column-typed).
function familyRank(v: Value): number {
  switch (v.kind) {
    case "null":
      return 0;
    case "bool":
      return 1;
    case "int":
      return 2;
    case "decimal":
      return 3;
    default: // text
      return 4;
  }
}

// AssignPlan is a resolved UPDATE assignment: target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
type AssignPlan = {
  idx: number;
  name: string;
  target: ScalarType;
  decimal: DecimalTypmod | null;
  notNull: boolean;
  source: RExpr;
};

// checkAssign type-checks + coerces a candidate value against a column — the same storeValue
// path INSERT uses (NULL into NOT NULL → 23502; an integer out of range → 22003; a decimal
// rounds to scale; a boolean into a boolean column is accepted as-is). The resolver proved the
// value's family is assignable.
function checkAssign(p: AssignPlan, v: Value): Value {
  return storeValue(v, p.target, p.decimal, p.notNull, p.name);
}
