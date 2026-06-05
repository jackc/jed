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
  Literal,
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
import { type EngineError, engineError } from "./errors.ts";
import { type Row, TableStore } from "./storage.ts";
import {
  type DecimalTypmod,
  type ScalarType,
  canonicalName,
  inRange,
  isBool,
  isBytea,
  isDecimal,
  isInteger,
  isText,
  isUuid,
  rank,
  scalarTypeFromName,
} from "./types.ts";
import {
  type Value,
  boolAnd,
  boolNot,
  boolOr,
  boolValue,
  byteaValue,
  compareBytea,
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
  parseByteaHex,
  parseUuid,
  renderByteaHex,
  renderUuid,
  textValue,
  uuidValue,
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
// DEFAULT_PAGE_SIZE is the default serialization page size (8 KiB — spec/design/storage.md §3),
// used for a fresh in-memory or newly-created database when no explicit size is given.
export const DEFAULT_PAGE_SIZE = 8192;

export class Database {
  readonly tables: Map<string, Table>;
  readonly stores: Map<string, TableStore>;
  // Persistence identity (spec/design/api.md §2): the backing file path (null for in-memory),
  // the monotonic commit counter, and the page size this database serializes with. Set by the
  // host API open/create; commit writes here.
  path: string | null;
  txid: bigint;
  pageSize: number;

  constructor() {
    this.tables = new Map();
    this.stores = new Map();
    this.path = null;
    this.txid = 0n;
    this.pageSize = DEFAULT_PAGE_SIZE;
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

  // executeStmt executes one parsed statement with no bind parameters.
  executeStmt(stmt: Statement): Outcome {
    return this.executeStmtParams(stmt, []);
  }

  // executeStmtParams executes one parsed statement, binding params to its $N placeholders (an
  // empty array for an unparameterized statement). DDL statements take no parameters — supplying
  // any is a 42601 (spec/design/api.md §5).
  executeStmtParams(stmt: Statement, params: Value[]): Outcome {
    switch (stmt.kind) {
      case "createTable":
        rejectParamsForDDL(params);
        return this.executeCreateTable(stmt);
      case "dropTable":
        rejectParamsForDDL(params);
        return this.executeDropTable(stmt);
      case "insert":
        return this.executeInsert(stmt, params);
      case "select":
        return this.executeSelect(stmt, params);
      case "update":
        return this.executeUpdate(stmt, params);
      case "delete":
        return this.executeDelete(stmt, params);
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
        // Integers and uuid may be a key. uuid is the FIRST non-integer key type — its fixed
        // uuid-raw16 encoding (spec/design/encoding.md §2.7) is exercised. The other non-integer
        // types' order-preserving key encodings (text §2.4, decimal §2.5, bytea §2.6, boolean's
        // bool-byte) are authored but unexercised, so a text/decimal/bytea/boolean PRIMARY KEY is
        // a documented 0A000 narrowing (types.md §9/§11/§12/§13), relaxable in a later in-key slice.
        if (!isInteger(ty) && !isUuid(ty)) {
          throw engineError(
            "feature_not_supported",
            "a " + canonicalName(ty) + " primary key is not supported yet",
          );
        }
        if (pkSeen) {
          throw engineError(
            "invalid_table_definition",
            "a table may have at most one primary key",
          );
        }
        pkSeen = true;
      }
      // Evaluate + type-coerce the DEFAULT literal once, here. A bad default fails at CREATE
      // TABLE: out of range 22003, cross-family 42804, decimal over-precision 22003. NOT NULL
      // is NOT enforced here (notNull=false), so a DEFAULT NULL on a NOT NULL column is accepted
      // and traps 23502 only when applied (constraints.md §2).
      const def_default =
        def.default === null
          ? null
          : storeValue(literalToValue(def.default), ty, decimal, false, def.name);
      columns.push({
        name: def.name,
        type: ty,
        decimal,
        primaryKey: def.primaryKey,
        notNull: def.primaryKey || def.notNull, // PRIMARY KEY ⇒ NOT NULL
        default: def_default,
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

  // executeInsert runs an INSERT of one or more rows. An optional column list names the target
  // columns (unknown → 42703, duplicate → 42701); an unlisted column, or a DEFAULT keyword slot,
  // takes the column's stored default else NULL. Each value is type-checked (NULL into NOT NULL
  // traps 23502; an integer outside the column type's range traps 22003 — CLAUDE.md §8); a
  // duplicate primary key traps 23505. A multi-row INSERT is two-phase / all-or-nothing
  // (grammar.md §12, constraints.md §2), mirroring UPDATE: every row is validated — including its
  // storage key — before any row is inserted, so a mid-batch failure stores nothing.
  private executeInsert(ins: Insert, params: Value[]): Outcome {
    const table = this.table(ins.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ins.table);
    }
    const store = this.stores.get(ins.table.toLowerCase())!;
    const pk = primaryKeyIndex(table);

    // Resolve the optional column list once. provided[i] >= 0 means table column i takes that
    // value position in each row; -1 means column i is omitted (its default, else NULL). With no
    // list it is the identity over all columns. arity is how many values each row must carry.
    const n = table.columns.length;
    const provided: number[] = new Array(n);
    let arity = n;
    if (ins.columns !== null) {
      provided.fill(-1);
      for (let p = 0; p < ins.columns.length; p++) {
        const name = ins.columns[p]!;
        const idx = columnIndex(table, name);
        if (idx < 0) {
          throw engineError(
            "undefined_column",
            `column ${name} of relation ${table.name} does not exist`,
          );
        }
        if (provided[idx]! >= 0) {
          throw engineError(
            "duplicate_column",
            "column " + table.columns[idx]!.name + " specified more than once",
          );
        }
        provided[idx] = p;
      }
      arity = ins.columns.length;
    } else {
      for (let i = 0; i < n; i++) provided[i] = i;
    }

    // A $N in a VALUES slot is typed as its TARGET COLUMN's type. Collect those types across
    // every row (a $N reused under two columns unifies; spec/design/api.md §5), then bind the
    // supplied values up front so a bad bind fails before any row is stored.
    const ptypes = new ParamTypes();
    for (const values of ins.rows) {
      for (let i = 0; i < n; i++) {
        const p = provided[i]!;
        if (p >= 0 && p < values.length) {
          const iv = values[p]!;
          if (iv.kind === "param") ptypes.note(iv.index - 1, table.columns[i]!.type);
        }
      }
    }
    const bound = bindParams(params, ptypes.finalize());

    // Phase 1 — validate every row and compute its key. Nothing is stored yet. For a
    // table with a primary key, the encoded key is checked for a duplicate (within the
    // batch via seenKeys, and against the store) up front; for a table with none, key is
    // null and a fresh monotonic rowid is allocated in phase 2.
    const prepared: { key: Uint8Array | null; row: Row }[] = [];
    const seenKeys = new Set<string>();
    for (const values of ins.rows) {
      if (values.length !== arity) {
        const which = ins.columns !== null ? "target columns are" : "columns are";
        throw engineError(
          "syntax_error",
          `INSERT row has ${values.length} values but ${arity} ${which} expected for table ${table.name}`,
        );
      }

      // Build the row in declaration order: each column takes its provided value (a literal, or
      // a DEFAULT keyword → the column default else NULL), or — when the column is omitted — its
      // default else NULL. storeValue then type-coerces and enforces NOT NULL (23502) uniformly,
      // so a NULL into a NOT NULL column traps here, before key encoding.
      const row: Row = new Array(n);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        let candidate: Value;
        if (p >= 0) {
          const iv = values[p]!;
          if (iv.kind === "default") candidate = col.default ?? nullValue();
          // A bound $N value; its target-column coercion is the storeValue below, identical to a
          // literal in this slot (spec/design/api.md §5).
          else if (iv.kind === "param") candidate = bound[iv.index - 1]!;
          else candidate = literalToValue(iv.lit);
        } else {
          candidate = col.default ?? nullValue();
        }
        row[i] = storeValue(candidate, col.type, col.decimal, col.notNull, col.name);
      }

      let key: Uint8Array | null = null;
      if (pk >= 0) {
        const pkv = row[pk]!; // non-null: a PK column is NOT NULL and was checked above
        if (pkv.kind === "uuid") {
          // uuid is the first non-integer key: its key is the bare 16 bytes (uuid-raw16,
          // encoding.md §2.7) — a PK is NOT NULL, so no presence tag, no sign-flip.
          key = pkv.bytes.slice();
        } else if (pkv.kind === "int") {
          key = encodeInt(table.columns[pk]!.type, pkv.int);
        } else {
          throw engineError("data_corrupted", "a primary key must be an integer or uuid value");
        }
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
    // INSERT reads no rows and evaluates no expression tree — its values are literals and
    // pre-evaluated constant defaults (folded at CREATE TABLE), i.e. leaves: zero cost.
    return { kind: "statement", cost: 0n };
  }

  // executeDelete resolves the table and optional predicate, collects the keys of
  // matching rows (only a TRUE predicate matches — Kleene), then removes them. No WHERE
  // deletes every row. Keys are collected before mutating.
  private executeDelete(del: Delete, params: Value[]): Outcome {
    const table = this.table(del.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + del.table);
    }
    // DELETE is single-table; resolve its WHERE against a one-relation scope.
    const scope = Scope.single(table);
    const ptypes = new ParamTypes();
    const filter = del.filter ? resolveBooleanFilter(scope, del.filter, ptypes) : null;
    const bound = bindParams(params, ptypes.finalize());

    // Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13;
    // spec/design/cost.md §3). Keys are collected before mutating.
    const meter = new Meter();
    const store = this.stores.get(del.table.toLowerCase())!;
    const keys: Uint8Array[] = [];
    for (const e of store.entriesInKeyOrder()) {
      // A WHERE arithmetic can throw (22003/22012); the throw propagates naturally.
      meter.charge(COSTS.storageRowRead);
      if (filter === null || isTrue(evalExpr(filter, e.row, bound, meter))) {
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
  private executeUpdate(upd: Update, params: Value[]): Outcome {
    const table = this.table(upd.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + upd.table);
    }

    // UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
    // shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
    const scope = Scope.single(table);
    const ptypes = new ParamTypes();

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
      const { node, type } = resolve(scope, a.value, col.type, { collecting: false, groupKeys: [], specs: [] }, ptypes);
      requireAssignable(type, col.type, a.column);
      plans.push({ idx, name: col.name, target: col.type, decimal: col.decimal, notNull: col.notNull, source: node });
    }

    const filter = upd.filter ? resolveBooleanFilter(scope, upd.filter, ptypes) : null;
    // All assignment RHSs + the WHERE are resolved: finalize + bind before any scan.
    const bound = bindParams(params, ptypes.finalize());

    // Phase 1: build + validate every matching row's new values; no writes yet. Each
    // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes do
    // not — they evaluate nothing; spec/design/cost.md §3).
    const meter = new Meter();
    const store = this.stores.get(upd.table.toLowerCase())!;
    const updates: { key: Uint8Array; row: Row }[] = [];
    for (const e of store.entriesInKeyOrder()) {
      meter.charge(COSTS.storageRowRead);
      if (filter !== null && !isTrue(evalExpr(filter, e.row, bound, meter))) continue;
      const newRow = e.row.slice();
      for (const p of plans) {
        newRow[p.idx] = checkAssign(p, evalExpr(p.source, e.row, bound, meter));
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
  private executeSelect(sel: Select, params: Value[]): Outcome {
    // Accumulates the inferred type of each $N across every clause of this SELECT, then is
    // finalized + bound once all resolution is done (spec/design/api.md §5).
    const ptypes = new ParamTypes();
    // Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
    // relation's flat column offset in FROM order, and reject a duplicate label — a self-join
    // without distinct aliases is 42712 (spec/design/grammar.md §15).
    const tableRefs = [sel.from, ...sel.joins.map((j) => j.table)];
    const rels: ScopeRel[] = [];
    const seenLabels = new Set<string>();
    let offset = 0;
    for (const tref of tableRefs) {
      const t = this.table(tref.name);
      if (!t) throw engineError("undefined_table", "table does not exist: " + tref.name);
      const label = (tref.alias ?? t.name).toLowerCase();
      if (seenLabels.has(label)) {
        throw engineError("duplicate_alias", "table name " + label + " specified more than once");
      }
      seenLabels.add(label);
      rels.push({ label, table: t, offset });
      offset += t.columns.length;
    }
    const scope = new Scope(rels);

    // Resolve projections (paired with output names — §8), the optional WHERE (must be
    // boolean), and the ORDER BY keys against the full scope. A bare key ambiguous across
    // relations is 42702; an unknown qualifier is 42P01 (§15).
    // Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column — grammar.md
    // §18). An unknown column is 42703, an ambiguous bare key 42702.
    const groupKeys: number[] = sel.groupBy.map((key) =>
      key.kind === "qualifiedColumn"
        ? scope.resolveQualified(key.qualifier, key.name)
        : scope.resolveBare((key as { name: string }).name),
    );

    // An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
    // resolves in collect mode — aggregates collect into synthetic slots and a non-grouped
    // column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
    // mode (columns normal). Output names per grammar.md §8.
    // GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an aggregate
    // query (HAVING alone groups the whole table — grammar.md §19).
    const isAgg = groupKeys.length > 0 || itemsHaveAggregate(sel.items) || sel.having !== null;
    const projAgg: AggCtx = { collecting: isAgg, groupKeys, specs: [] };
    const { nodes: projections, names: columnNames } = resolveProjections(scope, sel.items, projAgg, ptypes);
    // HAVING resolves against the same grouped scope (collect) — it may reference aggregates
    // (collected into the SAME specs, so their slots follow the projection's) and grouping keys;
    // a non-grouped column is 42803. It must be boolean (42804). Resolved after the projection so
    // the synthetic row is [group_keys..., projection aggs..., HAVING aggs...].
    let having: RExpr | null = null;
    if (sel.having !== null) {
      const { node, type } = resolve(scope, sel.having, null, projAgg, ptypes);
      if (type.kind !== "bool" && type.kind !== "null") {
        throw typeError("argument of HAVING must be boolean");
      }
      having = node;
    }
    const aggSpecs = projAgg.specs;
    // SELECT DISTINCT over an aggregate query's output (output-row dedup) is deferred (0A000).
    if (isAgg && sel.distinct) {
      throw engineError("feature_not_supported", "SELECT DISTINCT with aggregates is not supported yet");
    }
    const filter = sel.filter ? resolveBooleanFilter(scope, sel.filter, ptypes) : null;
    // ORDER BY resolution. In an aggregate query a key resolves against the GROUP KEYS — a
    // grouping column gives its synthetic-row slot, a non-grouping column is 42803 (the
    // grouping-error rule, grammar.md §18); the sort runs on the group rows. In a plain query
    // keys resolve against the FROM scope (a flat row index).
    const order: { idx: number; descending: boolean; nullsFirst: boolean }[] = [];
    for (const key of sel.orderBy) {
      const idx = key.qualifier !== null
        ? scope.resolveQualified(key.qualifier, key.column)
        : scope.resolveBare(key.column);
      let slot = idx;
      if (isAgg) {
        slot = groupKeys.indexOf(idx);
        if (slot < 0) throw groupingErrorColumn(key.column);
      }
      order.push({ idx: slot, descending: key.descending, nullsFirst: key.nullsFirst });
    }

    // SELECT DISTINCT restriction (spec/design/grammar.md §11): each ORDER BY key must appear
    // as a bare/qualified column in the select list (resolved to the same flat index; or the
    // list is `*`). Matches PostgreSQL (42P10). Aliases are invisible to ORDER BY (§8).
    if (sel.distinct && order.length > 0 && sel.items.kind === "list") {
      const projected = new Set<number>();
      for (const it of sel.items.items) {
        if (it.expr.kind === "column") projected.add(scope.resolveBare(it.expr.name));
        else if (it.expr.kind === "qualifiedColumn") {
          projected.add(scope.resolveQualified(it.expr.qualifier, it.expr.name));
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

    // Resolve each JOIN's ON predicate against the PARTIAL scope visible at that node (the
    // relations joined so far — rels[0..k+1]), so a forward reference to a not-yet-joined table
    // is a clean 42P01/42703 instead of an out-of-range row index. CROSS has no ON; INNER and
    // the OUTER kinds (LEFT/RIGHT/FULL) all resolve their ON the same way — the join kind only
    // changes how unmatched rows are handled in the loop below (§15).
    const joinOns: (RExpr | null)[] = sel.joins.map((j, k) => {
      if (j.on === null) return null;
      const partial = new Scope(scope.rels.slice(0, k + 2));
      return resolveBooleanFilter(partial, j.on, ptypes);
    });

    // All clauses resolved: finalize the inferred parameter types and bind the supplied values
    // (count mismatch 42601; out-of-range/family errors 22003/42804) BEFORE scanning any rows
    // (spec/design/api.md §5).
    const bound = bindParams(params, ptypes.finalize());

    // Materialize each base table once, in primary-key order, charging storageRowRead per
    // physical row (spec/design/cost.md §3 JOIN). The nested loop re-reads from these in-memory
    // buffers, which are not stores and charge nothing.
    const meter = new Meter();
    const materialized: Row[][] = scope.rels.map((rel) => {
      const tableRows: Row[] = [];
      for (const row of this.rowsInKeyOrder(rel.table.name)) {
        meter.charge(COSTS.storageRowRead);
        tableRows.push(row);
      }
      return tableRows;
    });

    // Left-deep nested-loop join. `running` holds the combined rows over the relations joined
    // so far (starting with the first table's rows). For each join, concatenate every running
    // row with every right-table row; CROSS keeps all pairs, INNER keeps a pair iff its ON
    // predicate is TRUE (three-valued — a NULL join key never matches). LEFT/FULL additionally
    // emit each unmatched left row NULL-extended over the right side; RIGHT/FULL emit each
    // unmatched right row NULL-extended over the left side. The NULL-extension pushes evaluate
    // no ON (no operator_eval — spec/design/cost.md §3). Output order is deterministic: running
    // order (outer) then right key order (inner), each unmatched left row after its (empty)
    // match run, all unmatched right rows last in right key order (CLAUDE.md §10).
    const nullRow = (n: number): Row => Array.from({ length: n }, () => nullValue());
    let running: Row[] = materialized[0]!;
    for (let k = 0; k < sel.joins.length; k++) {
      const rightRows = materialized[k + 1]!;
      const on = joinOns[k]!;
      const kind = sel.joins[k]!.kind;
      const emitLeft = kind === "left" || kind === "full";
      const emitRight = kind === "right" || kind === "full";
      // NULL-pad widths come from the SCOPE, never a sampled row, so they are correct even when
      // `running`/`rightRows` is empty: the right table begins at flat offset rels[k+1].offset
      // (= the width of every running row) and is that many columns wide.
      const leftPad = scope.rels[k + 1]!.offset;
      const rightPad = scope.rels[k + 1]!.table.columns.length;
      const next: Row[] = [];
      const rightMatched: boolean[] = new Array(rightRows.length).fill(false);
      for (const left of running) {
        let leftMatched = false;
        for (let ri = 0; ri < rightRows.length; ri++) {
          const combined = left.concat(rightRows[ri]!);
          if (on === null || isTrue(evalExpr(on, combined, bound, meter))) {
            next.push(combined);
            leftMatched = true;
            rightMatched[ri] = true;
          }
        }
        if (emitLeft && !leftMatched) next.push(left.concat(nullRow(rightPad)));
      }
      if (emitRight) {
        for (let ri = 0; ri < rightRows.length; ri++) {
          if (!rightMatched[ri]) next.push(nullRow(leftPad).concat(rightRows[ri]!));
        }
      }
      running = next;
    }

    // WHERE over the combined rows. A WHERE arithmetic can throw (22003/22012); each surviving
    // combined row's filter accrues operator_eval.
    const rows: Row[] = [];
    for (const row of running) {
      if (filter === null || isTrue(evalExpr(filter, row, bound, meter))) rows.push(row);
    }

    // ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
    // and a full tie keeps the scan order (JS Array#sort is stable). Each key's NULL placement
    // is decoupled from its value-direction flip (spec/design/grammar.md §10). Aggregate queries
    // sort their GROUP rows in the aggregate branch below — not these pre-aggregation rows — so
    // this is gated to plain queries.
    if (!isAgg && order.length > 0) {
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
    if (isAgg) {
      // Aggregate query — group + accumulate (aggregates.md §5). Bucket the post-WHERE rows by
      // their group-key values; the bucket key is the value-canonical distinctRowKey (it
      // collapses 1.5/1.50 and groups NULL with NULL), and the Map is only an index — output
      // order comes from the insertion-ordered `groups`, never Map iteration (no order leak —
      // §8/§10). Whole-table aggregation (no GROUP BY) is one pre-created empty-key group, so it
      // emits ONE row even over zero input; GROUP BY over an empty table creates no groups ->
      // zero rows. Each (row × aggregate) charges aggregateAccumulate; the bucketing/finalize is
      // unmetered (cost.md §3).
      const newAccs = (): Acc[] => aggSpecs.map((s) => newAcc(s.plan));
      const index = new Map<string, number>();
      const groups: { keys: Value[]; accs: Acc[] }[] = [];
      if (groupKeys.length === 0) {
        groups.push({ keys: [], accs: newAccs() });
        index.set("", 0);
      }
      for (const row of rows) {
        const keys = groupKeys.map((gk) => row[gk]!);
        const k = distinctRowKey(keys);
        let gi = index.get(k);
        if (gi === undefined) {
          gi = groups.length;
          index.set(k, gi);
          groups.push({ keys, accs: newAccs() });
        }
        const accs = groups[gi]!.accs;
        aggSpecs.forEach((spec, i) => {
          meter.charge(COSTS.aggregateAccumulate);
          const v = spec.operand === null ? nullValue() : evalExpr(spec.operand, row, bound, meter);
          foldAcc(accs[i]!, v);
        });
      }
      // Build one synthetic row per group: [group_key_values..., aggregate_results...].
      let groupRows = groups.map((g) => [...g.keys, ...g.accs.map((a) => finalizeAcc(a))]);
      // HAVING: filter the grouped rows (after aggregation, before ORDER BY). The predicate is
      // evaluated against each group's synthetic row (charging its operatorEvals per group);
      // only a TRUE result keeps the group. A dropped group charges no rowProduced (§8).
      if (having !== null) {
        groupRows = groupRows.filter((srow) => isTrue(evalExpr(having!, srow, bound, meter)));
      }
      // ORDER BY over the grouped output (keys are synthetic group-key slots).
      if (order.length > 0) {
        groupRows.sort((a, b) => {
          for (const key of order) {
            const c = keyCmp(a[key.idx]!, b[key.idx]!, key.descending, key.nullsFirst);
            if (c !== 0) return c;
          }
          return 0;
        });
      }
      // Window + project; only an emitted row charges rowProduced + its projection cost.
      const [start, end] = windowBounds(groupRows.length);
      out = groupRows.slice(start, end).map((srow) => {
        meter.charge(COSTS.rowProduced);
        return projections.map((p) => evalExpr(p, srow, bound, meter));
      });
    } else if (sel.distinct) {
      // Project every filtered row (charging projection cost per row, the §3 asymmetry),
      // keeping first occurrences. `seen` is membership-only: output order comes from the
      // deterministic source iteration, never from Set iteration (no order leak — §8/§10).
      const seen = new Set<string>();
      const distinctRows: Value[][] = [];
      for (const row of rows) {
        const tuple = projections.map((p) => evalExpr(p, row, bound, meter));
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
        return projections.map((p) => evalExpr(p, row, bound, meter));
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
        case "bytea":
          // A distinct 'y' tag over the hex form (collision-free), so a bytea never collides
          // with a text value of the same bytes.
          return "y" + renderByteaHex(v.bytes);
        case "uuid":
          // A distinct 'u' tag over the canonical form, so a uuid never collides with a
          // bytea/text of the same bytes.
          return "u" + renderUuid(v.bytes);
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
  | { kind: "bytea" } // the bytea family (raw bytes); does not promote
  | { kind: "uuid" } // the uuid family (fixed 16 bytes); does not promote. The first non-integer key.
  | { kind: "null" };

// RExpr is a resolved expression over fixed column indices. Arithmetic/neg nodes carry
// their (promotion-tower) result type so the computed value can be range-checked.
type RExpr =
  | { kind: "column"; index: number }
  // A bind parameter, by 0-based index into the bound-values array passed to evalExpr. Its
  // static type was inferred from context at resolve (spec/design/api.md §5); the value is
  // supplied (and coerced) before evaluation.
  | { kind: "param"; index: number }
  | { kind: "constInt"; value: bigint }
  | { kind: "constBool"; value: boolean }
  | { kind: "constText"; value: string }
  | { kind: "constDecimal"; value: Decimal }
  | { kind: "constBytea"; value: Uint8Array }
  | { kind: "constUuid"; value: Uint8Array }
  | { kind: "constNull" }
  | { kind: "cast"; target: ScalarType; typmod: DecimalTypmod | null; operand: RExpr }
  | { kind: "neg"; result: ScalarType; operand: RExpr }
  | { kind: "not"; operand: RExpr }
  | { kind: "arith"; op: BinaryOp; result: ScalarType; lhs: RExpr; rhs: RExpr }
  | { kind: "compare"; op: BinaryOp; lhs: RExpr; rhs: RExpr }
  | { kind: "and"; lhs: RExpr; rhs: RExpr }
  | { kind: "or"; lhs: RExpr; rhs: RExpr }
  | { kind: "isNull"; operand: RExpr; negated: boolean }
  | { kind: "distinct"; lhs: RExpr; rhs: RExpr; negated: boolean }
  | { kind: "like"; lhs: RExpr; rhs: RExpr; negated: boolean }
  | { kind: "case"; arms: { cond: RExpr; result: RExpr }[]; els: RExpr; coerceDecimal: boolean };

// ============================================================================
// Aggregate resolution + accumulation (spec/design/aggregates.md).
//
// An aggregate query's select list resolves in "collect" mode: each aggregate call is
// collected into an AggSpec (its plan + resolved argument) and replaced by a reference to a
// synthetic-row slot (a "column" RExpr indexing the finalized aggregate results), so the
// existing evaluator projects the result with no new node. Outside collect mode (WHERE / ON /
// an aggregate's own argument / any non-aggregate query) a column resolves normally and an
// aggregate call is a 42803 grouping error.
// ============================================================================

// AggPlan is the runtime plan for one aggregate, fixed at resolve from the function + operand
// type (the PG widening — spec/design/aggregates.md §3).
type AggPlan =
  | "countStar" // COUNT(*) — count every row
  | "count" // COUNT(expr) — count non-NULL inputs
  | "sumInt" // SUM(int16|int32) — accumulate int64, result int64 (trap at int64)
  | "sumDecimal" // SUM(int64|decimal) — accumulate decimal, result decimal
  | "avg" // AVG — decimal sum + count; result sum/count (NULL if count 0)
  | "min"
  | "max";

// AggSpec is one resolved aggregate: its plan and its resolved argument (evaluated per input
// row against the real row). operand is null for COUNT(*).
type AggSpec = { plan: AggPlan; operand: RExpr | null };

// AggCtx threads the aggregate-resolution mode through resolve. collecting === false is the
// Forbidden mode (a funcCall is 42803; columns resolve normally); collecting === true is an
// aggregate query's projection (a funcCall collects into specs and resolves to a synthetic slot
// groupKeys.length + index; a column resolves to its position among groupKeys if it is a
// grouping key, else 42803). groupKeys holds the resolved flat indices of the GROUP BY columns
// (empty for whole-table aggregation). The synthetic row is [group_key_values..., agg_results...].
type AggCtx = { collecting: boolean; groupKeys: number[]; specs: AggSpec[] };

// Acc is a running aggregate accumulator (one per AggSpec), folded per input row then finalized.
type Acc = {
  plan: AggPlan;
  count: bigint;
  sumInt: bigint;
  sumDec: Decimal;
  seen: boolean;
  cur: Value | null;
};

function newAcc(plan: AggPlan): Acc {
  return { plan, count: 0n, sumInt: 0n, sumDec: Decimal.fromBigInt(0n), seen: false, cur: null };
}

// foldAcc folds one input value into the accumulator. NULL arguments are skipped (COUNT(*)
// ignores the value and always counts). Traps 22003 on SUM overflow at the result bound.
function foldAcc(a: Acc, v: Value): void {
  switch (a.plan) {
    case "countStar":
      a.count += 1n;
      break;
    case "count":
      if (v.kind !== "null") a.count += 1n;
      break;
    case "sumInt":
      if (v.kind === "int") {
        const s = a.sumInt + v.int;
        if (!inRange("int64", s)) throw overflow("int64");
        a.sumInt = s;
        a.seen = true;
      }
      break;
    case "sumDecimal":
      if (v.kind !== "null") {
        a.sumDec = a.sumDec.add(toDecimal(v));
        a.seen = true;
      }
      break;
    case "avg":
      if (v.kind !== "null") {
        a.sumDec = a.sumDec.add(toDecimal(v));
        a.count += 1n;
      }
      break;
    case "min":
    case "max":
      if (v.kind !== "null") {
        if (a.cur === null) a.cur = v;
        else {
          const c = valueCmp(a.cur, v);
          const keepCur = a.plan === "min" ? c <= 0 : c >= 0;
          if (!keepCur) a.cur = v;
        }
      }
      break;
  }
}

// finalizeAcc produces the aggregate's final value over the group. COUNT → its count (0 over
// empty); SUM/MIN/MAX → NULL over an empty/all-NULL group; AVG → sum/count (NULL if count 0).
function finalizeAcc(a: Acc): Value {
  switch (a.plan) {
    case "countStar":
    case "count":
      return intValue(a.count);
    case "sumInt":
      return a.seen ? intValue(a.sumInt) : nullValue();
    case "sumDecimal":
      return a.seen ? decimalValue(a.sumDec) : nullValue();
    case "avg":
      return a.count === 0n ? nullValue() : decimalValue(a.sumDec.div(Decimal.fromBigInt(a.count)));
    case "min":
    case "max":
      return a.cur ?? nullValue();
  }
}

// itemsHaveAggregate reports whether any select item contains an aggregate call.
function itemsHaveAggregate(items: SelectItems): boolean {
  if (items.kind === "all") return false;
  return items.items.some((it) => exprHasFuncCall(it.expr));
}

// exprHasFuncCall reports whether an expression tree contains a function (aggregate) call.
function exprHasFuncCall(e: Expr): boolean {
  switch (e.kind) {
    case "funcCall":
      return true;
    case "cast":
      return exprHasFuncCall(e.inner);
    case "unary":
      return exprHasFuncCall(e.operand);
    case "isNull":
      return exprHasFuncCall(e.operand);
    case "binary":
    case "isDistinct":
      return exprHasFuncCall(e.lhs) || exprHasFuncCall(e.rhs);
    case "in":
      return exprHasFuncCall(e.lhs) || e.list.some(exprHasFuncCall);
    case "between":
      return exprHasFuncCall(e.lhs) || exprHasFuncCall(e.lo) || exprHasFuncCall(e.hi);
    case "like":
      return exprHasFuncCall(e.lhs) || exprHasFuncCall(e.rhs);
    case "case":
      return (
        (e.operand !== null && exprHasFuncCall(e.operand)) ||
        e.whens.some((w) => exprHasFuncCall(w.cond) || exprHasFuncCall(w.result)) ||
        (e.els !== null && exprHasFuncCall(e.els))
      );
    default:
      return false;
  }
}

// resolveAggregate resolves an aggregate call into a synthetic-row reference, collecting its
// AggSpec. Valid only in collect mode; in Forbidden mode (WHERE/ON/nested) it is 42803. The
// operand resolves in a fresh Forbidden sub-context (a nested aggregate is 42803; its columns
// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
function resolveAggregate(
  scope: Scope,
  e: { name: string; arg: Expr | null; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (!ag.collecting) {
    throw engineError("grouping_error", "aggregate functions are not allowed here");
  }
  const name = e.name.toLowerCase();
  const sub: AggCtx = { collecting: false, groupKeys: [], specs: [] };
  let plan: AggPlan;
  let operand: RExpr | null;
  let result: ResolvedType;
  const arg = (): Expr => {
    if (e.arg === null) throw engineError("syntax_error", "aggregate requires an argument");
    return e.arg;
  };
  if (name === "count") {
    if (e.star) {
      plan = "countStar";
      operand = null;
      result = { kind: "int", ty: "int64" };
    } else {
      operand = resolve(scope, arg(), null, sub, params).node;
      plan = "count";
      result = { kind: "int", ty: "int64" };
    }
  } else if (name === "sum" || name === "avg" || name === "min" || name === "max") {
    if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    const r = resolve(scope, arg(), null, sub, params);
    operand = r.node;
    if (name === "sum") {
      if (r.type.kind === "int" && r.type.ty === "int64") {
        plan = "sumDecimal";
        result = { kind: "decimal" };
      } else if (r.type.kind === "int") {
        plan = "sumInt";
        result = { kind: "int", ty: "int64" };
      } else if (r.type.kind === "decimal") {
        plan = "sumDecimal";
        result = { kind: "decimal" };
      } else {
        throw noAggOverload("sum");
      }
    } else if (name === "avg") {
      if (r.type.kind === "int" || r.type.kind === "decimal") {
        plan = "avg";
        result = { kind: "decimal" };
      } else {
        throw noAggOverload("avg");
      }
    } else {
      plan = name; // "min" | "max"
      result = r.type;
    }
  } else {
    throw engineError("undefined_function", "function does not exist: " + e.name);
  }
  // Aggregate results follow the group-key values in the synthetic row.
  const slot = ag.groupKeys.length + ag.specs.length;
  ag.specs.push({ plan, operand });
  return { node: { kind: "column", index: slot }, type: result };
}

// collectColumn resolves a column reference (already at real flat index `idx`) under an
// aggregate context. In Forbidden mode it reads the real row directly; in collect mode it must
// be a grouping key — resolved to its synthetic-row slot (its position among the group keys) —
// else 42803.
function collectColumn(scope: Scope, ag: AggCtx, idx: number, name: string): { node: RExpr; type: ResolvedType } {
  const type = resolvedTypeOf(scope.columnAt(idx).type);
  if (!ag.collecting) return { node: { kind: "column", index: idx }, type };
  const pos = ag.groupKeys.indexOf(idx);
  if (pos < 0) throw groupingErrorColumn(name);
  return { node: { kind: "column", index: pos }, type };
}

// noAggOverload is 42883 — an aggregate over an operand family it has no overload for.
function noAggOverload(fn: string): EngineError {
  return engineError("undefined_function", "no " + fn + " aggregate for that argument type");
}

// groupingErrorColumn is the 42803 for a non-aggregated column with no GROUP BY.
function groupingErrorColumn(name: string): EngineError {
  return engineError(
    "grouping_error",
    "column " + name + " must appear in the GROUP BY clause or be used in an aggregate function",
  );
}

// ============================================================================
// Resolution scope (multi-table FROM — spec/design/grammar.md §15).
//
// A Scope is the ordered list of relations a SELECT's FROM clause puts in scope, each
// carrying the flat COLUMN OFFSET at which its columns begin in the concatenated (joined)
// row. A resolved column reference bakes a single flat index offset+local into "column", so
// the joined row is just each relation's row concatenated in FROM order and the evaluator is
// unchanged. A single-table SELECT / UPDATE / DELETE is a one-relation scope (offset 0).
//
// NOTE (forward-compat): the scope keys resolution ONLY on column name and type — never on a
// column's notNull / primaryKey flags. A column on the nullable side of a future outer join
// is NULL-extended at runtime regardless of its declared nullability (grammar.md §15).
// ============================================================================

// ScopeRel is one relation in a FROM scope: its label (alias, else table name, lower-cased
// for case-insensitive matching), the table, and the flat offset of its first column.
type ScopeRel = { label: string; table: Table; offset: number };

class Scope {
  rels: ScopeRel[];
  constructor(rels: ScopeRel[]) {
    this.rels = rels;
  }

  // single builds a one-relation scope (the single-table SELECT / UPDATE / DELETE case).
  static single(t: Table): Scope {
    return new Scope([{ label: t.name.toLowerCase(), table: t, offset: 0 }]);
  }

  // resolveBare resolves a bare column name to a flat row index: no relation has it → 42703;
  // two or more relations have it → 42702 ambiguous; exactly one → its flat index.
  resolveBare(name: string): number {
    let found = -1;
    for (const r of this.rels) {
      const local = columnIndex(r.table, name);
      if (local >= 0) {
        if (found >= 0) throw ambiguousColumn(name);
        found = r.offset + local;
      }
    }
    if (found < 0) throw undefinedColumn(name);
    return found;
  }

  // resolveQualified resolves a qualified rel.col to a flat row index: an unknown rel is 42P01,
  // a known rel with no such column is 42703. Never ambiguous (it names one relation).
  resolveQualified(qualifier: string, name: string): number {
    const q = qualifier.toLowerCase();
    for (const r of this.rels) {
      if (r.label === q) {
        const local = columnIndex(r.table, name);
        if (local < 0) throw undefinedColumn(name);
        return r.offset + local;
      }
    }
    throw missingFromEntry(qualifier);
  }

  // columnAt returns the column at a flat index (the index is known valid — resolution made it).
  columnAt(flat: number): Column {
    for (const r of this.rels) {
      const n = r.table.columns.length;
      if (flat >= r.offset && flat < r.offset + n) return r.table.columns[flat - r.offset]!;
    }
    throw new Error("a resolved flat column index is always in range");
  }
}

// undefinedColumn is 42703 — a column name that no relation in scope defines.
function undefinedColumn(name: string): EngineError {
  return engineError("undefined_column", "column does not exist: " + name);
}

// ambiguousColumn is 42702 — a bare column name that more than one relation in scope defines.
function ambiguousColumn(name: string): EngineError {
  return engineError("ambiguous_column", "column reference " + name + " is ambiguous");
}

// missingFromEntry is 42P01 — a qualifier that names no relation in the FROM clause.
function missingFromEntry(qualifier: string): EngineError {
  return engineError("undefined_table", "missing FROM-clause entry for table " + qualifier);
}

// resolvedTypeOf is the resolved (static) type of a column of scalar type ty.
function resolvedTypeOf(ty: ScalarType): ResolvedType {
  if (isText(ty)) return { kind: "text" };
  if (isBool(ty)) return { kind: "bool" };
  if (isDecimal(ty)) return { kind: "decimal" };
  if (isBytea(ty)) return { kind: "bytea" };
  if (isUuid(ty)) return { kind: "uuid" };
  return { kind: "int", ty };
}

// ParamTypes accumulates the inferred type of each bind parameter ($N) across every clause of a
// statement (spec/design/api.md §5). types[i] is the inferred scalar type of $(i+1); a null entry
// marks a parameter referenced before any context fixed its type. Shared across every clause so a
// $1 used in both WHERE and the select list unifies, then finalized.
class ParamTypes {
  types: (ScalarType | null)[] = [];

  // note records that $(idx0+1) appears with context type ty (null = no context here). It unifies
  // with any prior inference: equal types agree, two integer widths widen to the wider, an
  // incompatible concrete pair is 42804.
  note(idx0: number, ty: ScalarType | null): void {
    while (idx0 >= this.types.length) this.types.push(null);
    if (ty === null) return;
    const prev = this.types[idx0]!;
    this.types[idx0] = prev === null ? ty : unifyParamType(prev, ty, idx0);
  }

  // finalize returns the ordered parameter types. A slot referenced but never typed — including a
  // gap in $1..$N — is 42P18 indeterminate_datatype.
  finalize(): ScalarType[] {
    const out: ScalarType[] = [];
    for (let i = 0; i < this.types.length; i++) {
      const t = this.types[i]!;
      if (t === null) {
        throw engineError(
          "indeterminate_datatype",
          `could not determine data type of parameter $${i + 1}`,
        );
      }
      out.push(t);
    }
    return out;
  }
}

// unifyParamType unifies two inferred types for the same parameter: equal agrees; two integer
// widths widen to the wider; any other mismatch is 42804 (spec/design/api.md §5).
function unifyParamType(a: ScalarType, b: ScalarType, idx0: number): ScalarType {
  if (a === b) return a;
  if (isInteger(a) && isInteger(b)) return rank(a) >= rank(b) ? a : b;
  throw engineError("datatype_mismatch", `inconsistent types inferred for parameter $${idx0 + 1}`);
}

// bindParams coerces each supplied bind value to its inferred parameter type, two-phase /
// all-or-nothing like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value
// is validated up front (22003/42804/22P02/23502 via storeValue) before any row is touched.
function bindParams(supplied: Value[], types: ScalarType[]): Value[] {
  if (supplied.length !== types.length) {
    throw engineError(
      "syntax_error",
      `bind parameter count mismatch: statement expects ${types.length}, got ${supplied.length}`,
    );
  }
  return types.map((ty, i) => storeValue(supplied[i]!, ty, null, false, `$${i + 1}`));
}

// rejectParamsForDDL throws 42601 if bind parameters are supplied to a CREATE/DROP TABLE (which
// has no expressions to bind — spec/design/api.md §5).
function rejectParamsForDDL(params: Value[]): void {
  if (params.length > 0) {
    throw engineError("syntax_error", "bind parameters are not allowed in a DDL statement");
  }
}

// resolveProjections resolves SELECT items into evaluable projections (any result type is
// allowed in the select list, including boolean — SELECT a = b), each paired with its output
// column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM order,
// each relation's columns in catalog order (§15).
function resolveProjections(
  scope: Scope,
  items: SelectItems,
  ag: AggCtx,
  params: ParamTypes,
): { nodes: RExpr[]; names: string[] } {
  if (items.kind === "all") {
    const nodes: RExpr[] = [];
    const names: string[] = [];
    for (const r of scope.rels) {
      r.table.columns.forEach((c, i) => {
        nodes.push({ kind: "column", index: r.offset + i });
        names.push(c.name);
      });
    }
    return { nodes, names };
  }
  const nodes: RExpr[] = [];
  const names: string[] = [];
  for (const it of items.items) {
    nodes.push(resolve(scope, it.expr, null, ag, params).node);
    names.push(it.alias ?? outputName(scope, it.expr));
  }
  return { nodes, names };
}

// outputName is the output column name of an un-aliased select item (grammar.md §8/§15): a
// bare or qualified column reference takes the catalog's canonical name (never the qualifier,
// never the SELECT spelling); every other expression takes the fixed "?column?". The column is
// known to exist — resolve validated it.
function outputName(scope: Scope, e: Expr): string {
  if (e.kind === "column") return scope.columnAt(scope.resolveBare(e.name)).name;
  if (e.kind === "qualifiedColumn") return scope.columnAt(scope.resolveQualified(e.qualifier, e.name)).name;
  // An un-aliased aggregate call is named by its lowercased function name (PG; §8).
  if (e.kind === "funcCall") return e.name.toLowerCase();
  return "?column?";
}

// resolveBooleanFilter resolves a WHERE / ON expression; it must resolve to boolean (or an
// untyped NULL, always unknown → no rows). An integer- or text-valued one is a 42804.
function resolveBooleanFilter(scope: Scope, e: Expr, params: ParamTypes): RExpr {
  // WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
  const { node, type } = resolve(scope, e, null, { collecting: false, groupKeys: [], specs: [] }, params);
  if (type.kind !== "bool" && type.kind !== "null") {
    throw typeError("argument of WHERE must be boolean");
  }
  return node;
}

// resolve resolves one Expr into an RExpr plus its static type. ctx (non-null) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); null
// defaults a bare literal to int64.
function resolve(
  scope: Scope,
  e: Expr,
  ctx: ScalarType | null,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  switch (e.kind) {
    case "column": {
      // Resolve for existence first (42703/42702 take priority, matching PostgreSQL); then in
      // an aggregate query's projection the column must be a grouping key (else 42803).
      const idx = scope.resolveBare(e.name);
      return collectColumn(scope, ag, idx, e.name);
    }
    case "qualifiedColumn": {
      const idx = scope.resolveQualified(e.qualifier, e.name);
      return collectColumn(scope, ag, idx, e.name);
    }
    case "param": {
      // A bind parameter is an adaptable operand (like an integer/string literal): it takes its
      // type from ctx — the sibling operand, target column, or CAST target. Record the inferred
      // type (null = no context here; finalize 42P18s a parameter that never gets one).
      const idx0 = e.index - 1;
      params.note(idx0, ctx);
      const type: ResolvedType = ctx !== null ? resolvedTypeOf(ctx) : { kind: "null" };
      return { node: { kind: "param", index: idx0 }, type };
    }
    case "funcCall":
      return resolveAggregate(scope, e, ag, params);
    case "literal":
      switch (e.literal.kind) {
        case "null":
          return { node: { kind: "constNull" }, type: { kind: "null" } };
        case "bool":
          return { node: { kind: "constBool", value: e.literal.value }, type: { kind: "bool" } };
        case "text": {
          // A string literal is text by default (collation C). It adapts to a BYTEA or a UUID
          // context (types.md §6/§13/§14): decode the hex input (bytea) or the PG-flexible uuid
          // input (uuid) — 22P02 on malformed; any other context — including none — keeps it text.
          if (ctx !== null && isBytea(ctx)) {
            return {
              node: { kind: "constBytea", value: decodeByteaLiteral(e.literal.text) },
              type: { kind: "bytea" },
            };
          }
          if (ctx !== null && isUuid(ctx)) {
            return {
              node: { kind: "constUuid", value: decodeUuidLiteral(e.literal.text) },
              type: { kind: "uuid" },
            };
          }
          return { node: { kind: "constText", value: e.literal.text }, type: { kind: "text" } };
        }
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
      // bytea casts are likewise deferred (types.md §5/§13): casting TO bytea is 0A000.
      if (isBytea(target)) {
        throw engineError("feature_not_supported", "casting to bytea is not supported yet");
      }
      // uuid casts are likewise deferred (types.md §5/§14): casting TO uuid is 0A000.
      if (isUuid(target)) {
        throw engineError("feature_not_supported", "casting to uuid is not supported yet");
      }
      const inner = resolve(scope, e.inner, null, ag, params);
      if (inner.type.kind === "bool") {
        throw typeError("cannot cast boolean to " + canonicalName(target));
      }
      // Casting FROM text is likewise deferred (0A000).
      if (inner.type.kind === "text") {
        throw engineError("feature_not_supported", "casting from text is not supported yet");
      }
      // Casting FROM bytea is likewise deferred (0A000).
      if (inner.type.kind === "bytea") {
        throw engineError("feature_not_supported", "casting from bytea is not supported yet");
      }
      // Casting FROM uuid is likewise deferred (0A000).
      if (inner.type.kind === "uuid") {
        throw engineError("feature_not_supported", "casting from uuid is not supported yet");
      }
      // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
      // decimal→decimal (re-scale), and NULL are all castable.
      const resultType: ResolvedType = isDecimal(target) ? { kind: "decimal" } : { kind: "int", ty: target };
      return { node: { kind: "cast", target, typmod, operand: inner.node }, type: resultType };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(scope, e.operand, ctx, ag, params);
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
        const { node, type } = resolve(scope, e.operand, null, ag, params);
        requireBool(type, "NOT requires a boolean operand");
        return { node: { kind: "not", operand: node }, type: { kind: "bool" } };
      }
    case "isNull": {
      const { node } = resolve(scope, e.operand, null, ag, params);
      return { node: { kind: "isNull", operand: node, negated: e.negated }, type: { kind: "bool" } };
    }
    case "isDistinct": {
      // NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a literal
      // adapts to its sibling; a text literal stays text), then require the operands be
      // comparable (both integer-ish or both text-ish; a mixed pair is 42804). The result
      // is always a definite boolean (functions.md §3).
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      classifyComparable(p.lt, p.rt);
      return { node: { kind: "distinct", lhs: p.rl, rhs: p.rr, negated: e.negated }, type: { kind: "bool" } };
    }
    case "binary":
      return resolveBinary(scope, e.op, e.lhs, e.rhs, ag, params);
    case "in": {
      // Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` is
      // `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list is
      // non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree reuses the
      // `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics, per-element operand
      // typing (a too-wide literal → 22003, a cross-family element → 42804), and cost all fall
      // out. The LHS is evaluated once per element (the OR-chain model — a documented cost
      // consequence, cost.md §3).
      let folded: Expr | null = null;
      for (const elem of e.list) {
        const eq: Expr = { kind: "binary", op: "eq", lhs: e.lhs, rhs: elem };
        folded = folded === null ? eq : { kind: "binary", op: "or", lhs: folded, rhs: eq };
      }
      // folded is non-null: the parser guarantees a non-empty list.
      let desugared = folded as Expr;
      if (e.negated) {
        desugared = { kind: "unary", op: "not", operand: desugared };
      }
      return resolve(scope, desugared, ctx, ag, params);
    }
    case "between": {
      // Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
      // result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a FALSE
      // operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL. NOT BETWEEN
      // negates the whole conjunction. The LHS is evaluated twice (the desugar model — a
      // documented cost consequence, cost.md §3).
      const ge: Expr = { kind: "binary", op: "ge", lhs: e.lhs, rhs: e.lo };
      const le: Expr = { kind: "binary", op: "le", lhs: e.lhs, rhs: e.hi };
      let desugared: Expr = { kind: "binary", op: "and", lhs: ge, rhs: le };
      if (e.negated) {
        desugared = { kind: "unary", op: "not", operand: desugared };
      }
      return resolve(scope, desugared, ctx, ag, params);
    }
    case "like": {
      // LIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal stays
      // text), then require BOTH operands be text (or a bare NULL); a non-text operand is 42804.
      // We do NOT use classifyComparable here — it would wrongly accept bytea×bytea.
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      requireTextOrNull(p.lt);
      requireTextOrNull(p.rt);
      return { node: { kind: "like", lhs: p.rl, rhs: p.rr, negated: e.negated }, type: { kind: "bool" } };
    }
    case "case": {
      // Resolve each branch's condition: searched form requires a boolean WHEN (42804
      // otherwise); simple form desugars to `operand = value` (reusing the `=` operand pairing +
      // comparability check, so the value adapts to the operand's type). The operand is evaluated
      // once per tested branch (the desugar model, like IN).
      const arms: { cond: RExpr; result: RExpr }[] = [];
      const resultTypes: ResolvedType[] = [];
      for (const w of e.whens) {
        let cond: RExpr;
        if (e.operand !== null) {
          const eq: Expr = { kind: "binary", op: "eq", lhs: e.operand, rhs: w.cond };
          cond = resolve(scope, eq, null, ag, params).node;
        } else {
          const rc = resolve(scope, w.cond, null, ag, params);
          requireBool(rc.type, "CASE WHEN condition must be boolean");
          cond = rc.node;
        }
        const rres = resolve(scope, w.result, null, ag, params);
        resultTypes.push(rres.type);
        arms.push({ cond, result: rres.node });
      }
      let els: RExpr;
      if (e.els !== null) {
        const re = resolve(scope, e.els, null, ag, params);
        els = re.node;
        resultTypes.push(re.type);
      } else {
        els = { kind: "constNull" };
        resultTypes.push({ kind: "null" });
      }
      const unified = unifyCaseTypes(resultTypes);
      return {
        node: { kind: "case", arms, els, coerceDecimal: unified.kind === "decimal" },
        type: unified,
      };
    }
  }
}

function resolveBinary(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
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
      const p = resolveOperandPair(scope, lhs, rhs, ag, params);
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
      const p = resolveOperandPair(scope, lhs, rhs, ag, params);
      classifyComparable(p.lt, p.rt);
      return { node: { kind: "compare", op, lhs: p.rl, rhs: p.rr }, type: { kind: "bool" } };
    }
    default: {
      // "and" | "or"
      const l = resolve(scope, lhs, null, ag, params);
      const r = resolve(scope, rhs, null, ag, params);
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
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { rl: RExpr; lt: ResolvedType; rr: RExpr; rt: ResolvedType } {
  const lhsLit = isAdaptableOperand(lhs);
  const rhsLit = isAdaptableOperand(rhs);
  let l: { node: RExpr; type: ResolvedType };
  let r: { node: RExpr; type: ResolvedType };
  if (lhsLit && rhsLit) {
    l = resolve(scope, lhs, "int64", ag, params);
    r = resolve(scope, rhs, "int64", ag, params);
  } else if (lhsLit) {
    r = resolve(scope, rhs, null, ag, params);
    l = resolve(scope, lhs, ctxOf(r.type), ag, params);
  } else if (rhsLit) {
    l = resolve(scope, lhs, null, ag, params);
    r = resolve(scope, rhs, ctxOf(l.type), ag, params);
  } else {
    l = resolve(scope, lhs, null, ag, params);
    r = resolve(scope, rhs, null, ag, params);
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
  // bytea compares only with bytea (or NULL); bytea with a number or text is a mismatch.
  const byteaL = lt.kind === "bytea";
  const byteaR = rt.kind === "bytea";
  if (byteaL !== byteaR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a bytea value with a non-bytea value");
  }
  // uuid compares only with uuid (or NULL); uuid with anything else is a mismatch.
  const uuidL = lt.kind === "uuid";
  const uuidR = rt.kind === "uuid";
  if (uuidL !== uuidR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a uuid value with a non-uuid value");
  }
}

// isAdaptableOperand reports whether e is an adaptable operand — one that takes its type from its
// sibling: an integer or string literal, or a bind parameter $N (spec/design/api.md §5). NULL,
// boolean, and decimal literals do not take a sibling's context.
function isAdaptableOperand(e: Expr): boolean {
  if (e.kind === "param") return true;
  return e.kind === "literal" && (e.literal.kind === "int" || e.literal.kind === "text");
}

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps int64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
function ctxOf(t: ResolvedType): ScalarType | null {
  if (t.kind === "int") return t.ty;
  if (t.kind === "bytea") return "bytea";
  if (t.kind === "uuid") return "uuid";
  if (t.kind === "text") return "text";
  if (t.kind === "bool") return "boolean";
  if (t.kind === "decimal") return "decimal";
  return null;
}

// intTypeOf returns the integer type of t (for promotion), or null.
function intTypeOf(t: ResolvedType): ScalarType | null {
  return t.kind === "int" ? t.ty : null;
}

// decodeByteaLiteral decodes a single-quoted literal's content as a bytea value via the hex
// input form (parseByteaHex), mapping malformed hex to a 22P02 (invalid_text_representation).
// Used when a string literal adapts to a bytea context (types.md §6/§13); the trap is
// deterministic and fires at resolve time, before any scan.
function decodeByteaLiteral(str: string): Uint8Array {
  const r = parseByteaHex(str);
  if ("error" in r) {
    throw engineError("invalid_text_representation", "invalid input syntax for type bytea: " + r.error);
  }
  return r.bytes;
}

// decodeUuidLiteral decodes a single-quoted literal's content as a uuid value via the
// PG-flexible input (parseUuid), mapping malformed input to a 22P02. Used when a string literal
// adapts to a uuid context (types.md §6/§14); deterministic, fires at resolve before any scan.
function decodeUuidLiteral(str: string): Uint8Array {
  const r = parseUuid(str);
  if ("error" in r) {
    throw engineError("invalid_text_representation", "invalid input syntax for type uuid: " + r.error);
  }
  return r.bytes;
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
  if (t.kind === "bool" || t.kind === "text" || t.kind === "bytea" || t.kind === "uuid") {
    throw typeError("arithmetic operators require numeric operands");
  }
}

function requireBool(t: ResolvedType, msg: string): void {
  if (
    t.kind === "int" ||
    t.kind === "text" ||
    t.kind === "decimal" ||
    t.kind === "bytea" ||
    t.kind === "uuid"
  ) {
    throw typeError(msg);
  }
}

// requireTextOrNull: LIKE requires both operands be text (or a bare NULL literal, which is
// comparable with anything and makes the result NULL at eval). A non-text operand is a 42804
// type error (spec/design/grammar.md §22).
function requireTextOrNull(t: ResolvedType): void {
  if (t.kind !== "text" && t.kind !== "null") throw typeError("LIKE requires text operands");
}

// unifyCaseTypes unifies a CASE's result-arm types (the THEN results + the ELSE, or "null" for an
// implicit ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped
// (they adapt); an all-NULL CASE is text (PostgreSQL). The non-NULL arms must share a family — all
// numeric unify to decimal if any is decimal, else the widest integer (the promotion tower);
// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family mix
// is 42804.
function unifyCaseTypes(arms: ResolvedType[]): ResolvedType {
  const nonNull = arms.filter((t) => t.kind !== "null");
  if (nonNull.length === 0) return { kind: "text" }; // every arm NULL/untyped → text
  let allNumeric = true;
  let anyDecimal = false;
  for (const t of nonNull) {
    if (t.kind !== "int" && t.kind !== "decimal") allNumeric = false;
    if (t.kind === "decimal") anyDecimal = true;
  }
  if (allNumeric) {
    if (anyDecimal) return { kind: "decimal" };
    // All integer: the widest via the promotion tower (width is unobservable in output — every
    // integer renders under the `I` tag — but the fold keeps the type precise).
    let acc = nonNull[0]!;
    for (const t of nonNull.slice(1)) acc = { kind: "int", ty: promote(acc, t) };
    return acc;
  }
  // Non-numeric: every arm must be the same family as the first (cross-family is 42804).
  const first = nonNull[0]!;
  for (const t of nonNull.slice(1)) {
    if (t.kind !== first.kind) throw typeError("CASE result types must be compatible");
  }
  return first;
}

// coerceCaseValue coerces a CASE arm's value to the unified result type. The only runtime
// coercion needed is widening an integer result to decimal when the unified type is decimal —
// integer-width unification needs none (all integers are bigint), and an all-NULL CASE is text but
// every arm evaluates to NULL anyway.
function coerceCaseValue(v: Value, toDecimal: boolean): Value {
  if (toDecimal && v.kind === "int") return decimalValue(Decimal.fromBigInt(v.int));
  return v;
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
  else if (isBytea(colTy)) ok = t.kind === "bytea" || t.kind === "null";
  else if (isUuid(colTy)) ok = t.kind === "uuid" || t.kind === "null";
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
      // A string literal adapts to a bytea column, decoding the hex input (types.md §6/§13);
      // malformed hex traps 22P02.
      if (isBytea(colTy)) return byteaValue(decodeByteaLiteral(v.text));
      // ... and to a uuid column via the PG-flexible uuid input (types.md §6/§14); 22P02 on bad input.
      if (isUuid(colTy)) return uuidValue(decodeUuidLiteral(v.text));
      throw typeError("cannot store a text value in " + canonicalName(colTy) + " column " + colName);
    case "bytea":
      if (isBytea(colTy)) return v;
      throw typeError("cannot store a bytea value in " + canonicalName(colTy) + " column " + colName);
    case "uuid":
      if (isUuid(colTy)) return v;
      throw typeError("cannot store a uuid value in " + canonicalName(colTy) + " column " + colName);
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
function literalToValue(lit: Literal): Value {
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
function evalExpr(e: RExpr, row: Row, params: Value[], m: Meter): Value {
  switch (e.kind) {
    case "column":
      return row[e.index]!;
    case "param":
      // The supplied value, already coerced to its inferred type by bindParams before execution
      // (spec/design/api.md §5).
      return params[e.index]!;
    case "constInt":
      return intValue(e.value);
    case "constBool":
      return { kind: "bool", value: e.value };
    case "constText":
      return textValue(e.value);
    case "constDecimal":
      return decimalValue(e.value);
    case "constBytea":
      return byteaValue(e.value);
    case "constUuid":
      return uuidValue(e.value);
    case "constNull":
      return nullValue();
    case "cast": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, params, m);
      if (v.kind === "null") return nullValue();
      return evalCast(v, e.target, e.typmod);
    }
    case "neg": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, params, m);
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
      return boolNot(evalExpr(e.operand, row, params, m));
    }
    case "arith": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, params, m);
      const b = evalExpr(e.rhs, row, params, m);
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
      const a = evalExpr(e.lhs, row, params, m);
      const b = evalExpr(e.rhs, row, params, m);
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
      const a = evalExpr(e.lhs, row, params, m);
      const b = evalExpr(e.rhs, row, params, m);
      return boolAnd(a, b);
    }
    case "or": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, params, m);
      const b = evalExpr(e.rhs, row, params, m);
      return boolOr(a, b);
    }
    case "isNull": {
      m.charge(COSTS.operatorEval);
      const isNull = evalExpr(e.operand, row, params, m).kind === "null";
      return { kind: "bool", value: isNull !== e.negated };
    }
    case "distinct": {
      m.charge(COSTS.operatorEval);
      const same = notDistinctFrom(evalExpr(e.lhs, row, params, m), evalExpr(e.rhs, row, params, m));
      // negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
      // the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
      // unknown (the null_safe discipline, functions.md §3).
      return { kind: "bool", value: same === e.negated };
    }
    case "like": {
      m.charge(COSTS.operatorEval);
      const subject = evalExpr(e.lhs, row, params, m);
      const pattern = evalExpr(e.rhs, row, params, m);
      // NULL propagates BEFORE the matcher runs, so a malformed pattern against a NULL operand
      // is still NULL, never 22025 (matches PG — grammar.md §22).
      if (subject.kind === "null" || pattern.kind === "null") return nullValue();
      if (subject.kind !== "text" || pattern.kind !== "text") {
        throw new Error("unreachable: resolver requires text LIKE operands");
      }
      // negated carries NOT LIKE: matched !== negated flips the result for NOT LIKE.
      return { kind: "bool", value: likeMatch(subject.text, pattern.text) !== e.negated };
    }
    case "case": {
      // CASE is the ONE deliberate exception to "no short-circuit" (cost.md §3): conditions are
      // evaluated in order and evaluation STOPS at the first TRUE — a FALSE or NULL/UNKNOWN
      // condition falls through, and later arms (and their results) are NOT evaluated. Required
      // for PG semantics (e.g. `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero).
      // Charge the node, then only the conditions up to the match plus the selected result.
      m.charge(COSTS.operatorEval);
      for (const arm of e.arms) {
        const cv = evalExpr(arm.cond, row, params, m);
        if (cv.kind === "bool" && cv.value) {
          return coerceCaseValue(evalExpr(arm.result, row, params, m), e.coerceDecimal);
        }
      }
      return coerceCaseValue(evalExpr(e.els, row, params, m), e.coerceDecimal);
    }
  }
}

// likeMatch is the SQL LIKE matcher (spec/design/grammar.md §22): `%` matches any (possibly
// empty) run of characters, `_` exactly one character, and `\` (the default escape) makes the
// next pattern character literal. It iterates by Unicode CODE POINT via Array.from (NOT `str[i]`
// / charCodeAt, the UTF-16 trap) so astral characters match `_` — a CLAUDE.md §8 determinism
// surface. Two-pointer greedy backtracking, identical across cores. It throws a 22025 error when
// the escape character is the LAST pattern character reached during matching (PostgreSQL's "LIKE
// pattern must not end with escape character") — data-dependent, since an earlier mismatch
// returns false first.
function likeMatch(subject: string, pattern: string): boolean {
  const s = Array.from(subject);
  const p = Array.from(pattern);
  let si = 0;
  let pi = 0;
  // The last '%' position in the pattern (a backtrack point) and the subject index when it was
  // taken; -1 until a '%' has been seen.
  let starPi = -1;
  let starSi = 0;
  while (si < s.length) {
    if (pi < p.length && p[pi] === "\\") {
      // Escape: the next pattern character must match the subject literally.
      if (pi + 1 >= p.length) {
        throw engineError("invalid_escape_sequence", "LIKE pattern must not end with escape character");
      }
      if (s[si] === p[pi + 1]) {
        si++;
        pi += 2;
        continue;
      }
      // literal mismatch → fall through to backtrack
    } else if (pi < p.length && p[pi] === "_") {
      si++;
      pi++;
      continue;
    } else if (pi < p.length && p[pi] === "%") {
      starPi = pi;
      starSi = si;
      pi++;
      continue;
    } else if (pi < p.length && p[pi] === s[si]) {
      si++;
      pi++;
      continue;
    }
    // Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
    if (starPi >= 0) {
      pi = starPi + 1;
      starSi++;
      si = starSi;
      continue;
    }
    return false;
  }
  // Subject consumed: any pattern remainder must be all '%' to match.
  while (pi < p.length && p[pi] === "%") pi++;
  return pi === p.length;
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
  if (a.kind === "bytea" && b.kind === "bytea") return compareBytea(a.bytes, b.bytes);
  if (a.kind === "uuid" && b.kind === "uuid") return compareBytea(a.bytes, b.bytes);
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
    case "text":
      return 4;
    case "bytea":
      return 5;
    default: // uuid
      return 6;
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
