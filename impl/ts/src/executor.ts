// Statement executor (CLAUDE.md §10). Mirrors executor.go / executor.rs: dispatch a
// parsed statement; analyze (resolve types, predicates, projections against the
// catalog); run. Errors throw EngineError. Results are produced deterministically
// (CLAUDE.md §10): scan in primary-key order, three-valued WHERE (only TRUE keeps a
// row), stable ORDER BY with NULLs last (the PostgreSQL model).

import type {
  AlterSequence,
  BinaryOp,
  CreateIndex,
  CreateSequence,
  CreateTable,
  CreateType,
  Delete,
  DropIndex,
  DropSequence,
  DropTable,
  DropType,
  Expr,
  Insert,
  InsertValue,
  JoinKind,
  Literal,
  OrderKey,
  QueryExpr,
  RefAction,
  Select,
  SelectItems,
  SetOp,
  SetOpKind,
  Statement,
  SubscriptSpec,
  TableRef,
  TypeMod,
  Update,
  WithQuery,
} from "./ast.ts";
import {
  type CheckConstraint,
  type ColField,
  type ColType,
  type Column,
  type CompositeField,
  type CompositeType,
  type DefaultExpr,
  type FkAction,
  type ForeignKey,
  type IndexDef,
  type SequenceDef,
  type Table,
  columnIndex,
  defaultSequenceBounds,
  pkIndices,
  primaryKeyIndex,
  resolveColType,
} from "./catalog.ts";
import { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import {
  Decimal,
  decimalFromParts,
  EXP_LIMIT,
  MAX_PRECISION,
  MAX_SCALE,
  workDiv,
  workLinear,
  workMod,
  workMul,
} from "./decimal.ts";
import { encodeBool, encodeInt } from "./encoding.ts";
import { EngineError, engineError } from "./errors.ts";
import type { SharedPaging } from "./paging.ts";
import { parseSQL } from "./parser.ts";
import { type KeyBound, compareBytes, unboundedBound } from "./pmap.ts";
import { DEFAULT_WORK_MEM, type RowCompare, type SpillSink, Sorter } from "./spill.ts";
import { type Entry, type Row, TableStore } from "./storage.ts";
import {
  type DecimalTypmod,
  type ScalarType,
  type Type,
  canonicalName,
  arrayT,
  compositeRefName,
  compositeT,
  isCompositeType,
  inRange,
  isBool,
  isBytea,
  isDecimal,
  isFloat,
  isInteger,
  isText,
  isUuid,
  isTimestamp,
  isTimestamptz,
  isInterval,
  isDate,
  promoteFloat,
  rank,
  roundToWidth,
  scalarT,
  scalarTypeFromName,
  typeCanonicalName,
  typeIsBoolean,
  typeIsInteger,
  typeIsTimestamp,
  typeIsTimestamptz,
  typeIsDate,
  typeIsUuid,
  typeScalar,
  widthBytes,
} from "./types.ts";
import { parseTimestamp, parseTimestamptz } from "./timestamp.ts";
import { parseDate } from "./date.ts";
import { uuidExtractTimestampMicros, uuidExtractVersion } from "./uuid.ts";
import { type ClockFunc, type RandomFill, Seam, StmtRng } from "./seam.ts";
import {
  type Interval,
  intervalAdd,
  intervalCmp,
  intervalNeg,
  intervalSpan,
  intervalSub,
  makeInterval,
  mulByFraction,
  parseFactorDecimal,
  parseInterval,
  tsDiff,
  tsShift,
} from "./interval.ts";
import { AGGREGATES, type AggregateDesc, OPERATORS, type OperatorDesc } from "./operators.ts";
import {
  type Value,
  type ArrayInResult,
  type ThreeValued,
  boolAnd,
  boolNot,
  arrayValue,
  emptyArray,
  arrayNdim,
  arrayUbound,
  boolOr,
  boolValue,
  byteaValue,
  canonFloat,
  compareBytea,
  compareTextC,
  compositeValue,
  parseArrayLiteral,
  decimalValue,
  eq3,
  float32Value,
  float64Value,
  floatTotalCmp,
  from3,
  gt3,
  intValue,
  isNullTest,
  isTrue,
  lt3,
  notDistinctFrom,
  nullValue,
  parseByteaHex,
  parseRecordTokens,
  parseUuid,
  renderByteaHex,
  renderFloat,
  renderUuid,
  textValue,
  uuidValue,
  timestampValue,
  timestamptzValue,
  intervalValue,
  dateValue,
} from "./value.ts";

// Outcome is the result of executing one statement: a bare statement (CREATE, INSERT,
// UPDATE, DELETE) or a query result set. cost is the deterministic execution cost accrued
// while running it (CLAUDE.md §13) — a DML statement accrues its scan + filter cost even
// though it returns no rows. It is a bigint for i64 parity across cores (§8).
// rowsAffected is how many rows a DML statement (INSERT/UPDATE/DELETE without RETURNING)
// touched — PostgreSQL's command-tag count (spec/design/api.md §4); 0 for a DML statement
// that matched nothing, null for DDL and transaction control, which have no row count.
// columnTypes is the canonical name of each output column's resolved type (parallel to
// columnNames) — i16/i32/i64/text/boolean/decimal/…, or "unknown" for an untyped NULL
// column. The resolved SCALAR type — for decimal the unconstrained "decimal", not the
// numeric(p,s) typmod (spec/design/conformance.md §7).
export type Outcome =
  | { kind: "statement"; cost: bigint; rowsAffected: number | null }
  | { kind: "query"; columnNames: string[]; columnTypes: string[]; rows: Value[][]; cost: bigint };

// SelectResult is the full result of running a SELECT (runSelect): the output column names and
// their resolved types, the rows in result order, and the accrued cost. Internal — executeSelect
// drops the types into the public Outcome, while INSERT ... SELECT uses the types to gate
// assignability up front (spec/design/grammar.md §24).
type SelectResult = {
  columnNames: string[];
  columnTypes: ResolvedType[];
  rows: Value[][];
  cost: bigint;
};

// Database is the whole database: catalog + per-table in-memory stores. Single
// committed state (CLAUDE.md §3); the staging-buffer commit model lands later. Names
// are keyed case-insensitively (lowercased).
// DEFAULT_PAGE_SIZE is the default serialization page size (8 KiB — spec/design/storage.md §3),
// used for a fresh in-memory or newly-created database when no explicit size is given.
export const DEFAULT_PAGE_SIZE = 8192;

// DEFAULT_MAX_SQL_LENGTH is the default per-handle input-SQL byte limit (1 MiB — CLAUDE.md §13;
// spec/design/api.md §8, cost.md §7). The §13 input-size gate's default ceiling: generous for
// hand-written / ORM SQL, yet bounds the parse tree to a few MB so unbounded untrusted input
// cannot exhaust memory. A caller raises it (trusted bulk loads) or sets 0 for unlimited via
// setMaxSqlLength. Identical across cores (§8).
export const DEFAULT_MAX_SQL_LENGTH = 1 << 20;

// MAX_COMPOSITE_DEPTH is the maximum composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A
// composite type's depth is the length of its deepest chain of nested composites, counting itself: a
// row of scalars is depth 1, `CREATE TYPE b AS (x a)` is `1 + depth(a)`, and an array field counts
// as its element (array levels are not composite levels — compositeRefName looks through one array
// level the same way). A CREATE TYPE whose result would exceed this is rejected 54001, and a loaded
// catalog that exceeds it is treated as corrupt XX001 — bounding the native recursion of every
// derived walk (value codec, comparator, record_out/record_in, resolveColType) at the two producers
// (DDL + load) so all downstream walks are transitively stack-safe. A fixed, cross-core constant like
// MAX_EXPR_DEPTH (§8). The chain is built across many cheap statements, so neither the per-statement
// input-size cap nor the parser nesting counter sees it (cost.md §7).
export const MAX_COMPOSITE_DEPTH = 32;

// SQL_BYTE_ENCODER measures a statement's UTF-8 byte length for the input-size gate (cost.md §7),
// matching the byte counts Rust (&str::len) and Go (len(string)) use, so the cap accepts / rejects
// identically across cores (§8). A TextEncoder is available in Node and the browser (the OPFS
// host), so this stays host-agnostic.
const SQL_BYTE_ENCODER = new TextEncoder();
function utf8ByteLength(s: string): number {
  return SQL_BYTE_ENCODER.encode(s).length;
}

// Snapshot is an immutable committed (or in-progress working) database state — the catalog + each
// table's store + the commit counter (spec/design/transactions.md §2). The committed state is one
// of these; a write transaction builds a new one from it (the persistent stores clone O(1) —
// pmap.ts / §3). A reader holds a Snapshot and is thereby stable for its life: a later commit
// produces a new Snapshot and never mutates this one. (JavaScript has no shared-memory threads for
// live objects, so P5.3b gives snapshot ISOLATION across async interleavings, not CPU parallelism.)
export class Snapshot {
  // txid is the snapshot's version — the commit counter (transactions.md §8; the watermark unit).
  txid: bigint;
  tables: Map<string, Table>;
  // types holds the user-defined composite (row) types, keyed by lowercased name
  // (spec/design/composite.md). A database-level object set, separate from tables; serialized into
  // the catalog's composite-type entries (spec/fileformat/format.md). Sorted by key when serialized
  // so map-iteration order never leaks (CLAUDE.md §8). CompositeType objects are never mutated in
  // place (only added/removed), so the shallow Map copy in clone is safe.
  types: Map<string, CompositeType>;
  stores: Map<string, TableStore>;
  // indexStores holds each secondary index's B-tree (spec/design/indexes.md §3): a
  // TableStore with ZERO value columns (entry keys only — the on-disk empty-payload
  // record), keyed by the lowercased index name (index names live in the relation
  // namespace, globally unique). Which table owns an index is recorded in that table's
  // indexes list.
  indexStores: Map<string, TableStore>;
  // sequences holds the database-level sequences, keyed by lowercased name
  // (spec/design/sequences.md). A separate object set from tables/types; serialized into the
  // catalog's sequence entries (spec/fileformat/format.md, entry_kind = 2). The mutable counter
  // (lastValue/isCalled) lives here, so nextval advances the working snapshot and rolls back with
  // it (sequences.md §5). SequenceDef objects are never mutated in place (only replaced/removed),
  // so the shallow Map copy in clone is safe.
  sequences: Map<string, SequenceDef>;

  constructor(
    txid: bigint = 0n,
    tables: Map<string, Table> = new Map(),
    stores: Map<string, TableStore> = new Map(),
    indexStores: Map<string, TableStore> = new Map(),
    types: Map<string, CompositeType> = new Map(),
    sequences: Map<string, SequenceDef> = new Map(),
  ) {
    this.txid = txid;
    this.tables = tables;
    this.stores = stores;
    this.indexStores = indexStores;
    this.types = types;
    this.sequences = sequences;
  }

  // clone returns an independent copy: the catalog maps are shallow (Table / CompositeType /
  // SequenceDef objects are never mutated in place — only added/removed) and each store is an O(1)
  // persistent-map clone (pmap.ts).
  clone(): Snapshot {
    return new Snapshot(
      this.txid,
      new Map(this.tables),
      cloneStores(this.stores),
      cloneStores(this.indexStores),
      new Map(this.types),
      new Map(this.sequences),
    );
  }

  // table looks up a table definition by name (case-insensitive).
  table(name: string): Table | undefined {
    return this.tables.get(name.toLowerCase());
  }

  // compositeType looks up a composite type definition by name (case-insensitive).
  compositeType(name: string): CompositeType | undefined {
    return this.types.get(name.toLowerCase());
  }

  // putType registers a composite type (CREATE TYPE). Lower-cased name is the key. The caller has
  // already resolved field types and checked for a duplicate.
  putType(ty: CompositeType): void {
    this.types.set(ty.name.toLowerCase(), ty);
  }

  // removeType removes a composite type (DROP TYPE). The caller has checked there are no dependents.
  removeType(key: string): void {
    this.types.delete(key);
  }

  // compositeTypesSorted is all composite types in ascending lowercased-name order — the on-disk
  // emission order (spec/fileformat/format.md) and a deterministic order with no map-iteration leak
  // (§8). Keys are ASCII (so code-unit sort == byte sort).
  compositeTypesSorted(): CompositeType[] {
    return [...this.types.keys()].sort().map((k) => this.types.get(k)!);
  }

  // sequence looks up a sequence by name (case-insensitive).
  sequence(name: string): SequenceDef | undefined {
    return this.sequences.get(name.toLowerCase());
  }

  // putSequence registers a sequence (CREATE SEQUENCE). Lower-cased name is the key. The caller has
  // already validated the option set and checked the relation namespace for a collision.
  putSequence(seq: SequenceDef): void {
    this.sequences.set(seq.name.toLowerCase(), seq);
  }

  // removeSequence removes a sequence (DROP SEQUENCE). The caller has checked it exists.
  removeSequence(key: string): void {
    this.sequences.delete(key);
  }

  // sequencesSorted is all sequences in ascending lowercased-name order — the on-disk emission
  // order (spec/fileformat/format.md) and a deterministic order with no map-iteration leak (§8).
  sequencesSorted(): SequenceDef[] {
    return [...this.sequences.keys()].sort().map((k) => this.sequences.get(k)!);
  }

  // compositeDependent reports whether any table column or composite-type field still references the
  // composite type `name` (case-insensitive) — the DROP TYPE ... RESTRICT dependency check (2BP01).
  // Returns the first dependent's description for the error detail, or null if there are none.
  compositeDependent(name: string): string | null {
    const key = name.toLowerCase();
    // compositeRefName looks through one array level, so an addr[] column / field counts as a
    // dependent of addr exactly as a bare addr one does (spec/design/array.md §12).
    for (const t of this.tables.values()) {
      for (const c of t.columns) {
        const r = compositeRefName(c.type);
        if (r !== null && r.toLowerCase() === key) {
          return `column ${c.name} of table ${t.name}`;
        }
      }
    }
    for (const ct of this.types.values()) {
      for (const f of ct.fields) {
        const r = compositeRefName(f.type);
        if (r !== null && r.toLowerCase() === key) {
          return `field ${f.name} of type ${ct.name}`;
        }
      }
    }
    return null;
  }

  // fkDependent reports whether any OTHER table's FOREIGN KEY references the table `name`
  // (case-insensitive) — the DROP TABLE dependency check (2BP01 — spec/design/constraints.md
  // §6.10). A self-reference does NOT block the drop (a table's own FK on itself disappears with
  // it). Returns the first dependent's description, scanning in ascending lowercased table-name
  // order for determinism (within a table, fks is already in name order), or null.
  fkDependent(name: string): string | null {
    const key = name.toLowerCase();
    const tkeys = [...this.tables.keys()].sort();
    for (const tk of tkeys) {
      const t = this.tables.get(tk)!;
      if (t.name.toLowerCase() === key) continue; // a self-reference does not block the drop
      for (const fk of t.fks) {
        if (fk.refTable.toLowerCase() === key) {
          return `constraint ${fk.name} on table ${t.name}`;
        }
      }
    }
    return null;
  }

  // validateCompositeTypes validates the loaded composite-type catalog (the on-disk two-pass load —
  // spec/design/composite.md §3): every composite a field references must exist, the reference graph
  // must be acyclic, and no type may nest deeper than MAX_COMPOSITE_DEPTH. A dangling, cyclic, or
  // over-deep reference is a malformed file (XX001). Called once after the whole catalog is read, and
  // BEFORE any store is built — so the subsequent resolveColType walks (and every later
  // value-codec/comparator walk) recurse over a depth-bounded catalog and stay stack-safe (CLAUDE.md
  // §13; cost.md §7b).
  validateCompositeTypes(): void {
    // Existence: every composite a field references (directly, or as an array element —
    // compositeRefName looks through one array level) names a registered type.
    for (const ct of this.types.values()) {
      for (const f of ct.fields) {
        const r = compositeRefName(f.type);
        if (r !== null && this.compositeType(r) === undefined) {
          throw engineError(
            "data_corrupted",
            `composite type ${ct.name} references unknown type ${r}`,
          );
        }
      }
    }
    // One DFS over the type → referenced-types graph that enforces BOTH acyclicity and the
    // nesting-depth bound (color: 0 unvisited, 1 on-stack, 2 done; cache memoizes each done type's
    // absolute nesting depth). Two guards make it stack-safe AND sound regardless of visitation
    // order: levelsAbove >= MAX_COMPOSITE_DEPTH bounds the native recursion on a fresh descent, and
    // the post-compute depth > MAX_COMPOSITE_DEPTH check catches an over-deep type reached via a
    // memoized (color-2) shortcut — which the descent guard alone would miss when the catalog is
    // colored bottom-up. Existence ran first, so every referenced type is present.
    const color = new Map<string, number>();
    const cache = new Map<string, number>();
    const visit = (key: string, levelsAbove: number): number => {
      if (levelsAbove >= MAX_COMPOSITE_DEPTH) {
        throw engineError(
          "data_corrupted",
          `composite type nesting exceeds the maximum depth of ${MAX_COMPOSITE_DEPTH}`,
        );
      }
      const c = color.get(key) ?? 0;
      if (c === 1) {
        throw engineError("data_corrupted", `composite type definition cycle through ${key}`);
      }
      if (c === 2) return cache.get(key) ?? 1;
      color.set(key, 1);
      let child = 0;
      const ct = this.types.get(key);
      if (ct) {
        for (const f of ct.fields) {
          const r = compositeRefName(f.type);
          if (r !== null) child = Math.max(child, visit(r.toLowerCase(), levelsAbove + 1));
        }
      }
      const depth = 1 + child;
      if (depth > MAX_COMPOSITE_DEPTH) {
        throw engineError(
          "data_corrupted",
          `composite type nesting exceeds the maximum depth of ${MAX_COMPOSITE_DEPTH}`,
        );
      }
      color.set(key, 2);
      cache.set(key, depth);
      return depth;
    };
    for (const k of [...this.types.keys()]) {
      if ((color.get(k) ?? 0) === 0) visit(k, 0);
    }
  }

  // compositeTypeDepth returns the composite-type nesting depth of ty against this snapshot's type
  // catalog, memoized in cache (lowercased name → depth): a scalar is 0, T[] is depth(T) (array
  // levels are not composite levels — compositeRefName looks through one array level the same way),
  // and a composite is 1 + max(field depths) (an empty composite is 1). The CREATE TYPE gate uses
  // this against the *existing* catalog, every type of which already satisfies depth ≤
  // MAX_COMPOSITE_DEPTH (the load + create invariant), so the recursion is bounded by the limit;
  // memoization keeps a diamond-shaped reference graph linear (spec/design/cost.md §7b).
  compositeTypeDepth(ty: Type, cache: Map<string, number>): number {
    const r = compositeRefName(ty);
    if (r === null) return 0; // a scalar (or a scalar array) adds no composite level
    const key = r.toLowerCase();
    const cached = cache.get(key);
    if (cached !== undefined) return cached;
    const def = this.types.get(key);
    let depth = 1;
    if (def) {
      let child = 0;
      for (const f of def.fields) child = Math.max(child, this.compositeTypeDepth(f.type, cache));
      depth = 1 + child;
    }
    cache.set(key, depth);
    return depth;
  }

  // store returns a table's store (the table is known to exist).
  store(name: string): TableStore {
    return this.stores.get(name.toLowerCase())!;
  }

  // putTable registers a new table and its empty store. The store carries the page payload cap (=
  // page_size − 12) and the column types so the page-backed B-tree can weigh records for its
  // size-driven split (spec/fileformat/format.md).
  putTable(t: Table, pageSize: number): void {
    const key = t.name.toLowerCase();
    // Resolve each column's ColType against the (already-registered) composite-type catalog —
    // self-contained codec/coercion trees the store carries, so the value codec never re-walks the
    // type catalog per row (spec/design/composite.md §4). Composite types are registered before any
    // table (the types-first catalog emission order), so resolveColType always resolves.
    const colTypes = t.columns.map((c) => resolveColType(c.type, this.types));
    this.stores.set(key, new TableStore(pageSize - 12, colTypes)); // 12 = PAGE_HEADER
    this.tables.set(key, t);
  }

  // removeTable removes a table's definition, its store, and its indexes' stores (DROP
  // TABLE — the indexes have no independent life, spec/design/indexes.md §2).
  removeTable(key: string): void {
    const t = this.tables.get(key);
    if (t) for (const idx of t.indexes) this.indexStores.delete(idx.name.toLowerCase());
    this.tables.delete(key);
    this.stores.delete(key);
  }

  // indexStore returns a secondary index's store (the index is known to exist). nameKey
  // is the lowercased index name.
  indexStore(nameKey: string): TableStore {
    return this.indexStores.get(nameKey)!;
  }

  // putIndex registers a new (empty) secondary index on tableKey: insert its definition
  // into the table's indexes in ascending lowercased-name order (the catalog/planner
  // order — spec/design/indexes.md §6) and create its zero-column store. The Table object
  // is re-allocated (catalog Tables are never mutated in place — snapshots share them).
  putIndex(tableKey: string, def: IndexDef, pageSize: number): void {
    const nameKey = def.name.toLowerCase();
    this.indexStores.set(nameKey, new TableStore(pageSize - 12, [])); // 12 = PAGE_HEADER
    const old = this.tables.get(tableKey)!;
    let pos = old.indexes.length;
    for (let i = 0; i < old.indexes.length; i++) {
      if (old.indexes[i]!.name.toLowerCase() > nameKey) {
        pos = i;
        break;
      }
    }
    const indexes = [...old.indexes.slice(0, pos), def, ...old.indexes.slice(pos)];
    this.tables.set(tableKey, { ...old, indexes });
  }

  // putIndexStore registers a loaded index store under its (lowercased) name — the file
  // loader's hook (format.ts): the owning table's indexes list came from its catalog
  // entry, so only the store is registered here.
  putIndexStore(nameKey: string, store: TableStore): void {
    this.indexStores.set(nameKey, store);
  }

  // removeIndex removes one secondary index (DROP INDEX): its definition from the owning
  // table and its store.
  removeIndex(tableKey: string, nameKey: string): void {
    const old = this.tables.get(tableKey);
    if (old) {
      const indexes = old.indexes.filter((ix) => ix.name.toLowerCase() !== nameKey);
      this.tables.set(tableKey, { ...old, indexes });
    }
    this.indexStores.delete(nameKey);
  }

  // findIndex finds the table owning the named index (case-insensitive):
  // [tableKey, def] or null.
  findIndex(name: string): [string, IndexDef] | null {
    const key = name.toLowerCase();
    for (const [tk, t] of this.tables) {
      for (const ix of t.indexes) {
        if (ix.name.toLowerCase() === key) return [tk, ix];
      }
    }
    return null;
  }

  // rowsInKeyOrder returns a table's rows in primary-key order, or [] if the table is absent.
  // Every value is fully materialized — the helper's callers compare whole rows, so no
  // unfetched reference may escape (large-values.md §14).
  rowsInKeyOrder(name: string): Row[] {
    const store = this.stores.get(name.toLowerCase());
    return store ? store.iterInKeyOrder().map((r) => store.resolveAll(r)) : [];
  }
}

// ActiveTx is an open transaction (spec/design/transactions.md §4.2). `writable` is the access mode
// (READ WRITE vs READ ONLY — a write in a READ ONLY block is 25006); `failed` marks an aborted block
// (every later statement but COMMIT/ROLLBACK is 25P02 — §6). `working` is the transaction's
// snapshot: a writable tx mutates it in place and publishes it at commit; a read-only tx reads it
// unchanged (read-your-snapshot, §4.3). committed is untouched until commit, so ROLLBACK drops this.
type ActiveTx = {
  writable: boolean;
  failed: boolean;
  working: Snapshot;
  // savedSessionSeq / savedSessionLastName capture the handle's currval/lastval session state
  // (spec/design/sequences.md §6) when this transaction opened. A nextval/setval inside the block
  // updates the handle's session state per-statement (so an in-block currval sees its own advance),
  // but those updates must ROLL BACK with the transaction (§5) — so ROLLBACK (and a failed/read-only
  // COMMIT) restores these, while a successful COMMIT keeps the advanced state.
  savedSessionSeq: Map<string, bigint>;
  savedSessionLastName: string | null;
};

export class Database {
  // The last committed, immutable state — what fresh readers (and autocommit reads) see.
  committed: Snapshot;
  // The open transaction, or null under autocommit (transactions.md §4.1/§4.2); a single-statement
  // autocommit write opens one implicitly for its duration.
  tx: ActiveTx | null;
  // Persistence identity (spec/design/api.md §2): the backing file path (null for in-memory) and
  // the page size this database serializes with. The commit counter lives in `committed.txid`.
  path: string | null;
  pageSize: number;
  // pageCount is the on-disk page high-water — the index an incremental commit extends at when the
  // free-list is exhausted (spec/fileformat/format.md). Set from the file's meta on open, from the
  // initial image on create; 0 (unused) for an in-memory database.
  pageCount: number;
  // freePages is the free-list (P6.2): page indices a prior root abandoned, reusable by the next
  // incremental commit (spec/fileformat/format.md *Reclamation*). Reconstructed on open as
  // [2, pageCount) minus the committed root's reachable pages; drawn lowest-first before the file is
  // extended. A page leaves the list only by being allocated into a new committed version, so it is
  // reachable from no live snapshot and reuse is torn-write-safe. Empty for an in-memory database and
  // for a freshly-created file (a from-scratch image leaks nothing).
  freePages: number[];
  // persistHook is the durable-write seam (spec/design/storage.md §2): null for an in-memory
  // database, set by a durable host (file.ts Node `fs` / opfs.ts Browser/OPFS — both to the shared
  // incremental commit, persist.ts) — and so the signal that a commit advances txid + persists (used
  // by commitTx). It is called by commitTx with the working snapshot being published (transactions.md
  // §4.1/§9). Injecting it here keeps the executor free of a host-module dependency (no import cycle).
  persistHook: ((db: Database, snap: Snapshot) => void) | null;
  // paging is the shared paging context for a file-backed database (spec/design/pager.md): the open
  // pager (kept for the handle's life) + the bounded leaf buffer pool, shared with every table store
  // so reads fault OnDisk leaves through the one pool. The load reads pages through it and every commit
  // writes through it. null for an in-memory database (persistHook is then null too); set by file.ts
  // open/create, dropped by close. A type-only import keeps the executor free of a file-module dependency.
  paging: SharedPaging | null;
  // maxCost is the caller-set execution-cost ceiling (CLAUDE.md §13; spec/design/api.md §8), or 0n
  // (the default) for unlimited. A positive value bounds every statement run on this handle: each
  // statement's Meter is built with this limit and aborts with 54P01 the instant accrued cost reaches
  // it. A handle setting (not stored in the file), set by setMaxCost; the primary guard for safely
  // evaluating untrusted, user-supplied queries.
  maxCost: bigint;
  // maxSqlLength is the maximum input SQL length, in bytes, accepted on this handle (CLAUDE.md §13;
  // spec/design/api.md §8, cost.md §7). Default DEFAULT_MAX_SQL_LENGTH (1 MiB); 0 = unlimited (a
  // trusted caller's opt-out). A statement whose text exceeds it is rejected with 54000 at the
  // handle's parse entry — before lexing — so unbounded input cannot exhaust parse memory/CPU (the
  // §13 input-size gate, which the cost meter cannot catch because parsing precedes metering). A
  // handle setting (not stored in the file), set by setMaxSqlLength.
  maxSqlLength: number;
  // readOnly marks a handle opened read-only (spec/design/api.md §2.1, OpenOptions.readOnly). A
  // read-only handle behaves like PostgreSQL hot standby: every transaction defaults to READ ONLY,
  // an explicit READ WRITE request and any write statement are 25006, and the file is opened
  // without write access, so it is never written. Always false for an in-memory or
  // normally-opened database.
  readOnly: boolean;
  // workMem is the work-memory budget in bytes (spec/design/spill.md §2, api.md §2.1): the memory a
  // single blocking operator (currently the ORDER BY external merge sort) may hold resident before it
  // spills sorted runs to disk. A handle setting (not stored in the file), set by setWorkMem; 0 means
  // unlimited (never spill). It never changes what a query observes (results + cost are invariant —
  // spill.md §6), only when an operator spills; an in-memory database ignores it. Default
  // DEFAULT_WORK_MEM.
  workMem: number;
  // spillSink is the host backing for the ORDER BY external merge sort's spilled runs (spec/design/
  // spill.md §4): set by a durable host that can spill to disk (file.ts → FileSpillSink), null for an
  // in-memory or OPFS database (which never spills — sorts stay resident, spill.md §2). A type-only
  // import keeps the executor free of any node:* dependency (the Node `fs` impl lives in spillfile.ts),
  // so the engine runs in a browser bundle (the OPFS host).
  spillSink: SpillSink | null;
  // seam: the entropy + clock seam for the uuid generators (spec/design/entropy.md): two
  // host-injectable functions (a random source + a clock), each unset ⇒ the platform primitive (OS
  // CSPRNG per value / wall clock). Set via setRandomSource / setClockSource; tests inject the
  // provided seededRandomSource + fixedClock (the # seed: / # clock: directives) for exact
  // cross-core output. A handle setting, not stored in the file.
  seam: Seam;
  // sessionSeq is per-handle SESSION currval state (spec/design/sequences.md §6): the last value
  // nextval/setval(…,true) produced IN THIS SESSION for each sequence (lowercased name). NOT part of
  // the snapshot and NOT persisted — strictly session-local, as in PostgreSQL. Updated when a
  // sequence-advancing statement succeeds (flushing pendingCurrval); currval of an unlisted sequence
  // this session is 55000.
  sessionSeq: Map<string, bigint>;
  // sessionLastName is per-handle SESSION lastval state (spec/design/sequences.md §6): the lowercased
  // NAME of the sequence the most recent nextval (of any sequence) ran on — null before the first
  // nextval. lastval() returns the CURRENT session value of that sequence (PG reads the last-used
  // sequence's cached value), so a setval on that same sequence is reflected; a setval never changes
  // which sequence this points to. 55000 when null.
  sessionLastName: string | null;
  // pendingSeq is per-STATEMENT running sequence advances (spec/design/sequences.md §4): a
  // nextval/setval records its advance here (seeded from the working snapshot on first touch), and
  // later calls in the same statement see the running state. On statement success it is flushed into
  // the working snapshot (so commit persists it); on error it is discarded (the transactional
  // rollback of the advance, sequences.md §5). Cleared at the start of every statement.
  pendingSeq: Map<string, SequenceDef>;
  // pendingCurrval is per-STATEMENT running currval updates (the names nextval/setval(…,true) touched
  // → their produced value). Separate from pendingSeq because currval is updated by a subset of
  // catalog mutations: setval(…,false) and ALTER … RESTART advance the counter without defining
  // currval. Flushed into sessionSeq on statement success.
  pendingCurrval: Map<string, bigint>;
  // pendingLastName is per-STATEMENT running lastval update (the lowercased name of the most recent
  // nextval this statement, null if none). setval never sets it. Flushed into sessionLastName on
  // success.
  pendingLastName: string | null;

  constructor() {
    this.committed = new Snapshot();
    this.tx = null;
    this.path = null;
    this.pageSize = DEFAULT_PAGE_SIZE;
    this.pageCount = 0;
    this.freePages = [];
    this.persistHook = null;
    this.paging = null;
    this.maxCost = 0n;
    this.maxSqlLength = DEFAULT_MAX_SQL_LENGTH;
    this.readOnly = false;
    this.workMem = DEFAULT_WORK_MEM;
    this.spillSink = null;
    this.seam = new Seam();
    this.sessionSeq = new Map();
    this.sessionLastName = null;
    this.pendingSeq = new Map();
    this.pendingCurrval = new Map();
    this.pendingLastName = null;
  }

  // setMaxCost sets the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
  // spec/design/api.md §8). A positive limit bounds every subsequent statement: it aborts with 54P01
  // the instant accrued cost reaches limit (spec/design/cost.md §6). limit <= 0n (the default) is
  // unlimited. The primary guard for safely evaluating untrusted, user-supplied queries; a handle
  // setting, not stored in the file.
  setMaxCost(limit: bigint): void {
    this.maxCost = limit;
  }

  // setMaxSqlLength sets the maximum input SQL length, in bytes, accepted on this handle (CLAUDE.md
  // §13; spec/design/api.md §8). A statement whose text exceeds bytes is rejected with 54000 at
  // parse entry, before lexing — the §13 input-size gate (cost.md §7). 0 is unlimited (a trusted
  // caller's opt-out); the default is DEFAULT_MAX_SQL_LENGTH (1 MiB). A handle setting, not stored
  // in the file (mirrors setMaxCost).
  setMaxSqlLength(bytes: number): void {
    this.maxSqlLength = bytes;
  }

  // parse parses one statement from sql, first enforcing this handle's maxSqlLength input-size limit
  // (CLAUDE.md §13; spec/design/api.md §8, cost.md §7). The §13 input-size gate: an over-limit
  // statement is rejected with 54000 before lexing, so unbounded untrusted input cannot exhaust
  // parse memory/CPU (the cost meter cannot catch this — parsing precedes metering). maxSqlLength
  // == 0 is unlimited. Every handle-bound parse path routes through here (execute/executeParams/
  // prepare/the session handles), so the per-handle limit has no hole. The byte length is the UTF-8
  // byte count, matching Rust/Go's byte-length idiom for cross-core identity (§8) — a JS UTF-16
  // .length would diverge on multi-byte input.
  parse(sql: string): Statement {
    const max = this.maxSqlLength;
    // Fast reject without encoding: a string's UTF-8 byte length is always >= its UTF-16 .length,
    // so if even the UTF-16 length exceeds the cap the statement is over-limit. Otherwise measure
    // the exact UTF-8 byte length (then bounded by the cap, so the encode is bounded).
    if (max > 0 && (sql.length > max || utf8ByteLength(sql) > max)) {
      throw engineError(
        "program_limit_exceeded",
        `SQL statement exceeds the maximum length of ${max} bytes`,
      );
    }
    return parseSQL(sql);
  }

  // setRandomSource injects a random source for the uuid generators (spec/design/entropy.md §6) —
  // the deterministic / reproducible path. Pass seededRandomSource for a byte-identical cross-core
  // stream (the conformance # seed: directive). clearRandomSource returns to the OS CSPRNG, drawn
  // per value (production — unpredictable output).
  setRandomSource(f: RandomFill): void {
    this.seam.randomFill = f;
  }

  clearRandomSource(): void {
    this.seam.randomFill = undefined;
  }

  // setClockSource injects a clock source for uuidv7 (entropy.md §6) — e.g. fixedClock (the # clock:
  // directive). clearClockSource returns to the wall clock.
  setClockSource(f: ClockFunc): void {
    this.seam.clock = f;
  }

  clearClockSource(): void {
    this.seam.clock = undefined;
  }

  // setWorkMem sets the work-memory budget (in bytes) for blocking operators run on this handle
  // (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly
  // this many bytes of rows resident before it spills sorted runs to disk. 0 is unlimited (never
  // spill). It never changes what a query observes (results + cost are invariant — spill.md §6),
  // only when an operator spills; an in-memory database ignores it. A handle setting, not stored in
  // the file (mirrors setMaxCost).
  setWorkMem(bytes: number): void {
    this.workMem = bytes;
  }

  // readSnap is the snapshot a read sees: the open transaction's working (read-your-writes for a
  // writable tx; the pinned snapshot for a read-only tx), else the committed snapshot.
  private readSnap(): Snapshot {
    return this.tx !== null ? this.tx.working : this.committed;
  }

  // working is the snapshot a write mutates — the open transaction's working. A write only ever
  // runs with a transaction open (autocommit opens one implicitly), so tx is non-null here.
  private working(): Snapshot {
    return this.tx!.working;
  }

  // seqNextval is nextval('name') (spec/design/sequences.md §4): advance the named sequence and
  // return the new value. The running state lives in pendingSeq, seeded from the working snapshot on
  // first touch this statement, and is flushed into the working snapshot + sessionSeq on statement
  // success (flushPendingSequences). A missing sequence is 42P01; advancing past a bound without
  // CYCLE is 2200H.
  seqNextval(name: string): bigint {
    const key = name.toLowerCase();
    let def = this.pendingSeq.get(key);
    if (def === undefined) {
      const committed = this.readSnap().sequence(name);
      if (committed === undefined) {
        throw engineError("undefined_table", `relation does not exist: ${name}`);
      }
      def = { ...committed };
    } else {
      def = { ...def };
    }
    let result: bigint;
    if (!def.isCalled) {
      // The first nextval returns START (the current lastValue) without incrementing.
      def.isCalled = true;
      result = def.lastValue;
    } else {
      // Advance by increment, treating an i64 overflow or a bound crossing identically (bigint never
      // overflows, so a wrap is detected purely by the bound test).
      const stepped = def.lastValue + def.increment;
      let next: bigint;
      if (def.increment > 0n && stepped <= def.maxValue) {
        next = stepped;
      } else if (def.increment < 0n && stepped >= def.minValue) {
        next = stepped;
      } else if (def.cycle) {
        next = def.increment > 0n ? def.minValue : def.maxValue;
      } else {
        throw engineError(
          "sequence_generator_limit_exceeded",
          `nextval: reached ${def.increment > 0n ? "maximum" : "minimum"} value of sequence ${name}`,
        );
      }
      def.lastValue = next;
      result = next;
    }
    this.pendingSeq.set(key, def);
    // nextval defines this session's currval for the sequence AND makes it the lastval target (the
    // most-recent-nextval sequence; lastval then reads its current session value — §6).
    this.pendingCurrval.set(key, result);
    this.pendingLastName = key;
    return result;
  }

  // seqSetval is setval('name', n) / setval('name', n, isCalled) (spec/design/sequences.md §4): set
  // the sequence's counter directly and return n. A missing sequence is 42P01; n outside
  // [minValue, maxValue] is 22003. lastValue = n, isCalled = the flag (default true); when isCalled
  // is true the value also defines this session's currval (PG: isCalled=false leaves currval
  // untouched). setval never updates lastval (PG — §6).
  seqSetval(name: string, n: bigint, isCalled: boolean): bigint {
    const key = name.toLowerCase();
    let def = this.pendingSeq.get(key);
    if (def === undefined) {
      const committed = this.readSnap().sequence(name);
      if (committed === undefined) {
        throw engineError("undefined_table", `relation does not exist: ${name}`);
      }
      def = { ...committed };
    } else {
      def = { ...def };
    }
    if (n < def.minValue || n > def.maxValue) {
      throw engineError(
        "numeric_value_out_of_range",
        `setval: value ${n} is out of bounds for sequence ${name} (${def.minValue}..${def.maxValue})`,
      );
    }
    def.lastValue = n;
    def.isCalled = isCalled;
    this.pendingSeq.set(key, def);
    // currval is defined only when isCalled (PG do_setval: elm->last_valid set iff iscalled).
    if (isCalled) this.pendingCurrval.set(key, n);
    return n;
  }

  // seqLastval is lastval() (spec/design/sequences.md §6): the CURRENT session value of the sequence
  // the most recent nextval (of any sequence) ran on IN THIS SESSION — PG reads the last-used
  // sequence's cached value, so a setval on that same sequence is reflected, while a setval on a
  // different sequence is not. Takes no name argument (no 42P01); 55000 before the first nextval. The
  // effective name and its value both honor the statement's running updates over the session state.
  seqLastval(): bigint {
    const key = this.pendingLastName ?? this.sessionLastName;
    if (key === null) {
      throw engineError("object_not_in_prerequisite_state", "lastval is not yet defined in this session");
    }
    const pending = this.pendingCurrval.get(key);
    if (pending !== undefined) return pending;
    const v = this.sessionSeq.get(key);
    if (v !== undefined) return v;
    // A nextval always defines the sequence's session value, so a recorded last-name with no value is
    // unreachable; fall back to 55000 defensively rather than returning a wrong value.
    throw engineError("object_not_in_prerequisite_state", "lastval is not yet defined in this session");
  }

  // seqCurrval is currval('name') (spec/design/sequences.md §6): the value nextval/setval(…,true)
  // last produced for this sequence IN THIS SESSION. Resolves the name against the catalog first
  // (42P01 if absent), then reads the running update this statement (pendingCurrval) else the session
  // value (sessionSeq); 55000 if it has not been defined this session.
  seqCurrval(name: string): bigint {
    if (this.readSnap().sequence(name) === undefined) {
      throw engineError("undefined_table", `relation does not exist: ${name}`);
    }
    const key = name.toLowerCase();
    const pending = this.pendingCurrval.get(key);
    if (pending !== undefined) return pending;
    const v = this.sessionSeq.get(key);
    if (v !== undefined) return v;
    throw engineError(
      "object_not_in_prerequisite_state",
      `currval of sequence ${name} is not yet defined in this session`,
    );
  }

  // flushPendingSequences flushes the statement's pending sequence advances into the working
  // snapshot (so a commit persists them) and the pending session updates into sessionSeq/
  // sessionLastName (so currval/lastval see them). Called on the success of a sequence-advancing
  // statement, while a write transaction is open; a no-op when nothing advanced. On statement error
  // the pending state is instead discarded (cleared at the next statement), giving the transactional
  // rollback of the advance (sequences.md §5).
  private flushPendingSequences(): void {
    for (const def of this.pendingSeq.values()) {
      this.working().putSequence(def);
    }
    for (const [key, v] of this.pendingCurrval) {
      this.sessionSeq.set(key, v);
    }
    if (this.pendingLastName !== null) {
      this.sessionLastName = this.pendingLastName;
    }
    this.pendingSeq.clear();
    this.pendingCurrval.clear();
    this.pendingLastName = null;
  }

  // restoreSessionState restores the handle's currval/lastval session state from a discarded
  // transaction's captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block
  // nextval/setval session updates. Called wherever a transaction is dropped without publishing.
  private restoreSessionState(tx: ActiveTx): void {
    this.sessionSeq = tx.savedSessionSeq;
    this.sessionLastName = tx.savedSessionLastName;
  }

  // newTx opens a transaction over a clone of the committed snapshot, capturing the handle's
  // currval/lastval session state so it can be restored if the transaction is discarded (the
  // rollback of any in-block nextval/setval session updates — spec/design/sequences.md §5/§6).
  private newTx(writable: boolean): ActiveTx {
    return {
      writable,
      failed: false,
      working: this.committed.clone(),
      savedSessionSeq: new Map(this.sessionSeq),
      savedSessionLastName: this.sessionLastName,
    };
  }

  // The monotonic commit counter (spec/design/api.md §2): the committed snapshot's version.
  get txid(): bigint {
    return this.committed.txid;
  }

  // oldestLiveTxid is the oldest still-live snapshot's txid (spec/design/transactions.md §8) — the
  // Phase-6 free-list reclamation gate. Single-handle (P5.3a) it is trivially the committed txid;
  // the P5.3b shared read snapshots make it meaningful.
  oldestLiveTxid(): bigint {
    return this.committed.txid;
  }

  // inTransaction reports whether an explicit transaction block is currently open
  // (spec/design/transactions.md §4.2). False under autocommit. Used by the host Transaction surface.
  inTransaction(): boolean {
    return this.tx !== null;
  }

  // table looks up a table definition by name (case-insensitive) in the visible snapshot.
  table(name: string): Table | undefined {
    return this.readSnap().table(name);
  }

  // compositeType looks up a composite type definition by name (case-insensitive) in the visible
  // snapshot (spec/design/composite.md).
  compositeType(name: string): CompositeType | undefined {
    return this.readSnap().compositeType(name);
  }

  // colTypeOf resolves a catalog Type into a self-contained ColType against the visible snapshot's
  // composite definitions (the codec/coercion tree — spec/design/composite.md §4). Used to coerce a
  // composite-element array literal (array-of-composite, array.md §12 AC1).
  colTypeOf(ty: Type): ColType {
    return resolveColType(ty, this.readSnap().types);
  }

  // tableNames is the canonical name of every table in the visible snapshot, sorted ascending
  // by lowercased name (the catalog's standing order — no map-iteration order may leak,
  // CLAUDE.md §8; keys are ASCII, so code-unit sort == byte sort). Secondary indexes are not
  // tables and are excluded (api.md §6).
  tableNames(): string[] {
    const snap = this.readSnap();
    return [...snap.tables.keys()].sort().map((k) => snap.tables.get(k)!.name);
  }

  // putTable registers a new table and its empty store in the working snapshot (DDL is
  // transactional — transactions.md §4.5).
  putTable(t: Table): void {
    this.working().putTable(t, this.pageSize);
  }

  // executeStmt executes one parsed statement with no bind parameters.
  executeStmt(stmt: Statement): Outcome {
    return this.executeStmtParams(stmt, []);
  }

  // executeStmtParams executes one parsed statement, binding params to its $N placeholders (an
  // empty array for an unparameterized statement). DDL statements take no parameters — supplying
  // any is a 42601 (spec/design/api.md §5).
  //
  // Transaction control (BEGIN/COMMIT/ROLLBACK) drives the handle's current-transaction state
  // directly (spec/design/transactions.md §4.2). Otherwise the statement runs either inside the
  // open explicit block or, with none open, under autocommit (§4.1):
  //
  //   - Inside a block (§4.2/§6): an aborted block rejects every statement but COMMIT/ROLLBACK with
  //     25P02; a write in a READ ONLY block is 25006; otherwise the statement runs against the
  //     working set in place — no per-statement durable write (the block publishes once, at COMMIT).
  //     ANY statement error aborts the block (it enters the failed state); the statement's own
  //     two-phase pass already guarantees it wrote nothing partial (§6), so the whole working set is
  //     discarded only at ROLLBACK.
  //   - Autocommit (§4.1): a read runs against the committed state directly; a write is its own
  //     transaction — the committed state is captured first (the stores are O(1) clones via the
  //     persistent map, pmap.ts), the statement runs, and on success the change is made durable (the
  //     persistHook, synchronous=on). Any failure — in the statement or the durable write — restores
  //     the captured state (rollback-on-error, discarding partial work and any rowid allocations,
  //     §7). For an in-memory database persistHook is null, so autocommit is pure in-memory.
  executeStmtParams(stmt: Statement, params: Value[]): Outcome {
    switch (stmt.kind) {
      case "begin":
        return this.beginTx(stmt.writable);
      case "commit":
        return this.commitTx();
      case "rollback":
        return this.rollbackTx();
    }
    // Fresh per-statement sequence-advance scratch (a prior statement's error may have left it
    // populated — it is discarded, not flushed, on error; sequences.md §5).
    this.pendingSeq.clear();
    this.pendingCurrval.clear();
    this.pendingLastName = null;

    // Inside an explicit block?
    const tx = this.tx;
    if (tx !== null) {
      if (tx.failed) {
        throw engineError(
          "in_failed_sql_transaction",
          "current transaction is aborted, commands ignored until end of transaction block",
        );
      }
      // Run the statement; ANY error aborts the block (it enters the failed state — §6).
      try {
        if (stmtIsWrite(stmt) && !tx.writable) {
          throw engineError(
            "read_only_sql_transaction",
            "cannot execute " + stmtKind(stmt) + " in a read-only transaction",
          );
        }
        const outcome = this.dispatchStmt(stmt, params);
        // Land any nextval advances into the block's working snapshot; COMMIT publishes them,
        // ROLLBACK discards them with the rest of the working set (sequences.md §5).
        this.flushPendingSequences();
        return outcome;
      } catch (e) {
        tx.failed = true;
        throw e;
      }
    }

    // Autocommit (no open block): an autocommit write runs as an implicit single-statement
    // transaction — open a working snapshot off committed, run, then commit on success / discard on
    // error. Because the write mutates only working, an error leaves committed untouched (no restore
    // needed); rolled-back rowid allocations vanish with working (§7).
    if (!stmtIsWrite(stmt)) {
      return this.dispatchStmt(stmt, params);
    }
    // On a read-only handle the implicit transaction is READ ONLY (PostgreSQL hot-standby
    // behavior — api.md §2.1), so an autocommit write fails exactly like a write inside a
    // READ ONLY block.
    if (this.readOnly) {
      throw engineError(
        "read_only_sql_transaction",
        "cannot execute " + stmtKind(stmt) + " in a read-only transaction",
      );
    }
    this.tx = this.newTx(true);
    let outcome: Outcome;
    try {
      outcome = this.dispatchStmt(stmt, params);
    } catch (e) {
      // The statement failed before any flush, so session state is untouched; restore from the
      // captured copy anyway to keep the discard path uniform (sequences.md §6).
      this.restoreSessionState(this.tx);
      this.tx = null;
      throw e;
    }
    // Persist any nextval advances into the working snapshot before publishing it (sequences.md §5);
    // a non-sequence statement flushes nothing.
    this.flushPendingSequences();
    this.commitTx();
    return outcome;
  }

  // beginTx opens an explicit transaction (spec/design/transactions.md §4.2). A nested BEGIN (a
  // block is already open) is 25001. writable is the *requested* access mode: null (unspecified)
  // defaults to READ WRITE on a normal handle and READ ONLY on a read-only handle (PostgreSQL
  // hot-standby behavior — api.md §2.1); requesting READ WRITE on a read-only handle is 25006.
  // The committed snapshot is captured as the transaction's working snapshot — a writable tx
  // mutates it in place; a read-only tx reads it unchanged (read-your-snapshot, §4.3). Cheap: the
  // persistent stores clone O(1) (pmap.ts) and the catalog is shallow. committed is untouched
  // until commit.
  beginTx(writable: boolean | null): Outcome {
    if (this.tx !== null) {
      throw engineError("active_sql_transaction", "there is already a transaction in progress");
    }
    if (writable === true && this.readOnly) {
      throw engineError(
        "read_only_sql_transaction",
        "cannot set transaction read-write mode on a read-only database",
      );
    }
    this.tx = this.newTx(writable ?? !this.readOnly);
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // commitTx commits the current transaction (spec/design/transactions.md §4.2). With no open block
  // it is a lenient no-op success. A failed block, or any read-only tx, publishes nothing — the
  // working snapshot is dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ WRITE block
  // publishes its working snapshot: bump its txid (a durable/persistent database — one with a
  // persistHook; an in-memory database has none and stays at txid 0), make it durable (the
  // persistHook, §9), then swap it in as committed. A durable-write failure leaves committed untouched
  // and rethrows. Returns to autocommit.
  commitTx(): Outcome {
    const tx = this.tx;
    if (tx === null) return { kind: "statement", cost: 0n, rowsAffected: null };
    this.tx = null;
    if (tx.failed || !tx.writable) {
      // A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG), so any
      // in-block session updates revert with the discarded working set (§5/§6).
      this.restoreSessionState(tx);
      return { kind: "statement", cost: 0n, rowsAffected: null };
    }
    const working = tx.working;
    // The txid advances for a durable database, signalled by the presence of a persistHook (the file
    // and OPFS hosts set one; an in-memory database has none and stays at txid 0). Keyed on persistHook,
    // not `path`: the OPFS host is durable but has no filesystem path (it leaves `path` null so the
    // disk-spill in newSorterFor stays off), so `path` alone would wrongly hold its txid at the create
    // value and reuse the same meta slot. For the file and in-memory hosts the two are equivalent
    // (path and persistHook are set or unset together), so this is observably identical there.
    if (this.persistHook !== null) working.txid = this.committed.txid + 1n;
    // persistHook (if any) throws on an I/O failure before committed is swapped, so committed is
    // left untouched (the commit failed; the working snapshot is discarded).
    if (this.persistHook !== null) this.persistHook(this, working);
    this.committed = working;
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // rollbackTx rolls back the current transaction (spec/design/transactions.md §4.2). With no open
  // block it is a no-op success. Otherwise the working snapshot is dropped — every staged
  // INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
  // committed was never mutated, so there is nothing to restore there. The handle's currval/lastval
  // session state, however, was updated in place by in-block nextval/setval, so it is restored from
  // the block's captured copy (sequences.md §5/§6).
  rollbackTx(): Outcome {
    if (this.tx !== null) this.restoreSessionState(this.tx);
    this.tx = null;
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // dispatchStmt routes one parsed statement to its executor. The autocommit transaction
  // handling (capture / durable commit / rollback-on-error) lives in executeStmtParams.
  private dispatchStmt(stmt: Statement, params: Value[]): Outcome {
    switch (stmt.kind) {
      case "createTable":
        rejectParamsForDDL(params);
        return this.executeCreateTable(stmt);
      case "dropTable":
        rejectParamsForDDL(params);
        return this.executeDropTable(stmt);
      case "createIndex":
        rejectParamsForDDL(params);
        return this.executeCreateIndex(stmt);
      case "dropIndex":
        rejectParamsForDDL(params);
        return this.executeDropIndex(stmt);
      case "createType":
        rejectParamsForDDL(params);
        return this.executeCreateType(stmt);
      case "dropType":
        rejectParamsForDDL(params);
        return this.executeDropType(stmt);
      case "createSequence":
        rejectParamsForDDL(params);
        return this.executeCreateSequence(stmt);
      case "alterSequence":
        rejectParamsForDDL(params);
        return this.executeAlterSequence(stmt);
      case "dropSequence":
        rejectParamsForDDL(params);
        return this.executeDropSequence(stmt);
      case "insert":
        return this.executeInsert(stmt, params);
      case "select":
        return this.executeSelect(stmt, params);
      case "setOp":
        return this.executeSetOp(stmt, params);
      case "with":
        return this.executeWith(stmt, params);
      case "update":
        return this.executeUpdate(stmt, params);
      case "delete":
        return this.executeDelete(stmt, params);
      default:
        // Transaction control (begin/commit/rollback) is handled by executeStmtParams before
        // dispatch; it never reaches here.
        throw engineError("syntax_error", "unexpected statement kind");
    }
  }

  // rowsInKeyOrder returns a table's rows in primary-key (encoded byte) order in the visible
  // snapshot, or [] if the table does not exist.
  rowsInKeyOrder(name: string): Row[] {
    return this.readSnap().rowsInKeyOrder(name);
  }

  // executeCreateTable resolves each column's type name, enforces a single primary key
  // across both forms (column-level and the table-level PRIMARY KEY (a, b, ...) constraint —
  // which is implicitly NOT NULL per member), rejects duplicate table and column names, then
  // registers it. Constraint checks mirror PostgreSQL's order (oracle-probed,
  // constraints.md §3): a second primary key traps 42P16 before its members resolve; members
  // resolve left to right (unknown 42703, repeated 42701); then the jed narrowings — the
  // declaration-order rule and the per-member key-type gate — trap 0A000.
  private executeCreateTable(ct: CreateTable): Outcome {
    // The relation namespace is shared between tables and indexes (indexes.md §2), so a
    // CREATE TABLE colliding with either kind is the same 42P07 — PG's "relation" word.
    if (this.relationExists(ct.name)) {
      throw engineError("duplicate_table", "relation already exists: " + ct.name);
    }

    const columns: Column[] = [];
    // pk is the primary-key member ordinals in KEY order (constraints.md §3): the
    // column-level form is the one-member case; the table-level list below records its
    // own order.
    let pk: number[] = [];
    let pkSeen = false;
    for (const def of ct.columns) {
      for (const c of columns) {
        if (c.name.toLowerCase() === def.name.toLowerCase()) {
          throw engineError("duplicate_column", "duplicate column name: " + def.name);
        }
      }
      // Resolve the column type: a built-in scalar, or a user-defined composite referenced by name
      // (spec/design/composite.md §3). An unknown name is 42704. A composite column carries no typmod
      // (the composite's fields carry their own); a type modifier written on a composite column is
      // rejected (0A000). A composite column is storable (the recursive value codec — §4) but never
      // keyable — the PK gate below rejects it 0A000 (§6).
      let colType: Type;
      let decimal: DecimalTypmod | null;
      const ctype = this.compositeType(def.typeName);
      if (def.typeName.endsWith("[]")) {
        // An array column (spec/design/array.md §3). The element type is a scalar or a
        // previously-defined composite (array-of-composite, §12 AC1 — element_type_code 14 + name);
        // a nested-array element and an array typmod (numeric(p,s)[]) stay deferred (0A000).
        const base = def.typeName.slice(0, -2);
        if (def.typeMod !== null) {
          throw engineError("feature_not_supported", "a type modifier on an array type is not supported yet");
        }
        const elemScalar = scalarTypeFromName(base);
        const baseComposite = this.compositeType(base);
        if (elemScalar !== undefined) {
          colType = arrayT(scalarT(elemScalar));
        } else if (baseComposite !== undefined) {
          colType = arrayT(compositeT(baseComposite.name));
        } else {
          throw engineError("undefined_object", "type does not exist: " + base);
        }
        decimal = null;
      } else if (scalarTypeFromName(def.typeName) !== undefined) {
        const [s, d] = resolveTypeAndTypmod(def.typeName, def.typeMod);
        colType = scalarT(s);
        decimal = d;
      } else if (ctype !== undefined) {
        if (def.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier is not supported for composite type " + def.typeName,
          );
        }
        colType = compositeT(ctype.name);
        decimal = null;
      } else {
        throw engineError("undefined_object", "type does not exist: " + def.typeName);
      }
      if (def.primaryKey) {
        // Integers, boolean, and uuid may be a key. uuid is the first non-integer key type (fixed
        // uuid-raw16, spec/design/encoding.md §2.7) and boolean the second (fixed 1-byte bool-byte,
        // §2.9) — both exercised + byte-pinned. The remaining non-integer types' order-preserving
        // key encodings (text §2.4, decimal §2.5, bytea §2.6, interval, float §2.8, composite §2.10)
        // are authored but unexercised, so a text/decimal/bytea/interval/float/composite PRIMARY KEY
        // is a documented 0A000 narrowing (types.md §11/§12/§13, composite.md §6), relaxable in a
        // later in-key slice. timestamp / timestamptz are also allowed — they share the i64
        // int-be-signflip key encoding (exercised + byte-pinned, spec/design/timestamp.md §6).
        if (
          !typeIsInteger(colType) &&
          !typeIsBoolean(colType) &&
          !typeIsUuid(colType) &&
          !typeIsTimestamp(colType) &&
          !typeIsTimestamptz(colType) &&
          !typeIsDate(colType)
        ) {
          throw engineError(
            "feature_not_supported",
            "a " + typeCanonicalName(colType) + " primary key is not supported yet",
          );
        }
        if (pkSeen) {
          throw engineError(
            "invalid_table_definition",
            "multiple primary keys for table " + ct.name + " are not allowed",
          );
        }
        pkSeen = true;
        pk.push(columns.length); // this column's ordinal (pushed below)
      }
      // Classify the DEFAULT by syntactic form (constraints.md §2). A bad default fails at
      // CREATE TABLE either way; NOT NULL is NOT enforced here (notNull=false), so a DEFAULT
      // NULL on a NOT NULL column is accepted and traps 23502 only when applied.
      //   - a bare literal is pre-evaluated + type-coerced to a constant value (the fast-path:
      //     out of range 22003, cross-family 42804, decimal rounded to typmod);
      //   - any other expression is validated (structural pre-walk, then resolved against an
      //     EMPTY scope — a default may not reference a column — then its result type is checked
      //     assignable to the column, 42804) and stored as text for per-row eval.
      let def_default: Value | null = null;
      let def_defaultExpr: DefaultExpr | null = null;
      if (colType.kind === "composite" || colType.kind === "array") {
        // A DEFAULT on a composite- or array-typed column is not supported this slice
        // (composite.md §12 / array.md §12).
        if (def.default !== null) {
          throw engineError(
            "feature_not_supported",
            "a DEFAULT on a composite- or array-typed column is not supported yet",
          );
        }
      } else if (def.default !== null) {
        const sty = colType.scalar;
        if (def.default.expr.kind === "literal") {
          def_default = storeValue(literalToValue(def.default.expr.literal), sty, decimal, false, def.name);
        } else {
          rejectDefaultStructure(def.default.expr);
          const { type: rt } = resolve(Scope.empty(this), def.default.expr, sty, { collecting: false, groupKeys: [], specs: [] }, new ParamTypes());
          if (!assignableTo(rt, sty)) {
            throw typeError(
              `column ${def.name} is of type ${canonicalName(sty)} but default expression is of type ${rtName(rt)}`,
            );
          }
          def_defaultExpr = { exprText: def.default.text, expr: def.default.expr };
        }
      }
      columns.push({
        name: def.name,
        type: colType,
        decimal,
        primaryKey: def.primaryKey,
        notNull: def.primaryKey || def.notNull, // PRIMARY KEY ⇒ NOT NULL
        default: def_default,
        defaultExpr: def_defaultExpr,
      });
    }

    // Table-level PRIMARY KEY (a, b, ...) constraints (constraints.md §3). Check order
    // mirrors PostgreSQL (oracle-probed): a second primary key is 42P16 before its
    // members resolve; members resolve left to right (42703 unknown, 42701 repeated).
    // The LIST order is the KEY order — it may differ from declaration order (the v5
    // catalog persists the ordinal list; the old 0A000 narrowing is lifted). The
    // per-member key-type gate (0A000) remains.
    for (const pkList of ct.tablePks) {
      if (pkSeen) {
        throw engineError(
          "invalid_table_definition",
          "multiple primary keys for table " + ct.name + " are not allowed",
        );
      }
      pkSeen = true;
      const indices: number[] = [];
      for (const name of pkList) {
        const lower = name.toLowerCase();
        const idx = columns.findIndex((c) => c.name.toLowerCase() === lower);
        if (idx < 0) {
          throw engineError("undefined_column", "column " + name + " named in key does not exist");
        }
        if (indices.includes(idx)) {
          throw engineError(
            "duplicate_column",
            "column " + name + " appears twice in primary key constraint",
          );
        }
        indices.push(idx);
      }
      for (const i of indices) {
        const ty = columns[i]!.type;
        if (
          !typeIsInteger(ty) &&
          !typeIsBoolean(ty) &&
          !typeIsUuid(ty) &&
          !typeIsTimestamp(ty) &&
          !typeIsTimestamptz(ty) &&
          !typeIsDate(ty)
        ) {
          throw engineError(
            "feature_not_supported",
            "a " + typeCanonicalName(ty) + " primary key is not supported yet",
          );
        }
        columns[i]!.primaryKey = true;
        columns[i]!.notNull = true; // PRIMARY KEY ⇒ NOT NULL, per member
      }
      pk = indices;
    }

    // UNIQUE constraints (constraints.md §5.1): resolve members in textual definition
    // order, AFTER the PRIMARY KEY constraints and BEFORE any CHECK validates (PG's
    // order, oracle-probed — transformIndexConstraint runs first). Each member must
    // exist (42703, PG's "named in key" wording), appear once (42701), and be of a
    // key-encodable type (0A000 — the same narrowing as a PK member / index key column;
    // unlike a PK member it stays nullable). Folding + naming happen LAST (after check
    // naming), mirroring PG's index_create-at-execution timing.
    const runiques: { name: string | null; cols: number[] }[] = [];
    for (const u of ct.uniques) {
      const indices: number[] = [];
      for (const cname of u.columns) {
        const lower = cname.toLowerCase();
        const idx = columns.findIndex((c) => c.name.toLowerCase() === lower);
        if (idx < 0) {
          throw engineError("undefined_column", "column " + cname + " named in key does not exist");
        }
        if (indices.includes(idx)) {
          throw engineError(
            "duplicate_column",
            "column " + cname + " appears twice in unique constraint",
          );
        }
        indices.push(idx);
      }
      for (const i of indices) {
        const ty = columns[i]!.type;
        if (
          !typeIsInteger(ty) &&
          !typeIsBoolean(ty) &&
          !typeIsUuid(ty) &&
          !typeIsTimestamp(ty) &&
          !typeIsTimestamptz(ty) &&
          !typeIsDate(ty)
        ) {
          throw engineError(
            "feature_not_supported",
            "a " + typeCanonicalName(ty) + " unique constraint member is not supported yet",
          );
        }
      }
      runiques.push({ name: u.name, cols: indices });
    }

    // CHECK constraints (constraints.md §4). All validation runs first, in textual
    // definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
    // oracle-probed); naming follows in a second pass, so a 42703 in a later check fires
    // before a 42710 between earlier ones. Resolution needs a catalog Table, so build it
    // now (checks attach below, before putTable).
    const table: Table = { name: ct.name, columns, pk, checks: [], indexes: [], fks: [] };
    for (const def of ct.checks) {
      // Structural rejections first (a single pre-walk — a documented micro-order
      // divergence from PG, which interleaves them with name/type resolution): subquery
      // 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
      rejectCheckStructure(def.expr);
      const scope = Scope.single(this, table);
      const { type } = resolve(scope, def.expr, null, { collecting: false, groupKeys: [], specs: [] }, new ParamTypes());
      if (type.kind !== "bool" && type.kind !== "null") {
        throw typeError("argument of CHECK must be boolean");
      }
    }
    // Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
    // used as written; a derived name is built from the LOWERCASED table/column names —
    // `<table>_<col>_check` when the expression references exactly one distinct column,
    // else `<table>_check` — suffixed with the smallest positive integer that frees it. A
    // collision (case-insensitive, PG folds) is 42710; derived names never yield to a
    // later explicit one (oracle-probed).
    const checks: CheckConstraint[] = [];
    const nameTaken = (name: string): boolean =>
      checks.some((c) => c.name.toLowerCase() === name.toLowerCase());
    for (const def of ct.checks) {
      let name: string;
      if (def.name !== null) {
        if (nameTaken(def.name)) {
          throw engineError(
            "duplicate_object",
            "constraint " + def.name + " for relation " + table.name + " already exists",
          );
        }
        name = def.name;
      } else {
        const cols = checkReferencedColumns(def.expr, columns);
        const base =
          cols.length === 1
            ? table.name.toLowerCase() + "_" + columns[cols[0]!]!.name.toLowerCase() + "_check"
            : table.name.toLowerCase() + "_check";
        name = base;
        for (let suffix = 1; nameTaken(name); suffix++) name = base + suffix.toString();
      }
      checks.push({ name, exprText: def.text, expr: def.expr });
    }
    // Evaluation (and on-disk) order: ascending byte order of the lowercased name
    // (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
    checks.sort((a, b) => {
      const an = a.name.toLowerCase();
      const bn = b.name.toLowerCase();
      return an < bn ? -1 : an > bn ? 1 : 0;
    });
    table.checks = checks;

    // UNIQUE fold + naming (constraints.md §5.2/§5.3, PG-probed). Fold first: a
    // constraint whose member list equals the primary key's (same order) creates
    // nothing; identical lists fold into the first occurrence, the surviving name being
    // the first explicitly-named one's. Then each survivor names its backing index in
    // textual order: an explicit name checks the relation namespace (42P07 — existing
    // relations, the table being created, and the statement's earlier indexes) before
    // the table's constraint names (42710); a derived `<table>_<cols>_key` suffix-walks
    // past BOTH namespaces.
    const sameCols = (a: number[], b: number[]): boolean =>
      a.length === b.length && a.every((v, i) => v === b[i]);
    const survivors: { name: string | null; cols: number[] }[] = [];
    for (const ru of runiques) {
      if (sameCols(ru.cols, table.pk)) continue;
      const existing = survivors.find((sv) => sameCols(sv.cols, ru.cols));
      if (existing) {
        if (existing.name === null) existing.name = ru.name;
        continue;
      }
      survivors.push(ru);
    }
    const relationTaken = (n: string): boolean =>
      this.relationExists(n) ||
      table.name.toLowerCase() === n.toLowerCase() ||
      table.indexes.some((ix) => ix.name.toLowerCase() === n.toLowerCase());
    const checkNameTaken = (n: string): boolean =>
      table.checks.some((c) => c.name.toLowerCase() === n.toLowerCase());
    for (const ru of survivors) {
      let name: string;
      if (ru.name !== null) {
        if (relationTaken(ru.name)) {
          throw engineError("duplicate_table", "relation already exists: " + ru.name);
        }
        if (checkNameTaken(ru.name)) {
          throw engineError(
            "duplicate_object",
            "constraint " + ru.name + " for relation " + table.name + " already exists",
          );
        }
        name = ru.name;
      } else {
        let base = table.name.toLowerCase();
        for (const i of ru.cols) base += "_" + columns[i]!.name.toLowerCase();
        base += "_key";
        name = base;
        for (let suffix = 1; relationTaken(name) || checkNameTaken(name); suffix++) {
          name = base + suffix.toString();
        }
      }
      // Insert in catalog (ascending lowercased-name) order — indexes.md §6.
      const nameKey = name.toLowerCase();
      let pos = table.indexes.findIndex((ix) => ix.name.toLowerCase() > nameKey);
      if (pos < 0) pos = table.indexes.length;
      table.indexes.splice(pos, 0, { name, columns: ru.cols, unique: true, kind: "btree" });
    }

    // FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
    // constraints (PG's order), each in textual definition order: resolve the local columns
    // (42703/42701) against this table; look up the parent (42P01, or the table itself for a
    // self-reference); resolve the referenced columns (default to the parent PK, 42704 if it has
    // none); check the arity (42830); name the constraint (explicit collision 42710, else derive
    // `<table>_<cols>_fkey` with a suffix walk through the constraint namespace); reject the
    // unsupported write-actions (0A000); require the referenced columns to be the parent PK or a
    // UNIQUE set (42830); and require same-type pairing (42804, stricter than PG). An FK owns no
    // B-tree — enforcement probes the parent at every write (§6.4/§6.5).
    const resolvedFks: ForeignKey[] = [];
    for (const fk of ct.fks) {
      // 1. Local (referencing) columns into this table.
      const local: number[] = [];
      for (const cname of fk.columns) {
        const idx = columnIndex(table, cname);
        if (idx < 0) {
          throw engineError("undefined_column", "column " + cname + " named in key does not exist");
        }
        if (local.includes(idx)) {
          throw engineError("duplicate_column", "column " + cname + " appears twice in foreign key constraint");
        }
        local.push(idx);
      }
      // 2. Parent table — a self-reference resolves against the in-progress definition.
      const selfRef = fk.refTable.toLowerCase() === table.name.toLowerCase();
      let parent: Table;
      if (selfRef) {
        parent = table;
      } else {
        const found = this.table(fk.refTable);
        if (found === undefined) {
          throw engineError("undefined_table", "table does not exist: " + fk.refTable);
        }
        parent = found;
      }
      // 3. Referenced columns into the parent (default to the parent's primary key).
      let refs: number[];
      if (fk.refColumns === null) {
        if (parent.pk.length === 0) {
          // Omitting the referenced list defaults to the parent's PRIMARY KEY; a parent without
          // one is 42704 (PG's code here — undefined_object — even when the parent has a UNIQUE),
          // distinct from the explicit-no-match 42830.
          throw engineError("undefined_object", "there is no primary key for referenced table " + parent.name);
        }
        refs = [...parent.pk];
      } else {
        refs = [];
        for (const cname of fk.refColumns) {
          const idx = columnIndex(parent, cname);
          if (idx < 0) {
            throw engineError("undefined_column", "column " + cname + " named in key does not exist");
          }
          if (refs.includes(idx)) {
            throw engineError("duplicate_column", "column " + cname + " appears twice in foreign key constraint");
          }
          refs.push(idx);
        }
      }
      // 4. Referencing/referenced count must agree.
      if (local.length !== refs.length) {
        throw engineError("invalid_foreign_key", "number of referencing and referenced columns for foreign key disagree");
      }
      // 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
      const nameTakenFk = (n: string): boolean =>
        table.checks.some((c) => c.name.toLowerCase() === n.toLowerCase()) ||
        resolvedFks.some((f) => f.name.toLowerCase() === n.toLowerCase());
      let fkName: string;
      if (fk.name !== null) {
        if (nameTakenFk(fk.name)) {
          throw engineError("duplicate_object", "constraint " + fk.name + " for relation " + table.name + " already exists");
        }
        fkName = fk.name;
      } else {
        let base = table.name.toLowerCase();
        for (const i of local) base += "_" + table.columns[i]!.name.toLowerCase();
        base += "_fkey";
        fkName = base;
        for (let suffix = 1; nameTakenFk(fkName); suffix++) fkName = base + suffix.toString();
      }
      // 6. Reject the unsupported write-actions (§6.6).
      const onDelete = fkAction(fk.onDelete, "DELETE");
      const onUpdate = fkAction(fk.onUpdate, "UPDATE");
      // 7. The referenced columns must be the parent's PK or a UNIQUE set (§6.2).
      const refSet = sortedUnique(refs);
      const matchesUnique =
        (parent.pk.length > 0 && sameSet(sortedUnique(parent.pk), refSet)) ||
        parent.indexes.some((i) => i.unique && sameSet(sortedUnique(i.columns), refSet));
      if (!matchesUnique) {
        throw engineError("invalid_foreign_key", "there is no unique constraint matching given keys for referenced table " + parent.name);
      }
      // 8. Same-type pairing (§6.2). Because the referenced columns are a PK/UNIQUE key they are
      // key-encodable, so a same-typed local column is key-encodable too — no separate 0A000 type
      // gate is needed.
      for (let p = 0; p < local.length; p++) {
        const li = local[p]!;
        const ri = refs[p]!;
        if (!fkTypesEqual(table.columns[li]!.type, parent.columns[ri]!.type)) {
          throw engineError(
            "datatype_mismatch",
            "foreign key constraint " + fkName + " cannot be implemented: key columns " +
              table.columns[li]!.name + " and " + parent.columns[ri]!.name +
              " are of incompatible types: " + typeCanonicalName(table.columns[li]!.type) +
              " and " + typeCanonicalName(parent.columns[ri]!.type),
          );
        }
      }
      resolvedFks.push({ name: fkName, columns: local, refTable: parent.name, refColumns: refs, onDelete, onUpdate });
    }
    // Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
    resolvedFks.sort((a, b) => {
      const an = a.name.toLowerCase();
      const bn = b.name.toLowerCase();
      return an < bn ? -1 : an > bn ? 1 : 0;
    });
    table.fks = resolvedFks;

    this.putTable(table);
    // The table is brand new (no rows), so each backing index store starts empty.
    for (const ix of table.indexes) {
      this.working().putIndexStore(
        ix.name.toLowerCase(),
        new TableStore(this.pageSize - 12, []), // 12 = PAGE_HEADER
      );
    }
    // DDL touches no rows and evaluates no expressions: zero cost.
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // resolveChecks resolves a table's CHECK constraints for a write statement: each stored
  // expression against a one-relation scope, in the catalog's (evaluation/name) order.
  // Cannot fail for a catalog produced by CREATE TABLE or a well-formed file (both
  // validated); a hand-corrupted expression surfaces its natural resolve error.
  private resolveChecks(table: Table): NamedCheck[] {
    if (table.checks.length === 0) return [];
    const scope = Scope.single(this, table);
    return table.checks.map((c) => ({
      name: c.name,
      node: resolve(scope, c.expr, null, { collecting: false, groupKeys: [], specs: [] }, new ParamTypes()).node,
    }));
  }

  // resolveDefaultExprs resolves each column's EXPRESSION default (constraints.md §2) to an
  // RExpr, once per INSERT statement — insertRows (and the VALUES DEFAULT-keyword
  // materialization) evaluate it per omitted/DEFAULT slot. Returns a slot per column (parallel
  // to table.columns): the resolved node for an expression default, null for a column with a
  // constant default or no default. The default resolves against an EMPTY scope (no columns; a
  // column reference was rejected 0A000 at CREATE TABLE) with the column's type as the operand
  // hint.
  private resolveDefaultExprs(table: Table): (RExpr | null)[] {
    return table.columns.map((col) => {
      if (col.defaultExpr === null) return null;
      return resolve(Scope.empty(this), col.defaultExpr.expr, typeScalar(col.type), { collecting: false, groupKeys: [], specs: [] }, new ParamTypes()).node;
    });
  }

  // evalDefault is the value an omitted column or a DEFAULT value slot takes (constraints.md §2):
  // the column's pre-evaluated constant (col.default, or NULL when it has none), OR — for an
  // expression default — the resolved RExpr evaluated against an empty row through the
  // per-statement seam/clock (rng) and metered (operator_eval per node). Reused by the VALUES
  // materialization (a DEFAULT keyword) and insertRows (an omitted column), sharing ONE StmtRng
  // so a multi-row DEFAULT uuidv7() stays monotonic. defaultRExpr is null for a constant/no default.
  private evalDefault(col: Column, defaultRExpr: RExpr | null, rng: StmtRng, meter: Meter): Value {
    if (defaultRExpr === null) return col.default ?? nullValue();
    meter.guard();
    const env: EvalEnv = {
      params: [],
      outer: [],
      runSubquery: (p, o) => this.execQueryPlan(p, o, [], EMPTY_CTE_CTX),
      seam: this.seam,
      rng,
      ctes: EMPTY_CTE_CTX,
      exec: this,
    };
    return evalExpr(defaultRExpr, [], env, meter);
  }

  // executeDropTable removes the table's definition and its row store from the catalog
  // (both keyed by the lower-cased name). A table that does not exist is the same 42P01
  // the DML paths raise — there is no IF EXISTS this slice (spec/design/grammar.md §13).
  // Like CREATE TABLE it touches no rows and evaluates no expression tree (the store is
  // discarded wholesale), so it accrues zero cost.
  private executeDropTable(dt: DropTable): Outcome {
    if (!this.table(dt.name)) {
      // An index's name is the wrong object kind (42809 — indexes.md §2, PG-probed);
      // anything else is the missing-table 42P01 the DML paths raise.
      if (this.findIndex(dt.name)) {
        throw engineError("wrong_object_type", dt.name + " is not a table");
      }
      throw engineError("undefined_table", "table does not exist: " + dt.name);
    }
    // A table referenced by ANOTHER table's FOREIGN KEY cannot be dropped (2BP01 — there is no
    // DROP TABLE … CASCADE; a self-reference does not block — spec/design/constraints.md §6.10).
    const detail = this.readSnap().fkDependent(dt.name);
    if (detail !== null) {
      const canonical = this.table(dt.name)?.name ?? dt.name;
      throw engineError(
        "dependent_objects_still_exist",
        "cannot drop table " + canonical + " because other objects depend on it: " + detail,
      );
    }
    this.working().removeTable(dt.name.toLowerCase());
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // findIndex finds the table owning the named index in the visible snapshot
  // (case-insensitive).
  private findIndex(name: string): [string, IndexDef] | null {
    return this.readSnap().findIndex(name);
  }

  // relationExists reports whether name is taken in the shared relation namespace (a
  // table OR an index OR a sequence — spec/design/indexes.md §2, sequences.md §2),
  // case-insensitively.
  private relationExists(name: string): boolean {
    return (
      this.table(name) !== undefined ||
      this.findIndex(name) !== null ||
      this.readSnap().sequence(name) !== undefined
    );
  }

  // fkProbeHits reports whether the parent currently holds the key/prefix `probe` (committed +
  // working state) — the child-side foreign-key existence test (spec/design/constraints.md §6.4).
  // `parentTable` is the referenced table's name. Unmetered, like the PK/UNIQUE probes (cost.md §3).
  private fkProbeHits(probe: FkProbe, parentTable: string): boolean {
    if (probe.kind === "pk") {
      return this.readSnap().store(parentTable).get(probe.bytes) !== undefined;
    }
    return this.readSnap().indexStore(probe.index).rangeEntries(uniqueProbeBound(probe.prefix)).length > 0;
  }

  // fkChildReferences reports whether any row of `childTable` references the parent tuple `target`
  // (the parent key bytes, in the byte space fkProbe produces) via `fk` — the reverse of the
  // child-side probe, a full scan since child FK columns are not index-backed
  // (spec/design/constraints.md §6.5). MATCH SIMPLE: a child row with any NULL FK column references
  // nothing. Rows whose storage key is in `exclude` are skipped — the END STATE for a
  // self-reference, whose child IS the table being mutated (so its deleted/updated rows must not
  // count). `parent` is the referenced table's catalog. Unmetered validation.
  private fkChildReferences(
    childTable: string,
    fk: ForeignKey,
    parent: Table,
    target: Uint8Array,
    exclude: Set<string>,
  ): boolean {
    for (const e of this.readSnap().store(childTable).entriesInKeyOrder()) {
      if (exclude.has(e.key.join(","))) continue;
      const probe = fkProbe(fk, parent, e.row, fk.columns);
      if (probe !== null && bytesEq(fkProbeBytes(probe), target)) return true;
    }
    return false;
  }

  // fkReferencers returns every (child table name, FK) pair in the visible snapshot whose FK
  // references `parentName` (case-insensitive), including a self-reference — the inbound FKs a
  // parent DELETE/UPDATE must not strand (spec/design/constraints.md §6.5). Sorted by (lowercased
  // child table, FK name) for a deterministic report order. The FK objects are the snapshot's
  // (the caller probes stores without mutating the catalog).
  private fkReferencers(parentName: string): { childTable: string; fk: ForeignKey }[] {
    const snap = this.readSnap();
    const key = parentName.toLowerCase();
    const out: { childTable: string; fk: ForeignKey }[] = [];
    const tkeys = [...snap.tables.keys()].sort();
    for (const tk of tkeys) {
      const t = snap.tables.get(tk)!;
      for (const fk of t.fks) {
        if (fk.refTable.toLowerCase() === key) out.push({ childTable: t.name, fk });
      }
    }
    return out;
  }

  // executeCreateIndex analyzes and runs a CREATE INDEX (spec/design/indexes.md §2).
  // Validation mirrors PostgreSQL's order (oracle-probed): the table must exist (42P01);
  // each key column, in list order, must exist (42703) and be of a key-encodable type
  // (0A000 — the same narrowing as a PRIMARY KEY member); then an explicit name is
  // checked against the shared relation namespace (42P07), or an omitted name derives
  // PG's choice — the lowercased <table>_<col>..._idx with the smallest free suffix. The
  // index is then built by scanning the table once: page_read per node + storage_row_read
  // per row (the metered build scan — cost.md §3); maintenance thereafter is unmetered.
  private executeCreateIndex(ci: CreateIndex): Outcome {
    const table = this.table(ci.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ci.table);
    }
    const tableKey = table.name.toLowerCase();
    const columns = table.columns;
    // Resolve the access method (spec/design/gin.md §3): undefined / "btree" is the ordered
    // B-tree, "gin" a GIN inverted index; an unknown method is 42704. Resolved here (not in the
    // parser) so the error is the resolve-time undefined_object, after the table-exists check.
    const method = (ci.using ?? "btree").toLowerCase();
    let kind: "btree" | "gin";
    if (method === "btree") kind = "btree";
    else if (method === "gin") kind = "gin";
    else throw engineError("undefined_object", "access method does not exist: " + ci.using);
    const cols: number[] = [];
    for (const name of ci.columns) {
      const idx = columnIndex(table, name);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + name);
      }
      const ty = columns[idx]!.type;
      if (kind === "gin") {
        // GIN needs an operator class for the column type: only an array has one (else 42704),
        // and this slice only the integer element types (else 0A000) — spec/design/gin.md §3.
        if (ty.kind !== "array") {
          throw engineError(
            "undefined_object",
            "data type " + typeCanonicalName(ty) + " has no default operator class for access method gin",
          );
        }
        if (!typeIsInteger(ty.elem)) {
          throw engineError(
            "feature_not_supported",
            "a gin index on " + typeCanonicalName(ty) + " is not supported yet",
          );
        }
      } else if (
        !typeIsInteger(ty) &&
        !typeIsBoolean(ty) &&
        !typeIsUuid(ty) &&
        !typeIsTimestamp(ty) &&
        !typeIsTimestamptz(ty)
      ) {
        throw engineError(
          "feature_not_supported",
          "a " + typeCanonicalName(ty) + " index column is not supported yet",
        );
      }
      // A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
      cols.push(idx);
    }
    // GIN narrowings this slice (spec/design/gin.md §3): no uniqueness (undefined for an inverted
    // index) and a single column only — both deferred 0A000.
    if (kind === "gin") {
      if (ci.unique) {
        throw engineError("feature_not_supported", "access method gin does not support unique indexes");
      }
      if (cols.length !== 1) {
        throw engineError("feature_not_supported", "a multi-column gin index is not supported yet");
      }
    }
    let name: string;
    if (ci.name !== null) {
      if (this.relationExists(ci.name)) {
        throw engineError("duplicate_table", "relation already exists: " + ci.name);
      }
      name = ci.name;
    } else {
      // PG's ChooseIndexName (probed): lowercased table + every listed column name
      // (list order, duplicates included) + "idx", then the smallest free suffix.
      let base = tableKey;
      for (const cn of ci.columns) base += "_" + cn.toLowerCase();
      base += "_idx";
      name = base;
      for (let suffix = 1; this.relationExists(name); suffix++) name = base + suffix.toString();
    }

    // The build scan (cost.md §3): page_read per table-tree node + storage_row_read per
    // row, with the indexed columns as the touched set (fixed-width — the chain/decompress
    // terms are structurally zero). An empty table charges 0. The entries are computed
    // here, against the pre-index store; the writes below are unmetered.
    const meter = new Meter(this.maxCost);
    const mask = columns.map(() => false);
    for (const c of cols) mask[c] = true;
    const def: IndexDef = { name, columns: cols, unique: ci.unique, kind };
    const store = this.readSnap().store(ci.table);
    const { entries: stored, pages: nodes, slabs } = store.scanWithUnits(mask);
    meter.charge(COSTS.pageRead * BigInt(nodes) + COSTS.valueDecompress * BigInt(slabs));
    const entries: Uint8Array[] = [];
    // A UNIQUE build verifies the existing rows before the index is registered
    // (indexes.md §8): two rows sharing a fully-non-NULL key tuple — i.e. an exempt-free
    // prefix — trap 23505 and create nothing. Unmetered validation (cost.md §3).
    const seenPrefixes = new Set<string>();
    for (const e of stored) {
      meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
      meter.charge(COSTS.storageRowRead);
      if (def.unique) {
        const prefix = indexPrefixKey(columns, def, e.row);
        if (prefix !== null) {
          const k = prefix.join(",");
          if (seenPrefixes.has(k)) {
            throw engineError(
              "unique_violation",
              "duplicate key value violates unique constraint: " + def.name,
            );
          }
          seenPrefixes.add(k);
        }
      }
      entries.push(...indexEntryKeys(columns, def, e.key, e.row));
    }
    meter.guard();

    const nameKey = def.name.toLowerCase();
    this.working().putIndex(tableKey, def, this.pageSize);
    const istore = this.working().indexStore(nameKey);
    // Insert sorted by entry key (indexes.md §1): every insert is then a right-edge append,
    // so the built tree packs ~full instead of splintering under the storage-key order the
    // scan produced (random in entry-key space). Part of the byte contract — the sort fixes
    // the built tree's shape across cores.
    entries.sort(compareBytes);
    for (const ek of entries) {
      if (!istore.insert(ek, [])) {
        throw new Error("index entry keys are unique (storage-key suffix)");
      }
    }
    return { kind: "statement", cost: meter.accrued, rowsAffected: null };
  }

  // executeDropIndex runs a DROP INDEX (spec/design/indexes.md §2): a table's name is
  // 42809, a missing one 42704. A pure catalog edit — zero cost, like DROP TABLE.
  private executeDropIndex(di: DropIndex): Outcome {
    if (this.table(di.name)) {
      throw engineError("wrong_object_type", di.name + " is not an index");
    }
    const found = this.findIndex(di.name);
    if (!found) {
      throw engineError("undefined_object", "index does not exist: " + di.name);
    }
    this.working().removeIndex(found[0], di.name.toLowerCase());
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeCreateType analyzes and runs a CREATE TYPE (spec/design/composite.md): reject a
  // duplicate type name (42710), resolve each field's type (a built-in scalar, or a
  // *previously-defined* composite — 42704 if unknown; no self- or forward-reference), reject a
  // duplicate field name (42701), then register the composite type in the catalog. Named
  // composites only.
  private executeCreateType(ct: CreateType): Outcome {
    if (this.compositeType(ct.name) !== undefined) {
      throw engineError("duplicate_object", "type " + ct.name + " already exists");
    }
    const fields: CompositeField[] = [];
    for (const f of ct.fields) {
      for (const g of fields) {
        if (g.name.toLowerCase() === f.name.toLowerCase()) {
          throw engineError("duplicate_column", "attribute " + f.name + " specified more than once");
        }
      }
      let fty: Type;
      let fdecimal: DecimalTypmod | null = null;
      if (f.typeName.endsWith("[]")) {
        // An array-typed field (spec/design/array.md §12 — the mirror of an array-of-composite
        // element). The element is a scalar or a previously-defined composite (element_type_code
        // 14 + name on disk); a nested-array element and an array typmod stay deferred (0A000),
        // exactly as for an array column.
        const base = f.typeName.slice(0, -2);
        if (f.typeMod !== null) {
          throw engineError("feature_not_supported", "a type modifier on an array type is not supported yet");
        }
        const elemScalar = scalarTypeFromName(base);
        const baseComposite = this.compositeType(base);
        if (elemScalar !== undefined) {
          fty = arrayT(scalarT(elemScalar));
        } else if (baseComposite !== undefined) {
          fty = arrayT(compositeT(baseComposite.name));
        } else {
          throw engineError("undefined_object", "type does not exist: " + base);
        }
      } else if (scalarTypeFromName(f.typeName) !== undefined) {
        const [s, d] = resolveTypeAndTypmod(f.typeName, f.typeMod);
        fty = scalarT(s);
        fdecimal = d;
      } else if (this.compositeType(f.typeName) !== undefined) {
        if (f.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier is not supported for composite type " + f.typeName,
          );
        }
        fty = compositeT(f.typeName);
      } else {
        throw engineError("undefined_object", "type does not exist: " + f.typeName);
      }
      fields.push({ name: f.name, type: fty, decimal: fdecimal, notNull: f.notNull });
    }
    // Bound composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A chain of CREATE TYPEs each
    // nesting the previous (`a`, `b AS (x a)`, …) builds unbounded depth across many cheap statements —
    // invisible to the per-statement input-size cap and the parser nesting counter — and every derived
    // recursive walk (codec, comparator, record_out/in, resolveColType) recurses to this depth. Reject
    // at the producer so no over-deep type enters the catalog and every downstream walk stays
    // stack-safe. Fields reference only existing types (each already ≤ MAX_COMPOSITE_DEPTH), so this
    // depth computation's recursion is itself bounded.
    const cache = new Map<string, number>();
    let maxField = 0;
    for (const f of fields) maxField = Math.max(maxField, this.readSnap().compositeTypeDepth(f.type, cache));
    const depth = 1 + maxField;
    if (depth > MAX_COMPOSITE_DEPTH) {
      throw engineError(
        "statement_too_complex",
        `composite type ${ct.name} nesting depth ${depth} exceeds the maximum of ${MAX_COMPOSITE_DEPTH}`,
      );
    }
    this.working().putType({ name: ct.name, fields });
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeDropType analyzes and runs a DROP TYPE (spec/design/composite.md §7). RESTRICT (the only
  // behavior this slice): a missing type is 42704 unless IF EXISTS; if any table column or composite
  // field still references the type, 2BP01; otherwise remove it from the catalog.
  private executeDropType(dt: DropType): Outcome {
    if (this.compositeType(dt.name) === undefined) {
      if (dt.ifExists) return { kind: "statement", cost: 0n, rowsAffected: null };
      throw engineError("undefined_object", "type does not exist: " + dt.name);
    }
    const dep = this.readSnap().compositeDependent(dt.name);
    if (dep !== null) {
      throw engineError(
        "dependent_objects_still_exist",
        "cannot drop type " + dt.name + " because other objects depend on it: " + dep,
      );
    }
    this.working().removeType(dt.name.toLowerCase());
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeCreateSequence analyzes and runs a CREATE SEQUENCE (spec/design/sequences.md). Resolve
  // the option overrides against the INCREMENT sign's type defaults, validate the set (22023),
  // reject a relation-namespace collision (42P07 unless IF NOT EXISTS), and register the sequence.
  private executeCreateSequence(cs: CreateSequence): Outcome {
    if (this.relationExists(cs.name)) {
      if (cs.ifNotExists) return { kind: "statement", cost: 0n, rowsAffected: null };
      throw engineError("duplicate_table", `relation already exists: ${cs.name}`);
    }
    const increment = cs.increment ?? 1n;
    if (increment === 0n) {
      throw engineError("invalid_parameter_value", "INCREMENT must not be zero");
    }
    const cache = cs.cache ?? 1n;
    if (cache < 1n) {
      throw engineError("invalid_parameter_value", `CACHE (${cache}) must be greater than zero`);
    }
    const [defMin, defMax] = defaultSequenceBounds(increment);
    // `{ value: v }` MINVALUE v / `{ value: null }` NO MINVALUE / outer null unset → the default.
    const minValue = cs.minValue !== null && cs.minValue.value !== null ? cs.minValue.value : defMin;
    const maxValue = cs.maxValue !== null && cs.maxValue.value !== null ? cs.maxValue.value : defMax;
    if (minValue > maxValue) {
      throw engineError(
        "invalid_parameter_value",
        `MINVALUE (${minValue}) must be less than MAXVALUE (${maxValue})`,
      );
    }
    // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
    const start = cs.start ?? (increment < 0n ? maxValue : minValue);
    if (start < minValue || start > maxValue) {
      throw engineError(
        "invalid_parameter_value",
        `START value (${start}) cannot be ${
          start < minValue ? `less than MINVALUE` : `greater than MAXVALUE`
        } the ${start < minValue ? minValue : maxValue} value`,
      );
    }
    this.working().putSequence({
      name: cs.name,
      increment,
      minValue,
      maxValue,
      start,
      cache,
      cycle: cs.cycle ?? false,
      lastValue: start,
      isCalled: false,
    });
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeDropSequence analyzes and runs a DROP SEQUENCE (spec/design/sequences.md §1).
  // RESTRICT-only: a missing sequence is 42P01 unless IF EXISTS. No dependency tracking this slice
  // (a plain `DEFAULT nextval('s')` creates none — PG). Multiple names are dropped left to right.
  private executeDropSequence(ds: DropSequence): Outcome {
    for (const name of ds.names) {
      if (this.readSnap().sequence(name) === undefined) {
        if (ds.ifExists) continue;
        throw engineError("undefined_table", `sequence does not exist: ${name}`);
      }
      this.working().removeSequence(name.toLowerCase());
    }
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeAlterSequence analyzes and runs an ALTER SEQUENCE [IF EXISTS] s RESTART [WITH n]
  // (spec/design/sequences.md §4). A missing sequence is 42P01 unless IF EXISTS (then a no-op).
  // RESTART WITH n resets lastValue to n, a bare RESTART to the original start (unchanged); either
  // way isCalled = false, so the next nextval returns that value. A restart value outside
  // [minValue, maxValue] is 22023. Touches no session state (currval/lastval unchanged).
  private executeAlterSequence(as: AlterSequence): Outcome {
    const committed = this.readSnap().sequence(as.name);
    if (committed === undefined) {
      if (as.ifExists) return { kind: "statement", cost: 0n, rowsAffected: null };
      throw engineError("undefined_table", `relation does not exist: ${as.name}`);
    }
    const def = { ...committed };
    const value = as.restartWith ?? def.start;
    if (value < def.minValue || value > def.maxValue) {
      // PG's init_params path: 22023 (distinct from setval's 22003 do_setval path — §4).
      const bound =
        value > def.maxValue
          ? `greater than MAXVALUE (${def.maxValue})`
          : `less than MINVALUE (${def.minValue})`;
      throw engineError("invalid_parameter_value", `RESTART value (${value}) cannot be ${bound}`);
    }
    def.lastValue = value;
    def.isCalled = false;
    this.working().putSequence(def);
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // indexBoundRows executes an index equality bound (cost.md §3 "index-bounded scan"):
  // fetch the rows the equality admits, in index-entry order (= storage-key order among
  // equal values), and return them with the scan's up-front units (pages, slabs) — the
  // index-tree nodes overlapping the prefix range plus, per admitted entry, the
  // table-tree nodes of that row's point lookup and its touched-column decompress slabs.
  // The caller feeds the rows through the same scanSource as any bounded scan (page_read
  // block + per-row storage_row_read). A provably empty bound (NULL / contradictory
  // equalities / out-of-range) returns nothing and charges nothing.
  indexBoundRows(
    tableName: string,
    ib: IndexBound,
    params: Value[],
    outer: Row[],
    mask: boolean[],
  ): { rows: Row[]; pages: number; slabs: number } {
    // Every equality const-source must encode to ONE agreed value: a NULL is 3VL-never-
    // true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
    // integer can equal no stored value — all provably empty.
    let agreed: Uint8Array | null = null;
    for (const src of ib.eqs) {
      const bk = encodeBoundKey(ib.colType, src, params, outer);
      if (bk.kind !== "key") return { rows: [], pages: 0, slabs: 0 };
      if (agreed === null) agreed = bk.key;
      else if (!bytesEq(agreed, bk.key)) return { rows: [], pages: 0, slabs: 0 };
    }
    // The entry-key prefix: the §2.2 present tag + the value's bare key bytes. The range
    // is every entry extending the prefix: [prefix, byte-successor(prefix)).
    const prefix = new Uint8Array(1 + agreed!.length);
    prefix.set(agreed!, 1);
    const b: KeyBound = { lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false };
    const istore = this.readSnap().indexStore(ib.nameKey);
    // The index store has no payload columns, so its mask is empty and its fused scan
    // contributes only the index-tree page_read count (no spill/compress units).
    const iscan = istore.rangeScanWithUnits(b, []);
    let pages = iscan.pages;
    const store = this.readSnap().store(tableName);
    let slabs = 0;
    const rows: Row[] = [];
    for (const e of iscan.entries) {
      // Skip the remaining key components (each self-delimiting — indexes.md §5); the
      // suffix after them is the row's storage key (indexes.md §3).
      let at = prefix.length;
      for (const ty of ib.tailTypes) {
        at += e.key[at] === 0x01 ? 1 : 1 + widthBytes(ty);
      }
      const rowKey = e.key.slice(at);
      const u = store.getWithUnits(rowKey, mask);
      pages += u.pages;
      slabs += u.slabs;
      if (u.row === undefined) throw new Error("an index entry references a stored row");
      rows.push(u.row);
    }
    return { rows, pages, slabs };
  }

  // executeInsert runs an INSERT whose rows come from a VALUES list or a SELECT (grammar.md
  // §12 / §24). An optional column list names the target columns (unknown → 42703, duplicate →
  // 42701); an unlisted column, or a DEFAULT keyword slot, takes the column's stored default
  // else NULL. Each value is type-checked (NULL into NOT NULL traps 23502; an integer outside the
  // column type's range traps 22003 — CLAUDE.md §8); a duplicate primary key traps 23505. An
  // INSERT is two-phase / all-or-nothing, mirroring UPDATE: every row is validated — including its
  // storage key — before any row is inserted, so a mid-batch failure stores nothing. The two
  // sources differ only in where the candidate rows come from and in cost: VALUES is zero
  // (literals + constant defaults), SELECT is the embedded query's accrued cost. The SELECT
  // source additionally validates output arity (42601) and per-column type assignability (42804)
  // up front, before any row is produced — so both fire even over an empty source.
  private executeInsert(ins: Insert, params: Value[]): Outcome {
    const table = this.table(ins.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ins.table);
    }
    const store = this.working().store(ins.table);
    // The key members in key order — one for a single-column PK, several for a composite
    // (constraints.md §3), empty for a no-PK (rowid) table.
    const pk = pkIndices(table);
    // The CHECK constraints, resolved once per statement in evaluation (name) order;
    // insertRows evaluates them per candidate row (constraints.md §4.4).
    const checks = this.resolveChecks(table);
    // Each column's EXPRESSION default, resolved once per statement (constraints.md §2);
    // applied per omitted column / DEFAULT slot, sharing one per-statement StmtRng.
    const defaultExprs = this.resolveDefaultExprs(table);
    const stmtRng = new StmtRng();

    // Resolve the optional column list once. provided[i] >= 0 means table column i takes that
    // value position in each row; -1 means column i is omitted (its default, else NULL). With no
    // list it is the identity over all columns. arity is how many values each row must carry (for
    // a SELECT source, how many columns it must project).
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

    if (ins.source.kind === "select") {
      // SELECT source (§24). Plan the source query, then resolve the RETURNING projection
      // (PostgreSQL's analysis order — both precede any execution), threading ONE ParamTypes
      // so a $N shared by the source and the RETURNING list unifies statement-wide (api.md
      // §5). The source returns OWNED rows, so a self-insert (INSERT INTO t SELECT ... FROM
      // t) reads the pre-insert snapshot, then writes.
      const ptypes = new ParamTypes();
      const plan = this.planQuery(ins.source.select, null, [], ptypes);
      const ret = ins.returning !== null ? this.resolveReturning(table, ins.returning, false, ptypes) : null;
      const bound = bindParams(params, ptypes.finalize());
      const meter = new Meter(this.maxCost);
      const foldCost = { value: 0n };
      this.foldUncorrelatedInPlan(plan, bound, EMPTY_CTE_CTX, foldCost);
      // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
      // pre-statement snapshot (grammar.md §32).
      if (ret !== null) {
        ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, EMPTY_CTE_CTX, foldCost));
      }
      meter.charge(foldCost.value);
      const q = this.execQueryPlan(plan, [], bound, EMPTY_CTE_CTX);
      // Arity: the SELECT's output column count must match the target — checked before any row is
      // produced, so it fires even when the source returns zero rows.
      if (q.columnNames.length !== arity) {
        const noun = arity === 1 ? "column" : "columns";
        throw engineError(
          "syntax_error",
          `INSERT into table ${table.name} has ${arity} target ${noun} but SELECT produces ${q.columnNames.length}`,
        );
      }
      // Type-assignability, the up-front PostgreSQL gate (§24): each projected column's TYPE must
      // be assignable to its target column. Fires even at zero rows (the difference from per-row
      // checking). The per-row storeValue in insertRows then still range-checks values (22003)
      // and enforces NOT NULL.
      for (let i = 0; i < n; i++) {
        const p = provided[i]!;
        if (p < 0) continue;
        const col = table.columns[i]!;
        // INSERT ... SELECT into a composite column lands in a later slice (the VALUES + ROW(…)
        // path is S3 — spec/design/composite.md §12).
        if (col.type.kind === "composite") {
          throw engineError(
            "feature_not_supported",
            "INSERT ... SELECT into composite column " + col.name + " is not supported yet",
          );
        }
        if (col.type.kind === "array") {
          throw engineError(
            "feature_not_supported",
            "INSERT ... SELECT into array column " + col.name + " is not supported yet",
          );
        }
        if (!assignableTo(q.columnTypes[p]!, col.type.scalar)) {
          throw typeError(
            `column ${col.name} is of type ${typeCanonicalName(col.type)} but expression is of type ${rtName(q.columnTypes[p]!)}`,
          );
        }
      }
      // Cost = the embedded SELECT's accrued cost (§24) plus the disposition plan's
      // compression attempts for over-RECORD_MAX rows (value_compress, cost.md §3) plus the
      // RETURNING projection; storing the rows themselves stays unmetered. One meter keeps
      // one ceiling over the whole statement.
      meter.charge(q.cost);
      const returned = this.insertRows(table, store, pk, checks, defaultExprs, stmtRng, provided, q.rows, ret?.nodes ?? null, bound, meter);
      return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, q.rows.length, meter.accrued);
    }

    // VALUES source. A $N in a VALUES slot is typed as its TARGET COLUMN's type. Collect those
    // types across every row (a $N reused under two columns unifies; spec/design/api.md §5), then
    // bind the supplied values up front so a bad bind fails before any row is stored.
    const rowsIn = ins.source.rows;
    const ptypes = new ParamTypes();
    for (const values of rowsIn) {
      if (values.length !== arity) {
        const which = ins.columns !== null ? "target columns are" : "columns are";
        throw engineError(
          "syntax_error",
          `INSERT row has ${values.length} values but ${arity} ${which} expected for table ${table.name}`,
        );
      }
      for (let i = 0; i < n; i++) {
        const p = provided[i]!;
        if (p >= 0 && p < values.length) {
          const iv = values[p]!;
          // A top-level $N slot takes its target column's scalar type; a composite-column param
          // stays untyped (42P18 at finalize this slice — materializeInsertValue handles ROW(…)).
          const ct = table.columns[i]!.type;
          if (iv.kind === "param" && ct.kind === "scalar") {
            ptypes.note(iv.index - 1, ct.scalar);
          }
        }
      }
    }
    // Resolve the RETURNING projection after the source (PostgreSQL's analysis order) and
    // before binding/execution — a 42703 here beats a would-be 23505 (grammar.md §32).
    const ret = ins.returning !== null ? this.resolveReturning(table, ins.returning, false, ptypes) : null;
    const bound = bindParams(params, ptypes.finalize());

    // INSERT ... VALUES reads no rows; with only literal values and constant defaults it
    // evaluates no expression tree (leaves), so a plain fully-inline insert still costs zero. An
    // EXPRESSION default (DEFAULT uuidv7()) evaluates a tree per application — operator_eval per
    // node — the documented exception (constraints.md §2, like CHECK). Other metered work: the
    // disposition plan's compression attempts for over-RECORD_MAX rows (value_compress) and the
    // RETURNING projection. The meter is created here (before materialization) so a
    // DEFAULT-keyword expression default charges it too.
    const meter = new Meter(this.maxCost);

    // Materialize each row into its value-position-indexed candidates (length arity, checked
    // above), resolving each slot: a literal, a bound $N, or a DEFAULT keyword → that column's
    // default (a constant, or its expression evaluated for this row through the shared stmtRng).
    // The shared insertRows then builds the declaration-order row and applies OMITTED defaults.
    const rows: Value[][] = [];
    for (const values of rowsIn) {
      const rv: Value[] = new Array(arity);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        if (p >= 0) {
          const iv = values[p]!;
          // DEFAULT at the top level → the column's default (constant or per-row expression). A
          // ROW(…) / literal / $N slot is materialized against the column's resolved ColType
          // (composite-aware — composite.md §1/§4); coerceForStore in insertRows then range-checks.
          if (iv.kind === "default") rv[p] = this.evalDefault(col, defaultExprs[i]!, stmtRng, meter);
          else rv[p] = materializeInsertValue(iv, store.columnTypes()[i]!, bound);
        }
      }
      rows.push(rv);
    }
    // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
    // pre-statement snapshot (grammar.md §32).
    if (ret !== null) {
      const foldCost = { value: 0n };
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, EMPTY_CTE_CTX, foldCost));
      meter.charge(foldCost.value);
    }
    const returned = this.insertRows(table, store, pk, checks, defaultExprs, stmtRng, provided, rows, ret?.nodes ?? null, bound, meter);
    return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, rows.length, meter.accrued);
  }

  // insertRows runs phase 1 + phase 2 of an INSERT, shared by the VALUES and SELECT sources. Each
  // element of rows is one row's candidate values indexed by VALUE POSITION p (length arity); the
  // declaration-order stored row is built via provided (an omitted column takes its default else
  // NULL) and each value is type-coerced + range-checked by storeValue (23502 / 22003 / 22P02 /
  // 42804). The storage key is computed and checked for a duplicate (23505 — within this batch via
  // seenKeys AND against the store) BEFORE any row is written; only once every row validates are
  // they all inserted (phase 2), allocating a fresh monotonic rowid in row order for a no-PK
  // table. All-or-nothing: a failure leaves the store untouched and burns no rowids.
  // `returning` is the resolved RETURNING projection (grammar.md §32), evaluated over the
  // validated rows after every check passes and BEFORE phase 2 writes — so its subqueries
  // observe the pre-statement snapshot and a ceiling abort stays all-or-nothing; `params`
  // feeds its $Ns. Returns the projected output rows, null without a clause.
  private insertRows(
    table: Table,
    store: TableStore,
    pk: number[],
    checks: NamedCheck[],
    defaultExprs: (RExpr | null)[],
    rng: StmtRng,
    provided: number[],
    rows: Value[][],
    returning: RExpr[] | null,
    params: Value[],
    meter: Meter,
  ): Value[][] | null {
    const n = table.columns.length;
    // The columns' resolved ColTypes (a scalar, or a composite resolved to its field tree), for
    // composite-aware store coercion (spec/design/composite.md §4).
    const colTypes = store.columnTypes();
    const prepared: { key: Uint8Array | null; row: Row }[] = [];
    const seenKeys = new Set<string>();
    // Per UNIQUE index (catalog/name order), the prefixes earlier rows of this batch
    // claimed — an in-batch duplicate traps 23505 like a stored one (indexes.md §8).
    const uniqDefs = table.indexes.filter((d) => d.unique);
    const seenPrefixes = uniqDefs.map(() => new Set<string>());
    let cunits = 0n;
    for (const values of rows) {
      const row: Row = new Array(n);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        // An omitted column takes its default — a constant, or its expression evaluated for
        // this row through the shared per-statement seam/clock (constraints.md §2). evalDefault
        // charges operator_eval for an expression default; a constant (or no default → NULL) is
        // free.
        const candidate: Value = p >= 0 ? values[p]! : this.evalDefault(col, defaultExprs[i]!, rng, meter);
        row[i] = coerceForStore(candidate, colTypes[i]!, col.decimal, col.notNull, col.name);
      }

      // CHECK constraints, in name order, on the fully-coerced candidate row — after NOT
      // NULL (storeValue above), before the key/duplicate check (PG's per-row order,
      // constraints.md §4.4). TRUE and NULL pass; the first FALSE aborts the whole
      // statement (two-phase — nothing has been written). Evaluation is metered
      // expression work (operator_eval), so guard the ceiling per checked row. The
      // per-statement rng is shared with the default evaluation above (one StmtRng).
      if (checks.length > 0) {
        meter.guard();
        const env: EvalEnv = {
          params: [],
          outer: [],
          runSubquery: (p, o) => this.execQueryPlan(p, o, [], EMPTY_CTE_CTX),
          seam: this.seam,
          rng,
          ctes: EMPTY_CTE_CTX,
          exec: this,
        };
        evalChecks(checks, table.name, row, env, meter);
      }

      let key: Uint8Array | null = null;
      if (pk.length > 0) {
        // The composite key is the concatenation of the members' bare encodings in key
        // order (encoding.md §2.3) — every keyable type is fixed-width, so the
        // concatenation is self-delimiting and byte comparison equals the tuple's order. A
        // single-column key is the one-member case of the same rule.
        const parts: Uint8Array[] = [];
        for (const i of pk) {
          const pkv = row[i]!; // non-null: a PK member is NOT NULL and was checked above
          if (pkv.kind === "uuid") {
            // uuid is the first non-integer key: its key is the bare 16 bytes (uuid-raw16,
            // encoding.md §2.7) — a PK is NOT NULL, so no presence tag, no sign-flip.
            parts.push(pkv.bytes.slice());
          } else if (pkv.kind === "bool") {
            // boolean is the second non-integer key: the bare 1-byte bool-byte (0x00 false /
            // 0x01 true, encoding.md §2.9) — likewise no presence tag.
            parts.push(encodeBool(pkv.value));
          } else if (pkv.kind === "int") {
            parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.int));
          } else if (pkv.kind === "timestamp" || pkv.kind === "timestamptz") {
            // A timestamp / timestamptz PK encodes its i64 instant (spec/design/timestamp.md §6).
            parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.micros));
          } else if (pkv.kind === "date") {
            // A date PK encodes its i32 day count (spec/design/date.md §5).
            parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.days));
          } else {
            throw engineError("data_corrupted", "a primary key must be an integer, boolean, uuid, or timestamp value");
          }
        }
        const total = parts.reduce((acc, b) => acc + b.length, 0);
        key = new Uint8Array(total);
        let off = 0;
        for (const b of parts) {
          key.set(b, off);
          off += b.length;
        }
        // The PK's 23505 reports PostgreSQL's derived auto-name for the PK index,
        // `<table>_pkey` — jed persists/reserves no such relation (constraints.md §5.4).
        const seen = key.join(",");
        if (seenKeys.has(seen) || store.get(key) !== undefined) {
          throw engineError(
            "unique_violation",
            "duplicate key value violates unique constraint: " +
              table.name.toLowerCase() +
              "_pkey",
          );
        }
        seenKeys.add(seen);
      }
      // UNIQUE-index probes (indexes.md §8), AFTER the primary-key duplicate check (PG
      // reports the PK first when both are violated — probed): per unique index in
      // catalog (name) order, a fully-non-NULL key tuple (its slot prefix) must match no
      // existing entry and no earlier row of this batch. Unmetered validation, like the
      // PK duplicate check (cost.md §3).
      for (let u = 0; u < uniqDefs.length; u++) {
        const def = uniqDefs[u]!;
        const prefix = indexPrefixKey(table.columns, def, row);
        if (prefix === null) continue;
        const istore = this.readSnap().indexStore(def.name.toLowerCase());
        const stored = istore.rangeEntries(uniqueProbeBound(prefix));
        const k = prefix.join(",");
        if (stored.length > 0 || seenPrefixes[u]!.has(k)) {
          throw engineError(
            "unique_violation",
            "duplicate key value violates unique constraint: " + def.name,
          );
        }
        seenPrefixes[u]!.add(k);
      }
      // Meter the row's disposition-plan compression attempts (value_compress, cost.md §3).
      // For a no-PK table the synthetic rowid is allocated in phase 2; only the key LENGTH
      // feeds the plan, so an 8-byte placeholder stands in deterministically.
      cunits += BigInt(store.writeCompressUnits(key ?? new Uint8Array(8), row));
      prepared.push({ key, row });
    }

    // FOREIGN KEY existence (constraints.md §6.4) — after all candidate rows are prepared, so the
    // check sees the statement's batch END STATE (a later row may supply an earlier row's parent
    // key; a self-reference resolves within the batch — PG's end-of-statement semantics). Unmetered
    // validation, like the PK/UNIQUE probes, and before any write (all-or-nothing). MATCH SIMPLE: a
    // row with any NULL local column is exempt.
    const relation = table.name;
    const fks = this.table(table.name)?.fks ?? [];
    for (const fk of fks) {
      // The parent exists (validated at CREATE TABLE; DROP TABLE refuses to drop a referenced
      // table — §6.10), so a consistent catalog always finds it.
      const parent = this.table(fk.refTable);
      if (parent === undefined) continue;
      // Only a self-reference can satisfy against this statement's batch (a different parent table
      // is unchanged by this INSERT). Collect the parent keys the batch supplies.
      const batch = new Set<string>();
      if (fk.refTable.toLowerCase() === relation.toLowerCase()) {
        for (const pr of prepared) {
          const p = fkProbe(fk, parent, pr.row, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
      }
      for (const pr of prepared) {
        const probe = fkProbe(fk, parent, pr.row, fk.columns);
        if (probe === null) continue; // a NULL local column → exempt (MATCH SIMPLE)
        if (batch.has(fkProbeBytes(probe).join(","))) continue;
        if (!this.fkProbeHits(probe, fk.refTable)) {
          throw engineError(
            "foreign_key_violation",
            "insert or update on table " + relation + " violates foreign key constraint " + fk.name,
          );
        }
      }
    }

    // Charge + enforce the ceiling BEFORE phase 2 writes anything (all-or-nothing).
    meter.charge(COSTS.valueCompress * cunits);
    meter.guard();

    // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the validated
    // rows — every check has passed, nothing is written yet, so subqueries in the list read
    // the pre-statement snapshot and a 54P01 here leaves the store untouched.
    const returned =
      returning !== null
        ? this.projectReturning(returning, prepared.map((pr) => pr.row), null, params, meter)
        : null;

    // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
    // rowid is allocated here, in row order, so a failed validation pass burns none
    // (spec/fileformat/format.md, grammar.md §12). Each stored row's secondary-index
    // entries are computed against its final key (the rowid included) and written after
    // the rows (indexes.md §4 — an index write cannot fail, so all-or-nothing is
    // unchanged).
    const indexInserts: Uint8Array[][] = table.indexes.map(() => []);
    for (const pr of prepared) {
      const key = pr.key ?? encodeInt("i64", store.allocRowid());
      for (let k = 0; k < table.indexes.length; k++) {
        indexInserts[k]!.push(...indexEntryKeys(table.columns, table.indexes[k]!, key, pr.row));
      }
      if (!store.insert(key, pr.row)) {
        throw new Error("pre-validated INSERT key must be unique");
      }
    }
    for (let k = 0; k < table.indexes.length; k++) {
      const istore = this.working().indexStore(table.indexes[k]!.name.toLowerCase());
      for (const ek of indexInserts[k]!) {
        if (!istore.insert(ek, [])) {
          throw new Error("index entry keys are unique (storage-key suffix)");
        }
      }
    }
    return returned;
  }

  // resolveReturning resolves a RETURNING item list against the target table's one-relation
  // scope (grammar.md §32): aggregates are 42803 (the non-collecting AggCtx), subqueries
  // resolve (and may correlate against the returned row), output names follow §8. Returns the
  // projection nodes, names, and the canonical type names (Outcome columnTypes — conformance.md §7).
  // The scope is the RETURNING scope (Scope.returning — the table at offset 0 plus the
  // old/new qualifier-only pseudo-relations over the [base | other] projection row, with
  // baseIsOld true for DELETE).
  private resolveReturning(
    table: Table,
    items: SelectItems,
    baseIsOld: boolean,
    ptypes: ParamTypes,
  ): { nodes: RExpr[]; names: string[]; types: string[] } {
    const scope = Scope.returning(this, table, baseIsOld);
    const { nodes, names, types } = resolveProjections(
      scope,
      items,
      { collecting: false, groupKeys: [], specs: [] },
      ptypes,
    );
    return { nodes, names, types: typeNames(types) };
  }

  // projectReturning evaluates a resolved RETURNING projection over the affected rows
  // (grammar.md §32, cost.md §3): per returned row, guard the ceiling, charge one
  // row_produced, then evaluate each item — metered expression work, exactly a SELECT's
  // projection (a correlated subquery re-runs here, its outer reference reading the row being
  // returned). Callers run this after all validation and BEFORE any write.
  // The evaluation row is the concatenation [base | other] the RETURNING scope resolved
  // against: others[i] is the row's opposite version (UPDATE's old rows), null the all-NULL
  // row (INSERT's old side, DELETE's new side).
  private projectReturning(
    nodes: RExpr[],
    rows: Row[],
    others: Row[] | null,
    params: Value[],
    meter: Meter,
  ): Value[][] {
    const env: EvalEnv = { params, outer: [], runSubquery: (p, o) => this.execQueryPlan(p, o, params, EMPTY_CTE_CTX), seam: this.seam, rng: new StmtRng(), ctes: EMPTY_CTE_CTX, exec: this };
    const out: Value[][] = [];
    rows.forEach((row, i) => {
      meter.guard();
      meter.charge(COSTS.rowProduced);
      const combined = row.concat(others !== null ? others[i]! : row.map(() => nullValue()));
      out.push(nodes.map((node) => evalExpr(node, combined, env, meter)));
    });
    return out;
  }

  // executeDelete resolves the table and optional predicate, collects the keys of
  // matching rows (only a TRUE predicate matches — Kleene), then removes them. No WHERE
  // deletes every row. Keys are collected before mutating.
  private executeDelete(del: Delete, params: Value[]): Outcome {
    const table = this.table(del.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + del.table);
    }
    // DELETE is single-table; resolve its WHERE against a one-relation scope. The RETURNING
    // projection resolves after it (PostgreSQL's analysis order), against the same scope
    // (grammar.md §32).
    const scope = Scope.single(this, table);
    const ptypes = new ParamTypes();
    let filter = del.filter ? resolveBooleanFilter(scope, del.filter, ptypes) : null;
    const ret = del.returning !== null ? this.resolveReturning(table, del.returning, true, ptypes) : null;
    const bound = bindParams(params, ptypes.finalize());

    // Fold globally-uncorrelated WHERE subqueries once (their cost is added a single time —
    // spec/design/grammar.md §26, cost.md §3); a correlated one stays and re-runs per row via the
    // per-row outer environment below (it pushes the current row, so `target.col` reads it). The
    // uncorrelated execution reads the pre-DELETE snapshot (keys are collected before mutating).
    // Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13; cost.md §3).
    const meter = new Meter(this.maxCost);
    if (filter !== null) {
      const cost = { value: 0n };
      filter = this.foldUncorrelatedInRExpr(filter, bound, EMPTY_CTE_CTX, cost);
      meter.charge(cost.value);
    }
    // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
    // pre-statement snapshot (grammar.md §32).
    if (ret !== null) {
      const cost = { value: 0n };
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, EMPTY_CTE_CTX, cost));
      meter.charge(cost.value);
    }
    const env: EvalEnv = { params: bound, outer: [], runSubquery: (p, o) => this.execQueryPlan(p, o, bound, EMPTY_CTE_CTX), seam: this.seam, rng: new StmtRng(), ctes: EMPTY_CTE_CTX, exec: this };
    const store = this.working().store(del.table);
    // matched collects (key, row) pairs before mutating; the rows feed phase 2's
    // index-entry removal (indexed columns are fixed-width and always resident).
    const matched: { key: Uint8Array; row: Row }[] = [];
    // DELETE's touched set (cost.md §3): the filter's columns plus the RETURNING items'
    // OLD-side references — a returned old value is a logical read of the dropped row,
    // while a new.col is the constant NULL row and reads nothing. The RETURNING mask spans
    // the [base | other] projection row (2 x ncols); only the base (old) half maps back to
    // storage. A bare DELETE still charges no chain/decompress units at all.
    const mask: boolean[] = new Array(table.columns.length).fill(false);
    if (filter !== null) collectTouched(filter, 0, mask);
    if (ret !== null) {
      const retMask: boolean[] = new Array(2 * table.columns.length).fill(false);
      for (const node of ret.nodes) collectTouched(node, 0, retMask);
      for (let i = 0; i < mask.length; i++) {
        if (retMask[i]) mask[i] = true;
      }
    }
    // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
    // scan"); an empty bound deletes nothing — with RETURNING that is still a query result
    // (empty rows), never a bare statement (grammar.md §32). The whole WHERE stays the
    // residual filter below. page_read per visited node (block, before the rows), then
    // storageRowRead per scanned row.
    const { entries, overlap, slabs } = scanEntries(store, mutationPkBound(table, filter), bound, mask);
    if (entries === null) return dmlOutcome(ret?.names ?? null, ret?.types ?? null, null, 0, meter.accrued); // empty bound
    meter.charge(COSTS.pageRead * BigInt(overlap) + COSTS.valueDecompress * BigInt(slabs));
    for (const e of entries) {
      meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
      // A WHERE arithmetic can throw (22003/22012); the throw propagates naturally.
      meter.charge(COSTS.storageRowRead);
      // Materialize the filter's columns if the lazy load left them unfetched — exactly the
      // touched set the block above charged (large-values.md §14).
      const row = store.resolveColumns(e.row, mask);
      if (filter === null || isTrue(evalExpr(filter, row, env, meter))) {
        matched.push({ key: e.key, row });
      }
    }

    // FOREIGN KEY parent-side (constraints.md §6.5): a DELETE must not strand a child. For each
    // inbound FK, every deleted row's referenced tuple disappears (the referenced columns are
    // unique, so each is unique to its row); if a child still references it → 23503. Unmetered,
    // before phase 2 (all-or-nothing). For a self-reference the child IS this table, whose end
    // state excludes the rows being deleted.
    const delReferencers = this.fkReferencers(del.table);
    if (delReferencers.length > 0) {
      const parent = this.table(del.table)!;
      const deletedKeys = new Set<string>(matched.map((m) => m.key.join(",")));
      const empty = new Set<string>();
      for (const { childTable, fk } of delReferencers) {
        const exclude = childTable.toLowerCase() === del.table.toLowerCase() ? deletedKeys : empty;
        for (const m of matched) {
          const probe = fkProbe(fk, parent, m.row, fk.refColumns);
          if (probe === null) continue; // a NULL referenced value cannot be referenced (MATCH SIMPLE)
          if (this.fkChildReferences(childTable, fk, parent, fkProbeBytes(probe), exclude)) {
            throw engineError(
              "foreign_key_violation",
              "update or delete on table " + parent.name + " violates foreign key constraint " + fk.name + " on table " + childTable,
            );
          }
        }
      }
    }

    // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched rows'
    // OLD values before anything is removed — subqueries in the list read the pre-statement
    // snapshot, and a 54P01 here deletes nothing (all-or-nothing).
    const returned =
      ret !== null
        ? this.projectReturning(ret.nodes, matched.map((m) => m.row), null, bound, meter)
        : null;
    // Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
    // unmetered write work; an index removal cannot fail).
    for (const m of matched) store.remove(m.key);
    for (const def of table.indexes) {
      const istore = this.working().indexStore(def.name.toLowerCase());
      for (const m of matched) {
        for (const ek of indexEntryKeys(table.columns, def, m.key, m.row)) istore.remove(ek);
      }
    }
    return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, matched.length, meter.accrued);
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
    const scope = Scope.single(this, table);
    const ptypes = new ParamTypes();

    // Resolve assignments up front (fail fast, deterministic).
    // The 0A000 guard covers EVERY key member — for a composite PRIMARY KEY, assigning
    // any member would change the storage key (constraints.md §3).
    const pkMembers = pkIndices(table);
    const plans: AssignPlan[] = [];
    for (const a of upd.assignments) {
      const idx = columnIndex(table, a.column);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + a.column);
      }
      if (pkMembers.includes(idx)) {
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
      // Updating a composite-typed column lands in a later slice (the storable + INSERT/SELECT
      // round-trip is S3 — spec/design/composite.md §12); reject it for now (0A000).
      if (col.type.kind === "composite") {
        throw engineError("feature_not_supported", "updating composite column " + a.column + " is not supported yet");
      }
      if (col.type.kind === "array") {
        throw engineError("feature_not_supported", "updating array column " + a.column + " is not supported yet");
      }
      const targetScalar = col.type.scalar;
      // The RHS is a general expression evaluated against the OLD row; a literal operand
      // adapts to the target column's type. The result must be assignable to the column's
      // family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
      const { node, type } = resolve(scope, a.value, targetScalar, { collecting: false, groupKeys: [], specs: [] }, ptypes);
      requireAssignable(type, targetScalar, a.column);
      plans.push({ idx, name: col.name, target: targetScalar, decimal: col.decimal, notNull: col.notNull, source: node });
    }

    let filter = upd.filter ? resolveBooleanFilter(scope, upd.filter, ptypes) : null;
    // The RETURNING projection resolves last (PostgreSQL's analysis order), against the same
    // one-relation scope; it evaluates each matched row's NEW values (grammar.md §32).
    const ret = upd.returning !== null ? this.resolveReturning(table, upd.returning, false, ptypes) : null;
    // The CHECK constraints, resolved once per statement in evaluation (name) order;
    // phase 1 evaluates them on each post-assignment row (constraints.md §4.4).
    const checks = this.resolveChecks(table);
    // All assignment RHSs + the WHERE + the RETURNING are resolved: finalize + bind before
    // any scan.
    const bound = bindParams(params, ptypes.finalize());

    // Fold globally-uncorrelated subqueries (in any assignment RHS or the WHERE) once — their
    // cost is added a single time (grammar.md §26, cost.md §3); a correlated one stays and re-runs
    // per row via the outer environment (which pushes the current OLD row). The uncorrelated
    // execution reads the pre-UPDATE snapshot (phase 1 only reads; phase 2 writes).
    //
    // Phase 1: build + validate every matching row's new values; no writes yet. Each scanned row,
    // the filter, and each assignment RHS accrue cost (the phase-2 writes do not — cost.md §3).
    const meter = new Meter(this.maxCost);
    const foldCost = { value: 0n };
    for (const p of plans) p.source = this.foldUncorrelatedInRExpr(p.source, bound, EMPTY_CTE_CTX, foldCost);
    if (filter !== null) filter = this.foldUncorrelatedInRExpr(filter, bound, EMPTY_CTE_CTX, foldCost);
    if (ret !== null) {
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, EMPTY_CTE_CTX, foldCost));
    }
    meter.charge(foldCost.value);
    const env: EvalEnv = { params: bound, outer: [], runSubquery: (p, o) => this.execQueryPlan(p, o, bound, EMPTY_CTE_CTX), seam: this.seam, rng: new StmtRng(), ctes: EMPTY_CTE_CTX, exec: this };
    const store = this.working().store(upd.table);
    // Each entry is (key, new row, OLD row) — the old row feeds the index maintenance.
    const updates: { key: Uint8Array; row: Row; oldRow: Row }[] = [];
    // UPDATE's touched set (cost.md §3): the filter's columns, every assignment SOURCE's, and
    // the RETURNING items' MINUS the assigned columns — an assigned column's returned value is
    // the freshly computed one, not a storage read. The rewrite re-stores an untouched spilled
    // value without logically re-reading it (large-values.md §14).
    const mask: boolean[] = new Array(table.columns.length).fill(false);
    if (filter !== null) collectTouched(filter, 0, mask);
    for (const p of plans) collectTouched(p.source, 0, mask);
    // The RETURNING mask spans the [base | other] projection row (new at 0, old at ncols):
    // the NEW side joins minus the assigned columns (an assigned column's returned value is
    // the freshly computed one, not a storage read); the OLD side joins unconditionally
    // (old.col is always a storage read, assigned or not).
    if (ret !== null) {
      const ncols = table.columns.length;
      const retMask: boolean[] = new Array(2 * ncols).fill(false);
      for (const node of ret.nodes) collectTouched(node, 0, retMask);
      for (let i = 0; i < ncols; i++) {
        if (retMask[i] && !plans.some((p) => p.idx === i)) mask[i] = true; // new side
        if (retMask[ncols + i]) mask[i] = true; // old side — always a storage read
      }
    }
    // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
    // scan"); an empty bound updates nothing — with RETURNING that is still a query result
    // (empty rows), never a bare statement (grammar.md §32). The whole WHERE stays the
    // residual filter below. page_read per visited node (block, before the rows), then
    // storageRowRead per scanned row.
    const { entries, overlap, slabs } = scanEntries(store, mutationPkBound(table, filter), bound, mask);
    if (entries === null) return dmlOutcome(ret?.names ?? null, ret?.types ?? null, null, 0, meter.accrued); // empty bound
    meter.charge(COSTS.pageRead * BigInt(overlap) + COSTS.valueDecompress * BigInt(slabs));
    for (const e of entries) {
      meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
      meter.charge(COSTS.storageRowRead);
      // Materialize the filter's + assignment sources' columns if the lazy load left them
      // unfetched — exactly the touched set the block above charged (large-values.md §14).
      const row = store.resolveColumns(e.row, mask);
      if (filter !== null && !isTrue(evalExpr(filter, row, env, meter))) continue;
      const newRow = row.slice();
      for (const p of plans) {
        newRow[p.idx] = checkAssign(p, evalExpr(p.source, row, env, meter));
      }
      // The rewritten row is stored fully resident: resolve any still-unfetched (untouched)
      // columns so its weight/disposition re-plan exactly as an eager writer's would —
      // unmetered, part of the rewrite like commit work (large-values.md §14).
      const resident = store.resolveAll(newRow);
      // CHECK constraints, in name order, on the post-assignment row — after the
      // assignments coerced (22003/23502 in checkAssign above), on the fully-resident row
      // (constraints.md §4.4). Every check evaluates (not only those mentioning assigned
      // columns); TRUE and NULL pass, the first FALSE aborts the statement (phase 1 —
      // nothing has been written).
      evalChecks(checks, table.name, resident, env, meter);
      updates.push({ key: e.key, row: resident, oldRow: row });
    }

    // UNIQUE validation against the statement's END STATE (indexes.md §8 — a documented
    // PG divergence: PG checks per-row in heap order, so a transient collision like
    // `SET v = v + 1` fails there and succeeds here). Per unique index in catalog (name)
    // order, over the rewritten rows in scan (storage-key) order: the new prefixes must
    // not collide with each other (in-batch), nor with an existing entry whose suffix is
    // NOT a rewritten row's key (a rewritten row's old entry is being replaced, so it
    // cannot conflict). Unmetered validation, phase 1.
    if (updates.length > 0 && table.indexes.some((d) => d.unique)) {
      const rewritten = new Set<string>(updates.map((u) => u.key.join(",")));
      for (const def of table.indexes) {
        if (!def.unique) continue;
        const istore = this.readSnap().indexStore(def.name.toLowerCase());
        const batch = new Set<string>();
        for (const u of updates) {
          const prefix = indexPrefixKey(table.columns, def, u.row);
          if (prefix === null) continue;
          const k = prefix.join(",");
          const conflict =
            batch.has(k) ||
            istore
              .rangeEntries(uniqueProbeBound(prefix))
              .some((e) => !rewritten.has(e.key.slice(prefix.length).join(",")));
          if (conflict) {
            throw engineError(
              "unique_violation",
              "duplicate key value violates unique constraint: " + def.name,
            );
          }
          batch.add(k);
        }
      }
    }

    // FOREIGN KEY child-side (constraints.md §6.4): re-validate an FK only when the statement
    // assigns one of its local columns (an unchanged value stays valid). Each updated NEW row must
    // reference an existing parent key — committed parent state, plus (for a self-reference) the
    // updated rows' new referenced values, so a row may reference a value another updated row now
    // supplies. Unmetered, phase 1, before any write.
    const relation = table.name;
    const assigned = new Set<number>(plans.map((p) => p.idx));
    const updFks = this.table(upd.table)?.fks ?? [];
    for (const fk of updFks) {
      if (!fk.columns.some((c) => assigned.has(c))) continue; // this FK's local columns were not assigned
      const parent = this.table(fk.refTable);
      if (parent === undefined) continue;
      const batch = new Set<string>();
      if (fk.refTable.toLowerCase() === relation.toLowerCase()) {
        for (const u of updates) {
          const p = fkProbe(fk, parent, u.row, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
      }
      for (const u of updates) {
        const probe = fkProbe(fk, parent, u.row, fk.columns);
        if (probe === null) continue; // a NULL local column → exempt (MATCH SIMPLE)
        if (batch.has(fkProbeBytes(probe).join(","))) continue;
        if (!this.fkProbeHits(probe, fk.refTable)) {
          throw engineError(
            "foreign_key_violation",
            "insert or update on table " + relation + " violates foreign key constraint " + fk.name,
          );
        }
      }
    }

    // FOREIGN KEY parent-side (constraints.md §6.5): an UPDATE of a referenced row must not strand
    // a child. A referenced PRIMARY KEY column cannot change (PK assignment is 0A000), so only a
    // referenced UNIQUE column is at risk. For each inbound FK, a referenced tuple DISAPPEARS when
    // an updated row's old value is absent from the statement's new end state (old − new over the
    // updated rows); if a child still references a disappearing tuple → 23503. Unmetered, phase 1.
    // A self-reference's child IS this table, whose end state excludes the rows being updated
    // (their new values are validated child-side above).
    const updReferencers = this.fkReferencers(upd.table);
    if (updReferencers.length > 0) {
      const parent = this.table(upd.table)!;
      const updatedKeys = new Set<string>(updates.map((u) => u.key.join(",")));
      const empty = new Set<string>();
      for (const { childTable, fk } of updReferencers) {
        // The referenced tuples the updated rows now supply (so a swap re-supplies one).
        const newPresent = new Set<string>();
        for (const u of updates) {
          const p = fkProbe(fk, parent, u.row, fk.refColumns);
          if (p !== null) newPresent.add(fkProbeBytes(p).join(","));
        }
        const exclude = childTable.toLowerCase() === upd.table.toLowerCase() ? updatedKeys : empty;
        for (const u of updates) {
          const oldProbe = fkProbe(fk, parent, u.oldRow, fk.refColumns);
          if (oldProbe === null) continue; // a NULL old referenced value was referenced by nothing
          // Unchanged tuples (incl. a NULL → already skipped) do not disappear.
          const newProbe = fkProbe(fk, parent, u.row, fk.refColumns);
          if (newProbe !== null && bytesEq(fkProbeBytes(newProbe), fkProbeBytes(oldProbe))) continue;
          // Re-supplied by another updated row (e.g. a value swap) → not disappearing.
          if (newPresent.has(fkProbeBytes(oldProbe).join(","))) continue;
          if (this.fkChildReferences(childTable, fk, parent, fkProbeBytes(oldProbe), exclude)) {
            throw engineError(
              "foreign_key_violation",
              "update or delete on table " + parent.name + " violates foreign key constraint " + fk.name + " on table " + childTable,
            );
          }
        }
      }
    }

    // Each rewritten row's disposition plan may attempt compression (a record over RECORD_MAX)
    // — meter the attempts (value_compress, cost.md §3) and enforce the ceiling BEFORE phase 2
    // writes anything, preserving all-or-nothing.
    let cunits = 0n;
    for (const u of updates) cunits += BigInt(store.writeCompressUnits(u.key, u.row));
    meter.charge(COSTS.valueCompress * cunits);
    meter.guard();

    // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched rows'
    // NEW (post-assignment, fully resident) values — all validation has passed, nothing is
    // written yet, so subqueries in the list read the pre-statement snapshot and a 54P01 here
    // writes nothing (all-or-nothing).
    const returned =
      ret !== null
        ? this.projectReturning(ret.nodes, updates.map((u) => u.row), updates.map((u) => u.oldRow), bound, meter)
        : null;

    // Index maintenance (indexes.md §4): an entry moves only when its key CHANGED — equal
    // old/new keys leave the index tree untouched (part of the contract: it keeps the
    // copy-on-write dirty set, and so the commit's written pages, byte-identical across
    // cores). The storage key cannot change (PK assignment is rejected), so the suffix is
    // stable.
    const indexMoves: { removals: Uint8Array[]; insertions: Uint8Array[] }[][] = table.indexes.map(
      () => [],
    );
    for (const u of updates) {
      for (let k = 0; k < table.indexes.length; k++) {
        const def = table.indexes[k]!;
        // The row's old and new entry SETS (one entry for an ordered index, one per term for GIN —
        // gin.md §5). Remove old−new, insert new−old: a shared entry is left untouched, keeping the
        // copy-on-write dirty set byte-identical across cores.
        const oldEks = indexEntryKeys(table.columns, def, u.key, u.oldRow);
        const newEks = indexEntryKeys(table.columns, def, u.key, u.row);
        const removals = bytesDiff(oldEks, newEks);
        const insertions = bytesDiff(newEks, oldEks);
        if (removals.length > 0 || insertions.length > 0) {
          indexMoves[k]!.push({ removals, insertions });
        }
      }
    }

    // Phase 2: apply (keys unchanged — a PK column can't be assigned), then move the
    // changed index entries (unmetered write work; cannot fail).
    for (const u of updates) store.replace(u.key, u.row);
    for (let k = 0; k < table.indexes.length; k++) {
      const istore = this.working().indexStore(table.indexes[k]!.name.toLowerCase());
      for (const mv of indexMoves[k]!) {
        for (const oldEk of mv.removals) istore.remove(oldEk);
        for (const newEk of mv.insertions) {
          if (!istore.insert(newEk, [])) {
            throw new Error("index entry keys are unique (storage-key suffix)");
          }
        }
      }
    }
    return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, updates.length, meter.accrued);
  }

  // executeSelect runs a SELECT as a top-level statement: runSelect, then wrap as a query
  // Outcome (the projection types are internal — only INSERT ... SELECT consumes them).
  private executeSelect(sel: Select, params: Value[]): Outcome {
    const r = this.runSelect(sel, params);
    return { kind: "query", columnNames: r.columnNames, columnTypes: typeNames(r.columnTypes), rows: r.rows, cost: r.cost };
  }

  // executeSetOp runs a set operation as a top-level statement: runSetOp, then wrap as a query
  // Outcome. Cost is lhs.cost + rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
  private executeSetOp(so: SetOp, params: Value[]): Outcome {
    const r = this.runSetOp(so, params);
    return { kind: "query", columnNames: r.columnNames, columnTypes: typeNames(r.columnTypes), rows: r.rows, cost: r.cost };
  }

  // runQueryExpr runs a query expression to a SelectResult — a lone SELECT via runSelect, or a set
  // operation via runSetOp (recursively, so a chain `a UNION b INTERSECT c` evaluates as the parsed
  // precedence tree).
  // runQueryExpr is the top-level orchestrator (spec/design/grammar.md §26): PLAN the whole
  // expression tree once against an empty scope chain (threading one ParamTypes so $N inference is
  // statement-wide), bind the parameters, then the foldUncorrelated pass executes each
  // globally-uncorrelated subquery once and folds it to a constant (preserving the once-only cost),
  // and finally EXECUTE against an empty outer-row environment. Correlated subqueries that survive
  // the fold are re-executed per outer row by the evaluator.
  private runQueryExpr(qe: QueryExpr, params: Value[]): SelectResult {
    const ptypes = new ParamTypes();
    const plan = this.planQuery(qe, null, [], ptypes);
    const bound = bindParams(params, ptypes.finalize());
    const subqueryCost = { value: 0n };
    this.foldUncorrelatedInPlan(plan, bound, EMPTY_CTE_CTX, subqueryCost);
    const r = this.execQueryPlan(plan, [], bound, EMPTY_CTE_CTX);
    return { ...r, cost: r.cost + subqueryCost.value };
  }

  // runSelect runs a lone SELECT — the entry point executeSelect and INSERT ... SELECT use.
  private runSelect(sel: Select, params: Value[]): SelectResult {
    return this.runQueryExpr(sel, params);
  }

  // runSetOp runs a set operation as a top-level statement.
  private runSetOp(so: SetOp, params: Value[]): SelectResult {
    return this.runQueryExpr(so, params);
  }

  // executeWith runs a WITH query (spec/design/cte.md) as a top-level statement: runWith, then wrap
  // as a query Outcome.
  private executeWith(wq: WithQuery, params: Value[]): Outcome {
    const r = this.runWith(wq, params);
    return { kind: "query", columnNames: r.columnNames, columnTypes: typeNames(r.columnTypes), rows: r.rows, cost: r.cost };
  }

  // runWith runs a WITH query (spec/design/cte.md). The CTE orchestrator:
  // (1) PLAN each CTE body in order against the prefix of earlier bindings (parent = null — a body
  //     is an independent query, NOT correlated to a reference site), deriving each binding's
  //     synthetic relation;
  // (2) plan the main body with all bindings visible, threading the one ParamTypes so $N infers
  //     statement-wide;
  // (3) decide each CTE's mode from its reference count + [NOT] MATERIALIZED hint;
  // (4) MATERIALIZE each referenced materialized CTE once, in list order, accruing its cost (a later
  //     body sees the earlier buffers);
  // (5) fold + EXECUTE the main body with the CTE context.
  // Cost composes like set operations — a sum of the parts.
  private runWith(wq: WithQuery, params: Value[]): SelectResult {
    const ptypes = new ParamTypes();
    // (1) Plan each CTE body against the already-built prefix; build its synthetic relation.
    const bindings: CteBinding[] = [];
    for (const cte of wq.ctes) {
      const lname = cte.name.toLowerCase();
      if (bindings.some((b) => b.name === lname)) {
        throw engineError("duplicate_alias", `WITH query name ${lname} specified more than once`);
      }
      const plan = this.planQuery(cte.query, null, bindings, ptypes);
      const table = cteSyntheticTable(lname, plan, cte.columns);
      bindings.push({ name: lname, table, plan, hint: cte.materialized, refs: 0 });
    }
    // (2) Plan the main body with all bindings visible.
    const plan = this.planQuery(wq.body, null, bindings, ptypes);
    const bound = bindParams(params, ptypes.finalize());

    // (3) Per-CTE evaluation mode: MATERIALIZED hint or >=2 references -> materialize, else inline
    //     (cost.md §3). An unreferenced CTE is planned (errors surfaced) but not run.
    const modes: CteMode[] = bindings.map((b) => {
      if (b.hint === true) return "materialize";
      if (b.hint === false) return "inline";
      return b.refs >= 2 ? "materialize" : "inline";
    });
    const plans: QueryPlan[] = bindings.map((b) => b.plan);

    // (4) Materialize each referenced materialized CTE once, in list order, accruing cost. A later
    //     body's inline/materialized reference to an earlier CTE sees the prefix context.
    let totalCost = 0n;
    const buffers: Row[][] = [];
    for (let i = 0; i < plans.length; i++) {
      if (modes[i] === "materialize") {
        // Earlier-only context (the prefix): a CTE body sees EARLIER CTEs, never itself or a later
        // one (forward-only visibility — cte.md §2).
        const ctx: CteCtx = { modes: modes.slice(0, i), plans: plans.slice(0, i), buffers };
        const r = this.execQueryPlan(plans[i]!, [], bound, ctx);
        totalCost += r.cost;
        buffers.push(r.rows);
      } else {
        buffers.push([]);
      }
    }

    // (5) Fold + execute the main body against the full CTE context.
    const ctx: CteCtx = { modes, plans, buffers };
    const subqueryCost = { value: 0n };
    this.foldUncorrelatedInPlan(plan, bound, ctx, subqueryCost);
    const r = this.execQueryPlan(plan, [], bound, ctx);
    return { ...r, cost: r.cost + subqueryCost.value + totalCost };
  }

  // planQuery resolves a query expression into an owned QueryPlan against the scope chain (parent =
  // the enclosing query's scope, null at top level). A subquery is planned here, once (§26). `ctes`
  // is the statement's CTE bindings visible here (spec/design/cte.md §2) — inherited into every
  // nested scope, never via the parent chain.
  // Not private: the free function planSubquery calls it through scope.catalog (an internal seam).
  planQuery(qe: QueryExpr, parent: Scope | null, ctes: CteBinding[], ptypes: ParamTypes): QueryPlan {
    return qe.kind === "select"
      ? this.planSelect(qe, parent, ctes, ptypes)
      : this.planSetOp(qe, parent, ctes, ptypes);
  }

  // execQueryPlan executes a resolved plan against an outer-row environment (outer = the enclosing
  // rows, innermost last; empty at top level), the bound parameters, and the CTE context (a FROM
  // reference at any depth delivers a CTE's rows — spec/design/cte.md §5).
  private execQueryPlan(plan: QueryPlan, outer: Row[], params: Value[], ctes: CteCtx): SelectResult {
    if (plan.kind === "select") return this.execSelectPlan(plan, outer, params, ctes);
    if (plan.kind === "values") return this.execValuesPlan(plan, outer, params, ctes);
    return this.execSetOpPlan(plan, outer, params, ctes);
  }

  // planSetOp plans a set operation (spec/design/grammar.md §25): plan both operands with the same
  // parent scope, check arity + unify column types up front (so the 42601/42804 fire even over
  // empty operands), and resolve the trailing ORDER BY by output column name.
  private planSetOp(so: SetOp, parent: Scope | null, ctes: CteBinding[], ptypes: ParamTypes): SetOpPlan {
    const lhs = this.planQuery(so.lhs, parent, ctes, ptypes);
    const rhs = this.planQuery(so.rhs, parent, ctes, ptypes);

    if (lhs.columnTypes.length !== rhs.columnTypes.length) {
      throw engineError(
        "syntax_error",
        `each ${setopName(so.op)} query must have the same number of columns`,
      );
    }
    const columnTypes = lhs.columnTypes.map((l, i) => unifySetopColumn(l, rhs.columnTypes[i]!, so.op));
    const columnNames = lhs.columnNames;
    const order: OrderSlot[] = so.orderBy.map((key) => ({
      idx: resolveSetopOrderKey(key, columnNames),
      descending: key.descending,
      nullsFirst: key.nullsFirst,
    }));
    return {
      kind: "setOp",
      op: so.op,
      all: so.all,
      lhs,
      rhs,
      columnNames,
      columnTypes,
      order,
      limit: so.limit,
      offset: so.offset,
    };
  }

  // execSetOpPlan executes a resolved set operation: run both operands against the outer
  // environment, coerce to the unified types, combine, then sort + window. Cost is lhs.cost +
  // rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
  private execSetOpPlan(plan: SetOpPlan, outer: Row[], params: Value[], ctes: CteCtx): SelectResult {
    const left = this.execQueryPlan(plan.lhs, outer, params, ctes);
    const right = this.execQueryPlan(plan.rhs, outer, params, ctes);

    coerceSetopRows(left.rows, left.columnTypes, plan.columnTypes);
    coerceSetopRows(right.rows, right.columnTypes, plan.columnTypes);

    let rows = combineSetop(plan.op, plan.all, left.rows, right.rows);
    const cost = left.cost + right.cost;

    if (plan.order.length > 0) {
      rows.sort((a, b) => {
        for (const k of plan.order) {
          const c = keyCmp(a[k.idx]!, b[k.idx]!, k.descending, k.nullsFirst);
          if (c !== 0) return c;
        }
        return 0;
      });
    }

    const n = BigInt(rows.length);
    const start = plan.offset === null ? 0n : plan.offset < n ? plan.offset : n;
    const end = plan.limit !== null && plan.limit < n - start ? start + plan.limit : n;
    rows = rows.slice(Number(start), Number(end));

    return { columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows, cost };
  }

  // planValues resolves a VALUES-body relation into a ValuesPlan (spec/design/grammar.md §42) — the
  // body of a FROM (VALUES …) derived table. Each value resolves as a CONSTANT against an EMPTY
  // scope with parent=null: the body is non-LATERAL, so a column reference is unresolved
  // (42703/42P01) and an aggregate is 42803; it still sees the statement's CTE bindings (an
  // uncorrelated subquery inside a value resolves like anywhere). Every row must have the same arity
  // (42601); the columns' types unify across rows like a set operation (42804 on a mismatch). A bind
  // parameter is then noted at its column's unified type (so VALUES (1),($1) types $1 as int); a
  // column with no concrete type — all NULL/param — leaves its $N untyped, surfacing 42P18 at
  // finalize (jed's no-cross-context inference posture, §26).
  private planValues(rows: Expr[][], parent: Scope | null, ctes: CteBinding[], ptypes: ParamTypes): ValuesPlan {
    const arity = rows[0]!.length; // the parser guarantees at least one row, each with ≥1 value
    // A constant scope: no local relations. With parent===null (the usual case) any column reference
    // is unresolved (the non-LATERAL rule, §42); with a parent (a LATERAL VALUES body, §44) a column
    // reference correlates to the earlier FROM relations instead. CTE bindings stay visible and
    // subqueries are allowed (an uncorrelated one folds before the rows run).
    const scope = new Scope([], this, parent, true, ctes);
    const resolvedRows: RExpr[][] = [];
    const colTypes: ResolvedType[] = [];
    // Per column: the 0-based bind-parameter slots in it, typed in a second pass from the unified
    // column type (a $N takes its column's type, like a set-operation operand).
    const colParams: number[][] = Array.from({ length: arity }, () => []);
    rows.forEach((row, ri) => {
      if (row.length !== arity) {
        throw engineError("syntax_error", "VALUES lists must all be the same length");
      }
      const resolvedRow: RExpr[] = [];
      row.forEach((val, ci) => {
        // Forbidden aggregate context: an aggregate inside a value is 42803.
        const { node, type } = resolve(scope, val, null, { collecting: false, groupKeys: [], specs: [] }, ptypes);
        if (node.kind === "param") colParams[ci]!.push(node.index);
        if (ri === 0) colTypes.push(type);
        else colTypes[ci] = unifyValuesColumn(colTypes[ci]!, type);
        resolvedRow.push(node);
      });
      resolvedRows.push(resolvedRow);
    });
    // Second pass: note each column's bind parameters at the unified column type. A column with no
    // scalar type (all NULL/param) passes null — the parameter stays untyped (42P18).
    for (let ci = 0; ci < arity; ci++) {
      const hint = scalarForParamHint(colTypes[ci]!);
      for (const idx0 of colParams[ci]!) ptypes.note(idx0, hint);
    }
    // PostgreSQL names a VALUES relation's columns column1, column2, … ; the derived table's optional
    // column-rename list overrides them at the synthetic relation (cteSyntheticTable).
    const columnNames = Array.from({ length: arity }, (_, i) => `column${i + 1}`);
    return { kind: "values", rows: resolvedRows, columnTypes: colTypes, columnNames };
  }

  // execValuesPlan executes a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate
  // each row's values as constants over an EMPTY environment (no local row, no outer row —
  // non-LATERAL), coerce each to the unified column type (the only runtime change is int → decimal,
  // the set-operation rule), and emit the rows. Charges rowProduced per row plus each value's
  // operatorEval (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded into the
  // caller's meter via execQueryPlan.
  private execValuesPlan(plan: ValuesPlan, outer: Row[], params: Value[], ctes: CteCtx): SelectResult {
    const env: EvalEnv = {
      params,
      outer,
      runSubquery: (p, o) => this.execQueryPlan(p, o, params, ctes),
      seam: this.seam,
      rng: new StmtRng(),
      ctes,
      exec: this,
    };
    const meter = new Meter(this.maxCost);
    const rows: Value[][] = [];
    for (const row of plan.rows) {
      meter.guard(); // enforce the cost ceiling per produced row (CLAUDE.md §13)
      meter.charge(COSTS.rowProduced);
      const out: Value[] = [];
      for (let ci = 0; ci < plan.columnTypes.length; ci++) {
        let v = evalExpr(row[ci]!, [], env, meter);
        // int → decimal where the column unified to decimal (the set-operation rule); every other
        // unified type is a value no-op (int-width promotion is free — all ints are bigint).
        if (plan.columnTypes[ci]!.kind === "decimal" && v.kind === "int") {
          v = decimalValue(Decimal.fromBigInt(v.int));
        }
        out.push(v);
      }
      rows.push(out);
    }
    return { columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows, cost: meter.accrued };
  }

  // runSelect resolves projected columns and the WHERE/ORDER BY columns against the catalog,
  // scans in primary-key order, filters (three-valued — only TRUE keeps a row), optionally
  // re-sorts by ORDER BY, then projects. Returns the rows with each output column's NAME and
  // resolved TYPE (the types let INSERT ... SELECT gate assignability up front — §24) and the
  // accrued cost.
  // planSelect resolves a SELECT into a SelectPlan against the scope chain (parent = the enclosing
  // query's scope, for correlated references — grammar.md §26). The resolve half of the old
  // runSelect: build the FROM scope, resolve every clause, infer $N types into ptypes. No row is
  // touched and no parameter is bound here (runQueryExpr binds once, after the whole tree is planned).
  private planSelect(sel: Select, parent: Scope | null, ctes: CteBinding[], ptypes: ParamTypes): SelectPlan {
    // Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
    // relation's flat column offset in FROM order, and reject a duplicate label — a self-join
    // without distinct aliases is 42712 (spec/design/grammar.md §15). A FROM-less SELECT
    // (sel.from === null) builds an EMPTY scope: nothing local resolves, so bare columns fall
    // through to `parent` (the correlated-subquery rule) or 42703 at top level
    // (spec/design/grammar.md §34). The scope links to `parent` (correlation) + the catalog
    // (so a subquery resolves its own FROM); allowSubquery is true.
    // A FROM item is a base table, a set-returning function (grammar.md §35), or a derived table
    // (§42). For a LATERAL item (§44) the body / SRF args resolve against the PREFIX of relations to
    // its left (a dependent join), so the build runs in FROM order and a prefix scope over the
    // already-resolved rels is handed to the body.
    const tableRefs = sel.from === null ? [] : [sel.from, ...sel.joins.map((j) => j.table)];
    const rels: ScopeRel[] = [];
    const srfPlans: (SrfPlan | undefined)[] = []; // aligned with rels; undefined = a base table
    const derivedPlans: (QueryPlan | undefined)[] = []; // aligned with rels; non-undefined = derived
    // lateralFlags[i] is true when FROM item i is a CORRELATED lateral relation (§44) — its body /
    // SRF args reference an earlier sibling (or an enclosing query), so the executor re-materializes
    // it per combined left-hand row. A non-correlated item (or the first item) is materialized once.
    const lateralFlags: boolean[] = [];
    const seenLabels = new Set<string>();
    let offset = 0;
    for (let i = 0; i < tableRefs.length; i++) {
      const tref = tableRefs[i]!;
      let t: Table;
      let srf: SrfPlan | undefined;
      let cteIdx: number | undefined;
      let derived: QueryPlan | undefined;
      let lateral = false;
      const isDerived = tref.subquery !== undefined || tref.values !== undefined;
      // A FROM item is lateral-ELIGIBLE when it can see earlier siblings: a derived table / VALUES
      // body explicitly marked LATERAL, or ANY table function (implicitly lateral — §44). The first
      // item (i === 0) has no earlier sibling, so it is never lateral; an SRF there resolves against
      // `parent` (the enclosing query) exactly as before.
      const lateralEligible = i > 0 && ((isDerived && tref.lateral === true) || tref.args !== null);
      // The prefix scope a LATERAL item resolves against: the relations to its left, chained to the
      // enclosing query's parent (so a sibling column correlates as Outer{level=1}, an enclosing one
      // deeper). null when not lateral-eligible.
      const lateralParent = lateralEligible ? new Scope([...rels], this, parent, true, ctes) : null;
      if (isDerived) {
        // Plan the body. LATERAL → parent is the prefix scope (a sibling/outer column correlates);
        // otherwise an INDEPENDENT query (parent=null, §42). A LATERAL VALUES body resolves its
        // values against the prefix too (a column ref then correlates instead of 42703).
        const bodyParent = lateralEligible ? lateralParent : null;
        const plan =
          tref.subquery !== undefined
            ? this.planQuery(tref.subquery, bodyParent, ctes, ptypes)
            : this.planValues(tref.values!, bodyParent, ctes, ptypes);
        lateral = lateralEligible && queryPlanReferencesOuter(plan, 0);
        const label = tref.alias === null ? "" : tref.alias.toLowerCase();
        t = cteSyntheticTable(label, plan, tref.columnAliases ?? null);
        derived = plan;
      } else if (tref.args !== null) {
        // A table function (SRF) — implicitly lateral. At i>0 its args resolve against the prefix
        // scope (a sibling column then correlates); at i==0 against `parent` (the enclosing query /
        // params), unchanged (functions.md §10).
        const srfParent = lateralEligible ? lateralParent : parent;
        const r = this.resolveSRF(tref.name, tref.args, tref.alias, srfParent, ctes, ptypes);
        t = r.table;
        srf = r.srf;
        lateral = lateralEligible && r.srf.args.some((a) => rexprReferencesOuter(a, 0));
      } else {
        // A plain FROM name (not an SRF call) may resolve to a CTE, which SHADOWS a catalog table of
        // the same name (cte.md §2); lookup is case-insensitive. A hit bumps the binding's reference
        // count (the inline-vs-materialize decision — cost.md §3).
        const lname = tref.name.toLowerCase();
        const ci = ctes.findIndex((b) => b.name === lname);
        if (ci >= 0) {
          ctes[ci]!.refs += 1;
          t = ctes[ci]!.table;
          cteIdx = ci;
        } else {
          const tbl = this.table(tref.name);
          if (!tbl) throw engineError("undefined_table", "table does not exist: " + tref.name);
          t = tbl;
        }
      }
      // RIGHT/FULL JOIN to a CORRELATED lateral item is rejected (§44): the right side cannot be both
      // kept whole and evaluated per left row. (i ≥ 1 here, so the item carries a join kind.)
      if (lateral && (sel.joins[i - 1]!.kind === "right" || sel.joins[i - 1]!.kind === "full")) {
        throw engineError(
          "invalid_column_reference",
          "invalid reference to FROM-clause entry for a LATERAL item: the combining JOIN type must be INNER or LEFT",
        );
      }
      const label = (tref.alias ?? t.name).toLowerCase();
      // An unaliased derived table (grammar.md §42, PG 18) has an EMPTY label — it has no qualifier,
      // so two of them never collide and the duplicate-label check is skipped (its bare columns still
      // resolve, and stay ambiguous via resolveBare). Every other relation has a non-empty label.
      if (label !== "") {
        if (seenLabels.has(label)) {
          throw engineError("duplicate_alias", "table name " + label + " specified more than once");
        }
        seenLabels.add(label);
      }
      rels.push({ label, table: t, offset, cte: cteIdx });
      srfPlans.push(srf);
      derivedPlans.push(derived);
      lateralFlags.push(lateral);
      offset += t.columns.length;
    }
    const scope = new Scope(rels, this, parent, true, ctes);

    // Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column — grammar.md
    // §18). An unknown column is 42703, an ambiguous bare key 42702. An outer (correlated) key —
    // grouping by an enclosing-query constant — is degenerate and 0A000 (§26).
    const groupKeys: number[] = sel.groupBy.map((key) => {
      const r =
        key.kind === "qualifiedColumn"
          ? scope.resolveQualified(key.qualifier, key.name)
          : scope.resolveBare((key as { name: string }).name);
      if (r.level !== 0) {
        throw engineError("feature_not_supported", "GROUP BY may not reference an outer query column");
      }
      return r.index;
    });

    // An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
    // resolves in collect mode — aggregates collect into synthetic slots and a non-grouped
    // column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
    // mode (columns normal). Output names per grammar.md §8.
    // GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an aggregate
    // query (HAVING alone groups the whole table — grammar.md §19).
    const isAgg = groupKeys.length > 0 || itemsHaveAggregate(sel.items) || sel.having !== null;
    const projAgg: AggCtx = { collecting: isAgg, groupKeys, specs: [] };
    const { nodes: projections, names: columnNames, types: columnTypes } = resolveProjections(scope, sel.items, projAgg, ptypes);
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
    // An outer (correlated) ORDER BY key — ordering by an enclosing-query constant — is degenerate
    // and 0A000 (§26).
    const order: OrderSlot[] = [];
    for (const key of sel.orderBy) {
      const r = key.qualifier !== null
        ? scope.resolveQualified(key.qualifier, key.column)
        : scope.resolveBare(key.column);
      if (r.level !== 0) {
        throw engineError("feature_not_supported", "ORDER BY may not reference an outer query column");
      }
      const idx = r.index;
      let slot = idx;
      if (isAgg) {
        slot = groupKeys.indexOf(idx);
        if (slot < 0) throw groupingErrorColumn(key.column);
      }
      order.push({ idx: slot, descending: key.descending, nullsFirst: key.nullsFirst });
    }

    // SELECT DISTINCT restriction (spec/design/grammar.md §11): each ORDER BY key must appear
    // as a bare/qualified column in the select list (resolved to the same flat index; or the
    // list is `*`). Matches PostgreSQL (42P10). Aliases are invisible to ORDER BY (§8). Only a
    // local match counts as "projected" (an outer reference has no per-row value).
    if (sel.distinct && order.length > 0 && sel.items.kind === "list") {
      const projected = new Set<number>();
      for (const it of sel.items.items) {
        if (it.expr.kind === "column") {
          const r = scope.resolveBare(it.expr.name);
          if (r.level === 0) projected.add(r.index);
        } else if (it.expr.kind === "qualifiedColumn") {
          const r = scope.resolveQualified(it.expr.qualifier, it.expr.name);
          if (r.level === 0) projected.add(r.index);
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
    // changes how unmatched rows are handled in the loop below (§15). The partial scope keeps the
    // same parent chain, so a correlated reference in an ON predicate resolves outward (§26).
    const joins: PlanJoin[] = sel.joins.map((j, k) => {
      if (j.on === null) return { kind: j.kind, on: null };
      const partial = new Scope(scope.rels.slice(0, k + 2), this, parent, true, ctes);
      return { kind: j.kind, on: resolveBooleanFilter(partial, j.on, ptypes) };
    });

    // Primary-key predicate pushdown, per base relation: detect WHERE conjuncts that bound that
    // relation's storage key, so its scan seeks/ranges instead of walking the whole B-tree (cost.md
    // §3 "bounded scan"). The filter is resolved against the full FROM scope, so a relation's PK
    // column is the GLOBAL index rel.offset+pkLocal; isConstSource only accepts a literal/param/outer
    // const (never a sibling column), so a JOIN base table is bounded only by a CONSTANT predicate on
    // its own PK — `b.pk = a.x` (index-nested-loop) stays a full scan, a follow-on. Sound for outer
    // joins too: a non-NULL PK conjunct in WHERE eliminates that relation's NULL-extended rows, so
    // bounding it cannot drop a surviving row. A no-PK relation gets null (full scan).
    // A set-returning relation is a computed row source with no PK/index — it never bounds
    // (functions.md §10), so skip detection for it. A CTE reference is likewise a computed/buffered
    // source with no store PK (cte.md §5), so skip it too.
    const relBounds: (ScanBound | null)[] = rels.map((rel, i) =>
      filter === null || srfPlans[i] !== undefined || rel.cte !== undefined || derivedPlans[i] !== undefined
        ? null
        : detectScanBound(filter, rel),
    );

    // Assemble the owned plan (table NAMES + offsets/widths replace the scope's tables, so the
    // plan outlives the scope and a correlated subquery can re-execute it per row).
    const planRels: PlanRel[] = scope.rels.map((rel, i) => ({
      tableName: rel.table.name,
      offset: rel.offset,
      colCount: rel.table.columns.length,
      srf: srfPlans[i],
      cte: rel.cte,
      derived: derivedPlans[i],
      lateral: lateralFlags[i],
    }));
    // The touched set per relation (cost.md §3 "The touched set"; large-values.md §14): the
    // columns this query statically references, collected depth-aware so a correlated
    // subquery's outer reference back into this scope counts. An aggregate query's projections
    // / HAVING / ORDER BY index the synthetic group row, whose inputs are exactly the group
    // keys + aggregate arguments collected here; a plain query's projections and ORDER BY keys
    // index the combined row directly.
    const totalCols = planRels.reduce((a, r) => a + r.colCount, 0);
    const touched: boolean[] = new Array(totalCols).fill(false);
    if (filter !== null) collectTouched(filter, 0, touched);
    for (const j of joins) if (j.on !== null) collectTouched(j.on, 0, touched);
    if (isAgg) {
      for (const gk of groupKeys) touched[gk] = true;
      for (const s of aggSpecs) if (s.operand !== null) collectTouched(s.operand, 0, touched);
    } else {
      for (const p of projections) collectTouched(p, 0, touched);
      for (const o of order) touched[o.idx] = true;
    }
    const relMasks = planRels.map((r) => touched.slice(r.offset, r.offset + r.colCount));
    return {
      kind: "select",
      rels: planRels,
      joins,
      filter,
      isAgg,
      groupKeys,
      aggSpecs,
      having,
      order,
      projections,
      columnNames,
      columnTypes,
      distinct: sel.distinct,
      limit: sel.limit,
      offset: sel.offset,
      relBounds,
      relMasks,
    };
  }

  // resolveSRF resolves a FROM-clause set-returning function call (generate_series(...)) into a
  // SYNTHETIC one-column relation plus its resolved argument expressions (spec/design/functions.md
  // §10). Only generate_series exists this slice (any other name → 42883), with 2 or 3 integer
  // args (a wrong arity/type → 42883). Non-LATERAL: the args resolve against an EMPTY-local-rels
  // scope whose parent is the enclosing query, so $N and correlated outer columns resolve while a
  // sibling FROM table does not (42703/42P01). The produced column is typed at the PROMOTED
  // integer type of the args (PG); a NULL-typed arg contributes no width. Its NAME follows
  // PostgreSQL's single-column function-alias rule: the table alias when one is given
  // (generate_series(1,5) AS g ⇒ column g), else the function name generate_series.
  private resolveSRF(name: string, args: Expr[], alias: string | null, parent: Scope | null, ctes: CteBinding[], ptypes: ParamTypes): { table: Table; srf: SrfPlan } {
    // The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings are
    // inherited so an arg subquery can reference a CTE (cte.md §2).
    const argScope = new Scope([], this, parent, true, ctes);
    const lname = name.toLowerCase();
    if (lname === "generate_series") return this.resolveGenerateSeries(args, alias, argScope, ptypes);
    if (lname === "unnest") return this.resolveUnnest(args, alias, argScope, ptypes);
    throw engineError("undefined_function", "function does not exist: " + name);
  }

  // resolveGenerateSeries resolves generate_series(start, stop[, step]) (spec/design/functions.md
  // §10): 2 or 3 integer args (a wrong arity/type → 42883). The produced column is typed at the
  // PROMOTED integer type of the args (PG); a NULL-typed arg contributes no width. All-NULL defaults
  // i64.
  private resolveGenerateSeries(args: Expr[], alias: string | null, argScope: Scope, ptypes: ParamTypes): { table: Table; srf: SrfPlan } {
    if (args.length !== 2 && args.length !== 3) throw noFuncOverload("generate_series");
    const forbidden: AggCtx = { collecting: false, groupKeys: [], specs: [] };
    const rargs: RExpr[] = [];
    let result: ScalarType | null = null;
    for (const a of args) {
      const { node, type } = resolve(argScope, a, "i64", forbidden, ptypes);
      if (type.kind === "int") {
        if (result === null || rank(type.ty) > rank(result)) result = type.ty;
      } else if (type.kind === "null") {
        // An untyped NULL/param adapts and contributes no width.
      } else {
        throw noFuncOverload("generate_series");
      }
      rargs.push(node);
    }
    const table = srfTable("generate_series", alias, scalarT(result ?? "i64"));
    return { table, srf: { kind: "generate_series", args: rargs } };
  }

  // resolveUnnest resolves unnest(anyarray) (spec/design/array-functions.md §9, §13): the single
  // argument must be an array (binding ELEM := its element type, the produced column's type), else
  // 42883 (a non-array, e.g. unnest(5)). A bare untyped NULL argument leaves ELEM undeterminable →
  // 42P18 (jed's polymorphic posture, like array_append(NULL, NULL)); a typed NULL array
  // (NULL::i32[]) resolves and yields zero rows at exec. ELEM may be a scalar OR a composite (AF7 —
  // unnest(composite[])): the synthetic column is typed at the bound element type directly
  // (typeFromResolved), so a composite array produces composite rows (an anonymous-composite element
  // has no catalog name → 0A000, not reachable from a typed array).
  private resolveUnnest(args: Expr[], alias: string | null, argScope: Scope, ptypes: ParamTypes): { table: Table; srf: SrfPlan } {
    if (args.length !== 1) throw noFuncOverload("unnest");
    const forbidden: AggCtx = { collecting: false, groupKeys: [], specs: [] };
    const { node, type } = resolve(argScope, args[0]!, null, forbidden, ptypes);
    if (type.kind === "array") {
      const elemTy = typeFromResolved(type.elem);
      const table = srfTable("unnest", alias, elemTy);
      return { table, srf: { kind: "unnest", args: [node] } };
    }
    if (type.kind === "null") throw indeterminatePoly();
    throw noFuncOverload("unnest");
  }

  // generateSeriesRows generates the rows of a generate_series(start, stop[, step]) FROM-clause
  // source (spec/design/functions.md §10), as one-column rows. The args evaluate ONCE against the
  // outer environment with no local row (non-LATERAL). PostgreSQL semantics: any NULL arg → zero
  // rows; a step of zero → 22023; start > stop with a positive step (or the reverse) → zero rows;
  // an i64 overflow while stepping STOPS the series cleanly (no trap). Each generated element
  // charges one generatedRow AT THE SOURCE, guarded so a maxCost ceiling aborts a runaway series
  // (54P01) mid-generation. Note (cross-core parity): TS values are bigint, which does NOT overflow
  // at 64 bits — so the i64 boundary must be detected EXPLICITLY here, or TS would emit rows Rust/Go
  // never reach.
  private generateSeriesRows(srf: SrfPlan, env: EvalEnv, meter: Meter): Row[] {
    const evalInt = (e: RExpr): bigint | null => {
      const v = evalExpr(e, [], env, meter);
      if (v.kind === "int") return v.int;
      if (v.kind === "null") return null;
      throw new Error("the resolver restricts generate_series args to integers");
    };
    const start = evalInt(srf.args[0]!);
    const stop = evalInt(srf.args[1]!);
    const step = srf.args.length === 3 ? evalInt(srf.args[2]!) : 1n;
    // Any NULL argument yields zero rows (PG).
    if (start === null || stop === null || step === null) return [];
    if (step === 0n) throw engineError("invalid_parameter_value", "step size cannot be equal to zero");
    const out: Row[] = [];
    let cur = start;
    for (;;) {
      const inRange = step > 0n ? cur <= stop : cur >= stop;
      if (!inRange) break;
      meter.guard();
      meter.charge(COSTS.generatedRow);
      out.push([intValue(cur)]);
      // i64 overflow while stepping ends the series cleanly, matching PostgreSQL (and Rust/Go).
      const next = cur + step;
      if (next > 9223372036854775807n || next < -9223372036854775808n) break;
      cur = next;
    }
    return out;
  }

  // unnestRows generates the rows of an unnest(anyarray) FROM-clause source (spec/design/array-functions.md
  // §9), as one-column rows. The single array argument evaluates ONCE against the outer environment with no
  // local row (non-LATERAL). PostgreSQL semantics: a NULL array yields zero rows; the empty array {} yields
  // zero rows; otherwise one row per element in flattened row-major order (a multidimensional array flattens;
  // a NULL element is produced as a NULL row). Each produced element charges one generatedRow AT THE SOURCE,
  // guarded so a maxCost ceiling aborts a runaway unnest (54P01) mid-generation, exactly like generate_series.
  private unnestRows(srf: SrfPlan, env: EvalEnv, meter: Meter): Row[] {
    const v = evalExpr(srf.args[0]!, [], env, meter);
    // A NULL array → zero rows (PG; the empty_on_null discipline).
    if (v.kind === "null") return [];
    if (v.kind !== "array") throw new Error("the resolver restricts unnest's argument to an array");
    const out: Row[] = [];
    for (const e of v.elements) {
      meter.guard();
      meter.charge(COSTS.generatedRow);
      out.push([e]);
    }
    return out;
  }

  // execSelectPlan executes a resolved SELECT against an outer-row environment (outer = the
  // enclosing rows, innermost last; empty at top level) and the bound parameters. The execute half
  // of the old runSelect: materialize, nested-loop join, WHERE, then aggregate / DISTINCT / window
  // + project. The per-row evaluator gets an EvalEnv carrying the outer rows + a runSubquery
  // callback, so a correlated subquery in any clause re-executes against them (grammar.md §26).
  // execStreamingLimit executes the LIMIT short-circuit path (spec/design/cost.md §3): a single-table,
  // no-blocking-operator query with a LIMIT streams scan→filter→project and stops the scan the instant
  // the LIMIT/OFFSET window is filled, charging storageRowRead only for the rows actually read.
  // Cost-equivalent to the eager path EXCEPT that it reads (and filters) fewer rows — the deliberate
  // cost change. pageRead is the full block (the bound's node count); only the row reads short-circuit.
  // Rows match the eager path exactly: the offset..offset+limit slice of the primary-key-ordered
  // filtered rows.
  private execStreamingLimit(plan: SelectPlan, env: EvalEnv, meter: Meter, params: Value[]): SelectResult {
    const store = this.readSnap().store(plan.rels[0]!.tableName);

    // Resolve the scan bound (the PK pushdown, if any) and charge the pageRead block. This path is
    // single-table (gated below), so the only relation is relBounds[0]. A correlated bound resolves
    // against env.outer (the enclosing rows).
    // An INDEX bound never streams — the dispatch gate routes it to the eager path
    // (cost.md §3 "LIMIT short-circuit").
    let bound: KeyBound = unboundedBound();
    let empty = false;
    const sb = plan.relBounds[0]!;
    if (sb !== null) {
      if (sb.kind === "index") throw new Error("the streaming path is gated to PK/full scans");
      const b = buildKeyBound(sb.pk, params, env.outer);
      if (b === null) empty = true;
      else bound = b;
    }
    const su = empty ? { pages: 0, slabs: 0 } : store.overlapScanUnits(bound, plan.relMasks[0]!);
    meter.charge(COSTS.pageRead * BigInt(su.pages) + COSTS.valueDecompress * BigInt(su.slabs));

    const limit = plan.limit!;
    const offset = plan.offset ?? 0n;
    const out: Value[][] = [];
    if (!empty && limit > 0n) {
      let passed = 0n;
      store.scanRange(bound, (_key, rawRow) => {
        meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
        meter.charge(COSTS.storageRowRead);
        // Materialize the touched columns if the lazy load left them unfetched
        // (large-values.md §14) — a fresh copy only when needed (resolveColumns).
        const row = store.resolveColumns(rawRow, plan.relMasks[0]!);
        if (plan.filter !== null && !isTrue(evalExpr(plan.filter, row, env, meter))) {
          return true;
        }
        passed += 1n;
        if (passed <= offset) return true;
        meter.charge(COSTS.rowProduced);
        out.push(plan.projections.map((p) => evalExpr(p, row, env, meter)));
        return BigInt(out.length) < limit; // stop once the window is filled
      });
    }
    return { columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.accrued };
  }

  // execStreamingSort is the streaming external sort for a single-table ORDER BY (spec/design/spill.md
  // §4/§5). It streams scan→filter→sorter, so the input is never materialized in the executor heap;
  // the sorter spills sorted runs to disk under workMem (file-backed databases) and k-way-merges them
  // at finish, then the window/projection loop pulls the sorted rows one at a time. Results + cost are
  // byte-identical to the eager sort: the same pageRead block, storageRowRead per scanned row, filter
  // operator_eval, and rowProduced per windowed row accrue — only the sort, which is unmetered
  // (cost.md §3), now spills. Gated (by the caller) to a single table, no join, non-aggregate,
  // non-DISTINCT, with an ORDER BY and no index bound.
  private execStreamingSort(plan: SelectPlan, env: EvalEnv, meter: Meter, params: Value[]): SelectResult {
    const store = this.readSnap().store(plan.rels[0]!.tableName);

    // Resolve the scan bound (the PK pushdown, if any) and charge the pageRead + valueDecompress block
    // up front — identical to the eager scan (cost.md §3). An INDEX bound never reaches here.
    let bound: KeyBound = unboundedBound();
    let empty = false;
    const sb = plan.relBounds[0]!;
    if (sb !== null) {
      if (sb.kind === "index") throw new Error("the streaming sort path is gated to PK/full scans");
      const b = buildKeyBound(sb.pk, params, env.outer);
      if (b === null) empty = true;
      else bound = b;
    }
    const su = empty ? { pages: 0, slabs: 0 } : store.overlapScanUnits(bound, plan.relMasks[0]!);
    meter.charge(COSTS.pageRead * BigInt(su.pages) + COSTS.valueDecompress * BigInt(su.slabs));

    // Stream the scan → filter → sorter. ORDER BY is blocking, so the scan never short-circuits: every
    // in-range row is read (charging storageRowRead), its touched columns resolved (large-values.md
    // §14), the WHERE applied (charging operator_eval), and a survivor pushed into the sorter, which
    // spills when it exceeds the budget.
    const sorter = this.newSorterFor(plan.order);
    if (!empty) {
      store.scanRange(bound, (_key, rawRow) => {
        meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
        meter.charge(COSTS.storageRowRead);
        const row = store.resolveColumns(rawRow, plan.relMasks[0]!);
        if (plan.filter !== null && !isTrue(evalExpr(plan.filter, row, env, meter))) {
          return true;
        }
        sorter.push(row);
        return true; // never stop early — the sort must see every row
      });
    }

    // LIMIT / OFFSET window over the sort's total row count (known without materializing the output).
    // Clamp in the bigint domain before indexing (CLAUDE.md §8).
    const total = BigInt(sorter.total);
    const offset = plan.offset ?? 0n;
    const start = offset < total ? offset : total;
    let end = total;
    if (plan.limit !== null && plan.limit < total - start) end = start + plan.limit;

    const sorted = sorter.finish();
    try {
      for (let i = 0n; i < start; i++) sorted.next(); // skip the OFFSET rows (unwindowed)
      const out: Value[][] = [];
      for (let i = start; i < end; i++) {
        const row = sorted.next();
        if (row === null) break;
        meter.guard(); // enforce the cost ceiling per produced row (CLAUDE.md §13)
        meter.charge(COSTS.rowProduced);
        out.push(plan.projections.map((p) => evalExpr(p, row, env, meter)));
      }
      return { columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.accrued };
    } finally {
      sorted.close(); // a LIMIT may stop the merge early — release any undrained run files
    }
  }

  // newSorterFor builds a Sorter for order, bounded by this handle's workMem. Spilling is enabled only
  // when a spillSink is present — a durable host that can spill to disk sets one (file.ts →
  // FileSpillSink, writing runs next to the database file); an in-memory or OPFS database leaves it
  // null and sorts fully resident (spill.md §2).
  private newSorterFor(order: OrderSlot[]): Sorter {
    const compare: RowCompare = (a, b) => {
      for (const k of order) {
        const c = keyCmp(a[k.idx]!, b[k.idx]!, k.descending, k.nullsFirst);
        if (c !== 0) return c;
      }
      return 0;
    };
    return new Sorter(compare, this.workMem, this.spillSink);
  }

  // materializeRel materializes one FROM relation ri into its rows, given the current outer-row stack
  // `outer` (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
  // outer); an SRF is generated; a CTE / derived table is delivered / run in place. For a CORRELATED
  // LATERAL relation (§44) the caller passes outer EXTENDED with the combined left-hand row, so the
  // body / SRF args read that row as their immediate outer; a non-lateral relation is passed the
  // query's own outer and its parent=null body simply ignores it (a parent=null plan holds no
  // outerColumn, so the two are observably identical).
  private materializeRel(plan: SelectPlan, ri: number, outer: Row[], baseEnv: EvalEnv, params: Value[], meter: Meter): Row[] {
    const rel = plan.rels[ri]!;
    const env: EvalEnv = { ...baseEnv, outer };
    // A set-returning relation is generated, not scanned (functions.md §10): produce its rows,
    // charging generated_row per element (its args read outer — implicitly lateral, §44).
    if (rel.srf !== undefined) {
      return rel.srf.kind === "unnest"
        ? this.unnestRows(rel.srf, env, meter)
        : this.generateSeriesRows(rel.srf, env, meter);
    }
    // A CTE reference delivers its rows from the per-statement context (cte.md §3/§5): a MATERIALIZED
    // CTE reads its buffer (charging cte_scan_row, guarded so a runaway scan aborts 54P01); an INLINE
    // CTE runs its body in place. (A CTE is never lateral.)
    if (rel.cte !== undefined) {
      const ci = rel.cte;
      if (env.ctes.modes[ci] === "materialize") {
        const buf = env.ctes.buffers[ci]!;
        for (let i = 0; i < buf.length; i++) {
          meter.guard();
          meter.charge(COSTS.cteScanRow);
        }
        return buf.slice();
      }
      const r = this.execQueryPlan(env.ctes.plans[ci]!, outer, params, env.ctes);
      meter.charge(r.cost);
      return r.rows;
    }
    // A DERIVED TABLE runs its body in place (grammar.md §42), charging its intrinsic cost — no
    // cte_scan_row. Non-lateral it was planned parent=null and ignores outer; a LATERAL body (§44)
    // reads the left-hand row from outer.
    if (rel.derived !== undefined) {
      const r = this.execQueryPlan(rel.derived, outer, params, env.ctes);
      meter.charge(r.cost);
      return r.rows;
    }
    // A base table: scan in primary-key order via a scanSource (the page_read block + per-row
    // storage_row_read accrue inside the generator — cost.md §3). A PK/index bound seeks/ranges
    // instead of a full walk; an empty bound reads nothing.
    const store = this.readSnap().store(rel.tableName);
    let rows: Row[];
    let nodeCount: number;
    let slabs = 0;
    const relBound = plan.relBounds[ri]!;
    if (relBound !== null && relBound.kind === "index") {
      const r = this.indexBoundRows(rel.tableName, relBound.index, params, outer, plan.relMasks[ri]!);
      rows = r.rows;
      nodeCount = r.pages;
      slabs = r.slabs;
    } else if (relBound !== null) {
      const b = buildKeyBound(relBound.pk, params, outer);
      if (b === null) {
        rows = [];
        nodeCount = 0;
      } else {
        const u = store.rangeScanWithUnits(b, plan.relMasks[ri]!);
        rows = u.entries.map((e) => e.row);
        nodeCount = u.pages;
        slabs = u.slabs;
      }
    } else {
      const u = store.scanWithUnits(plan.relMasks[ri]!);
      rows = u.entries.map((e) => e.row);
      nodeCount = u.pages;
      slabs = u.slabs;
    }
    // Materialize this relation's touched columns where the lazy load left unfetched references
    // (large-values.md §14) — exactly the static set the cost block charges.
    for (let i = 0; i < rows.length; i++) {
      rows[i] = store.resolveColumns(rows[i]!, plan.relMasks[ri]!);
    }
    meter.charge(COSTS.valueDecompress * BigInt(slabs));
    const tableRows: Row[] = [];
    for (const row of scanSource(rows, nodeCount, meter)) {
      tableRows.push(row);
    }
    return tableRows;
  }

  private execSelectPlan(plan: SelectPlan, outer: Row[], params: Value[], ctes: CteCtx): SelectResult {
    const env: EvalEnv = {
      params,
      outer,
      // A subquery inherits the same CTE context (a CTE reference works at any nesting depth —
      // cte.md §2/§5).
      runSubquery: (p, o) => this.execQueryPlan(p, o, params, ctes),
      seam: this.seam,
      rng: new StmtRng(),
      ctes,
      exec: this,
    };
    const meter = new Meter(this.maxCost);

    // LIMIT short-circuit (spec/design/cost.md §3): a single-table query with a LIMIT and no blocking
    // operator (no join, aggregate, DISTINCT, or ORDER BY) streams scan→filter→project and STOPS the
    // scan once the window is filled, so storageRowRead counts only the rows actually read. (ORDER BY/
    // DISTINCT/aggregate must see every row, so they keep the eager path below.) pageRead stays the
    // full block; only row reads short-circuit.
    if (
      plan.limit !== null &&
      plan.rels.length === 1 &&
      plan.joins.length === 0 &&
      !plan.isAgg &&
      !plan.distinct &&
      plan.order.length === 0 &&
      // An index-bounded scan does not stream (cost.md §3 "index-bounded scan"): it
      // reads the full admitted set via the eager path below.
      plan.relBounds[0]?.kind !== "index" &&
      // A set-returning relation is generated, not scanned — it takes the eager path
      // (functions.md §10); the streaming reader assumes a table store.
      plan.rels[0]!.srf === undefined &&
      // A CTE reference is a computed/buffered source, not a table store — the eager path
      // (cte.md §5) delivers its rows; the streaming reader assumes a store.
      plan.rels[0]!.cte === undefined &&
      // A derived table is a computed source too (grammar.md §42) — eager path.
      plan.rels[0]!.derived === undefined
    ) {
      return this.execStreamingLimit(plan, env, meter, params);
    }

    // Streaming external sort (spec/design/spill.md §5): a single-table, no-join, non-aggregate,
    // non-DISTINCT query with an ORDER BY streams scan→filter→sorter, so the input is never
    // materialized in the executor heap and the sort spills sorted runs to disk under workMem
    // (file-backed databases). DISTINCT/aggregate/join take the eager path below, and an index bound
    // does not stream (like the LIMIT short-circuit). Results + cost are identical to the eager sort
    // (the sort is unmetered — cost.md §3; spill.md §6).
    if (
      plan.order.length > 0 &&
      plan.rels.length === 1 &&
      plan.joins.length === 0 &&
      !plan.isAgg &&
      !plan.distinct &&
      plan.relBounds[0]?.kind !== "index" &&
      // A set-returning relation takes the eager path (functions.md §10).
      plan.rels[0]!.srf === undefined &&
      // A CTE reference takes the eager path (cte.md §5).
      plan.rels[0]!.cte === undefined &&
      // A derived table takes the eager path (grammar.md §42).
      plan.rels[0]!.derived === undefined
    ) {
      return this.execStreamingSort(plan, env, meter, params);
    }

    // Materialize each relation once, in primary-key order (base tables drain a scanSource — the
    // page_read block + per-row storage_row_read accrue inside the generator, cost.md §3). The nested
    // loop re-reads from these in-memory buffers, which are not stores and charge nothing. A
    // CORRELATED LATERAL relation (§44) depends on the left-hand row, so it cannot be materialized up
    // front — an empty placeholder holds its slot and the join loop re-materializes it per left row.
    const materialized: Row[][] = plan.rels.map((rel, ri) =>
      rel.lateral === true ? [] : this.materializeRel(plan, ri, outer, env, params, meter),
    );

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
    // A FROM-less SELECT has no relations: seed `running` with ONE virtual zero-column row
    // instead of a table's rows (grammar.md §34). No scan ran, so no scan cost accrued.
    let running: Row[] = plan.rels.length === 0 ? [[]] : materialized[0]!;
    for (let k = 0; k < plan.joins.length; k++) {
      const on = plan.joins[k]!.on;
      const kind = plan.joins[k]!.kind;
      const emitLeft = kind === "left" || kind === "full";
      const emitRight = kind === "right" || kind === "full";
      // NULL-pad widths come from the PLAN, never a sampled row, so they are correct even when
      // `running`/`rightRows` is empty: the right table begins at flat offset rels[k+1].offset
      // (= the width of every running row) and is that many columns wide.
      const leftPad = plan.rels[k + 1]!.offset;
      const rightPad = plan.rels[k + 1]!.colCount;
      const next: Row[] = [];
      // A CORRELATED LATERAL relation (§44): re-materialize it ONCE PER combined left-hand row, with
      // that row pushed onto the outer-row stack as the body's immediate outer (the correlated-
      // subquery mechanism). The plan guarantees INNER/CROSS/LEFT here (RIGHT/FULL to a correlated
      // lateral is 42P10), so there is no unmatched-right emission.
      if (plan.rels[k + 1]!.lateral === true) {
        for (const left of running) {
          const rightRows = this.materializeRel(plan, k + 1, [...outer, left], env, params, meter);
          let leftMatched = false;
          for (const right of rightRows) {
            const combined = left.concat(right);
            if (on === null || isTrue(evalExpr(on, combined, env, meter))) {
              next.push(combined);
              leftMatched = true;
            }
          }
          if (emitLeft && !leftMatched) next.push(left.concat(nullRow(rightPad)));
        }
        running = next;
        continue;
      }
      const rightRows = materialized[k + 1]!;
      const rightMatched: boolean[] = new Array(rightRows.length).fill(false);
      for (const left of running) {
        let leftMatched = false;
        for (let ri = 0; ri < rightRows.length; ri++) {
          const combined = left.concat(rightRows[ri]!);
          if (on === null || isTrue(evalExpr(on, combined, env, meter))) {
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
      if (plan.filter === null || isTrue(evalExpr(plan.filter, row, env, meter))) rows.push(row);
    }

    // ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
    // and a full tie keeps the scan order (JS Array#sort is stable). Each key's NULL placement
    // is decoupled from its value-direction flip (spec/design/grammar.md §10). Aggregate queries
    // sort their GROUP rows in the aggregate branch below — not these pre-aggregation rows — so
    // this is gated to plain queries.
    if (!plan.isAgg && plan.order.length > 0) {
      rows.sort((a, b) => {
        for (const key of plan.order) {
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
      const start = plan.offset === null ? 0n : plan.offset < n ? plan.offset : n;
      const end = plan.limit !== null && plan.limit < n - start ? start + plan.limit : n;
      return [Number(start), Number(end)];
    };

    // Build the output rows. The two paths differ in pipeline order
    // (spec/design/grammar.md §11): without DISTINCT the window slices the sorted source
    // rows and ONLY the windowed rows are projected; with DISTINCT every (sorted) filtered
    // row is projected — dedup must see them all — duplicates drop by first occurrence, and
    // the window then slices the DISTINCT rows.
    let out: Value[][];
    if (plan.isAgg) {
      // Aggregate query — group + accumulate (aggregates.md §5). Bucket the post-WHERE rows by
      // their group-key values; the bucket key is the value-canonical distinctRowKey (it
      // collapses 1.5/1.50 and groups NULL with NULL), and the Map is only an index — output
      // order comes from the insertion-ordered `groups`, never Map iteration (no order leak —
      // §8/§10). Whole-table aggregation (no GROUP BY) is one pre-created empty-key group, so it
      // emits ONE row even over zero input; GROUP BY over an empty table creates no groups ->
      // zero rows. Each (row × aggregate) charges aggregateAccumulate; the bucketing/finalize is
      // unmetered (cost.md §3).
      const newAccs = (): Acc[] => plan.aggSpecs.map((s) => newAcc(s.plan, s.floatWidth ?? "f64"));
      const index = new Map<string, number>();
      const groups: { keys: Value[]; accs: Acc[] }[] = [];
      if (plan.groupKeys.length === 0) {
        groups.push({ keys: [], accs: newAccs() });
        index.set("", 0);
      }
      for (const row of rows) {
        meter.guard(); // enforce the cost ceiling per folded row (CLAUDE.md §13)
        const keys = plan.groupKeys.map((gk) => row[gk]!);
        const k = distinctRowKey(keys);
        let gi = index.get(k);
        if (gi === undefined) {
          gi = groups.length;
          index.set(k, gi);
          groups.push({ keys, accs: newAccs() });
        }
        const accs = groups[gi]!.accs;
        plan.aggSpecs.forEach((spec, i) => {
          meter.charge(COSTS.aggregateAccumulate);
          const v = spec.operand === null ? nullValue() : evalExpr(spec.operand, row, env, meter);
          foldAcc(accs[i]!, v, meter);
        });
      }
      // Build one synthetic row per group: [group_key_values..., aggregate_results...].
      let groupRows = groups.map((g) => [...g.keys, ...g.accs.map((a) => finalizeAcc(a))]);
      // HAVING: filter the grouped rows (after aggregation, before ORDER BY). The predicate is
      // evaluated against each group's synthetic row (charging its operatorEvals per group);
      // only a TRUE result keeps the group. A dropped group charges no rowProduced (§8).
      if (plan.having !== null) {
        groupRows = groupRows.filter((srow) => isTrue(evalExpr(plan.having!, srow, env, meter)));
      }
      // ORDER BY over the grouped output (keys are synthetic group-key slots).
      if (plan.order.length > 0) {
        groupRows.sort((a, b) => {
          for (const key of plan.order) {
            const c = keyCmp(a[key.idx]!, b[key.idx]!, key.descending, key.nullsFirst);
            if (c !== 0) return c;
          }
          return 0;
        });
      }
      // Window + project; only an emitted row charges rowProduced + its projection cost.
      const [start, end] = windowBounds(groupRows.length);
      out = groupRows.slice(start, end).map((srow) => {
        meter.guard(); // enforce the cost ceiling per produced row (CLAUDE.md §13)
        meter.charge(COSTS.rowProduced);
        return plan.projections.map((p) => evalExpr(p, srow, env, meter));
      });
    } else if (plan.distinct) {
      // Project every filtered row (charging projection cost per row, the §3 asymmetry),
      // keeping first occurrences. `seen` is membership-only: output order comes from the
      // deterministic source iteration, never from Set iteration (no order leak — §8/§10).
      const seen = new Set<string>();
      const distinctRows: Value[][] = [];
      for (const row of rows) {
        const tuple = plan.projections.map((p) => evalExpr(p, row, env, meter));
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
        meter.guard(); // enforce the cost ceiling per produced row (CLAUDE.md §13)
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
        meter.guard(); // enforce the cost ceiling per produced row (CLAUDE.md §13)
        meter.charge(COSTS.rowProduced);
        return plan.projections.map((p) => evalExpr(p, row, env, meter));
      });
    }
    // The scan/eval cost (correlated subqueries fold their per-row cost in via the evaluator;
    // globally-uncorrelated ones are folded once before exec, their cost added at runQueryExpr).
    return { columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.accrued };
  }

  // ---- Uncorrelated subquery folding (spec/design/grammar.md §26) ----------------------
  //
  // After the whole statement tree is planned + the parameters bound, this bottom-up pass walks
  // every "subquery" RExpr node in the plan tree: it first folds within the node's own sub-plan,
  // then — if the subquery references NO enclosing scope (a global constant, PG's "initplan") —
  // executes it ONCE and replaces it with a constant (scalar -> its value; EXISTS -> a boolean; IN
  // -> an "inValues" over the result column), accruing the subquery's cost once (preserving the
  // committed once-only cost — cost.md §3). A CORRELATED subquery is left in place; the evaluator
  // re-executes it per outer row. So after this pass the only surviving "subquery" nodes are correlated.

  private foldUncorrelatedInPlan(plan: QueryPlan, bound: Value[], ctes: CteCtx, cost: { value: bigint }): void {
    if (plan.kind === "select") {
      this.foldUncorrelatedInSelect(plan, bound, ctes, cost);
      return;
    }
    if (plan.kind === "values") {
      // A VALUES-body value may itself hold an (uncorrelated) scalar subquery to fold once before
      // the rows are produced (grammar.md §42; the §26 fold).
      for (const row of plan.rows) {
        for (let c = 0; c < row.length; c++) {
          row[c] = this.foldUncorrelatedInRExpr(row[c]!, bound, ctes, cost);
        }
      }
      return;
    }
    this.foldUncorrelatedInPlan(plan.lhs, bound, ctes, cost);
    this.foldUncorrelatedInPlan(plan.rhs, bound, ctes, cost);
  }

  private foldUncorrelatedInSelect(sp: SelectPlan, bound: Value[], ctes: CteCtx, cost: { value: bigint }): void {
    for (const j of sp.joins) if (j.on !== null) j.on = this.foldUncorrelatedInRExpr(j.on, bound, ctes, cost);
    if (sp.filter !== null) sp.filter = this.foldUncorrelatedInRExpr(sp.filter, bound, ctes, cost);
    if (sp.having !== null) sp.having = this.foldUncorrelatedInRExpr(sp.having, bound, ctes, cost);
    for (const s of sp.aggSpecs) {
      if (s.operand !== null) s.operand = this.foldUncorrelatedInRExpr(s.operand, bound, ctes, cost);
    }
    sp.projections = sp.projections.map((p) => this.foldUncorrelatedInRExpr(p, bound, ctes, cost));
    // A set-returning relation's arguments may themselves contain an (uncorrelated) subquery to
    // fold once before the generator runs (functions.md §10).
    for (const rel of sp.rels) {
      if (rel.srf !== undefined) {
        rel.srf.args = rel.srf.args.map((a) => this.foldUncorrelatedInRExpr(a, bound, ctes, cost));
      }
    }
  }

  // foldUncorrelatedInRExpr folds this node if it is an uncorrelated "subquery", else recurses into
  // its children. It RETURNS the (possibly replaced) node; the caller reassigns the field.
  private foldUncorrelatedInRExpr(e: RExpr, bound: Value[], ctes: CteCtx, cost: { value: bigint }): RExpr {
    switch (e.kind) {
      case "subquery": {
        // Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
        // globally-uncorrelated subquery nested inside it is already a constant before we run it.
        if (e.lhs !== null) e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost);
        this.foldUncorrelatedInPlan(e.plan, bound, ctes, cost);
        if (queryPlanReferencesOuter(e.plan, 0)) return e; // correlated — re-run per outer row
        // Uncorrelated: execute ONCE and fold to a constant / inValues.
        const r = this.execQueryPlan(e.plan, [], bound, ctes);
        cost.value += r.cost;
        if (e.subKind === "scalar") {
          if (r.rows.length > 1) {
            throw engineError(
              "cardinality_violation",
              "more than one row returned by a subquery used as an expression",
            );
          }
          return valueToRExpr(r.rows.length === 1 ? r.rows[0]![0]! : nullValue());
        }
        if (e.subKind === "exists") {
          return { kind: "constBool", value: r.rows.length > 0 !== e.negated };
        }
        if (e.subKind === "quantified") {
          // An uncorrelated quantified subquery folds to a constant-array `quantified`
          // (array-functions.md §11.6): its single column becomes a 1-D array and the node reuses
          // the array form's 3VL fold — no per-row re-execution.
          const elements = r.rows.map((row) => row[0]!);
          return {
            kind: "quantified",
            op: e.op!,
            all: e.all!,
            lhs: e.lhs!,
            array: valueToRExpr(arrayValue(elements)),
          };
        }
        // in
        const list = r.rows.map((row) => row[0]!);
        return { kind: "inValues", lhs: e.lhs!, list, negated: e.negated };
      }
      case "cast":
      case "neg":
      case "not":
      case "isNull":
        e.operand = this.foldUncorrelatedInRExpr(e.operand, bound, ctes, cost);
        return e;
      case "arith":
      case "compare":
      case "and":
      case "or":
      case "distinct":
      case "like":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost);
        e.rhs = this.foldUncorrelatedInRExpr(e.rhs, bound, ctes, cost);
        return e;
      case "case":
        e.arms = e.arms.map((arm) => ({
          cond: this.foldUncorrelatedInRExpr(arm.cond, bound, ctes, cost),
          result: this.foldUncorrelatedInRExpr(arm.result, bound, ctes, cost),
        }));
        e.els = this.foldUncorrelatedInRExpr(e.els, bound, ctes, cost);
        return e;
      case "scalarFunc":
      case "arrayFunc":
      case "variadic":
        e.args = e.args.map((a) => this.foldUncorrelatedInRExpr(a, bound, ctes, cost));
        return e;
      case "row":
        e.fields = e.fields.map((f) => this.foldUncorrelatedInRExpr(f, bound, ctes, cost));
        return e;
      case "array":
        e.elements = e.elements.map((el) => this.foldUncorrelatedInRExpr(el, bound, ctes, cost));
        return e;
      case "field":
        e.base = this.foldUncorrelatedInRExpr(e.base, bound, ctes, cost);
        return e;
      case "subscript":
        e.base = this.foldUncorrelatedInRExpr(e.base, bound, ctes, cost);
        e.subscripts = e.subscripts.map((s) =>
          s.isSlice
            ? {
                isSlice: true,
                lower: s.lower === null ? null : this.foldUncorrelatedInRExpr(s.lower, bound, ctes, cost),
                upper: s.upper === null ? null : this.foldUncorrelatedInRExpr(s.upper, bound, ctes, cost),
              }
            : { isSlice: false, index: this.foldUncorrelatedInRExpr(s.index, bound, ctes, cost) },
        );
        return e;
      case "inValues":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost);
        return e;
      case "quantified":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost);
        e.array = this.foldUncorrelatedInRExpr(e.array, bound, ctes, cost);
        return e;
      default:
        return e; // leaves: column, outerColumn, param, const*
    }
  }
}

// scanSource streams a base table's already-materialized rows as a pull-based generator — the
// Volcano seam the streaming + point-lookup work (TODO Phase 6) builds on. The caller decides full
// scan vs primary-key bound (passing the matching rows + the visited-node count); this generator just
// charges the pageRead block (one per visited B-tree node — spec/design/cost.md §3 "page_read") once,
// before the first row, then storageRowRead per row yielded: the same units in the same order as the
// inline scan loop it replaced. The block fires before any row even for an empty scan (nodeCount 0 ⇒ a
// no-op charge, driven by the consuming for-of's first next()), so the accrued total never moves. The
// consumer drains it fully in this slice (no short-circuit), keeping the laziness unobservable and the
// cost identical to Go/Rust.
function* scanSource(rows: Row[], nodeCount: number, meter: Meter): Generator<Row> {
  meter.charge(COSTS.pageRead * BigInt(nodeCount));
  for (const row of rows) {
    // Enforce the cost ceiling before pulling the next row (CLAUDE.md §13): a runaway scan (or a
    // JOIN/correlated re-scan built on this source) stops deterministically once accrued cost
    // reaches the limit. No-op when unlimited (spec/design/cost.md §6).
    meter.guard();
    meter.charge(COSTS.storageRowRead);
    yield row;
  }
}

// ---- Primary-key predicate pushdown (spec/design/cost.md §3 "bounded scan / point lookup") ----
//
// A single-table WHERE on the primary key bounds the storage-key range a scan must visit. Detection is
// two-stage: detectPkBound at plan time (structural — which conjuncts are PK comparisons), buildKeyBound
// at exec time (the const values, and any $N, are known only then). The bound is a SUPERSET of the
// matching keys: the whole WHERE stays the residual filter (re-applied to each scanned row), so the
// result is always correct — the bound only narrows which rows are scanned, and page_read/storageRowRead
// drop to what it touches. The unbounded case keeps the full scan, so its cost never moves.

// ScanBound is a per-relation scan bound (cost.md §3): a primary-key range, or a
// secondary-index equality (spec/design/indexes.md §5). The PK bound wins when both apply
// (it is the row's own key — no second tree, range-capable, strictly cheaper).
type ScanBound = { kind: "pk"; pk: PkBound } | { kind: "index"; index: IndexBound };

// IndexBound is the plan-time result of index analysis (indexes.md §5): the chosen index
// (lowest lowercased name whose FIRST key column has an equality conjunct), that column's
// storage type, and every equality const-source on it. At exec time the sources must
// agree on one value (else the bound is provably empty) and the index is range-scanned
// over that value's presence-tagged prefix.
// tailTypes is the REMAINING key components' types (columns[1..]): an admitted entry's
// row-key suffix sits after every component slot, so the fetch skips these (each slot is
// self-delimiting — a 0x01 NULL tag alone, or 0x00 + the type's fixed width).
type IndexBound = { nameKey: string; colType: ScalarType; eqs: RExpr[]; tailTypes: ScalarType[] };

// detectScanBound picks one relation's scan bound (cost.md §3; indexes.md §5): the
// single-column PK bound first; else, among the relation's indexes (held in ascending
// lowercased-name order — the deterministic tie-break), the first whose FIRST key column
// has at least one equality conjunct against a type-matched const-source; else null
// (full scan).
function detectScanBound(filter: RExpr, rel: ScopeRel): ScanBound | null {
  const pkLocal = primaryKeyIndex(rel.table);
  if (pkLocal >= 0) {
    const bp = detectPkBound(filter, rel.offset + pkLocal, typeScalar(rel.table.columns[pkLocal]!.type));
    if (bp !== null) return { kind: "pk", pk: bp };
  }
  for (const idx of rel.table.indexes) {
    const ci = idx.columns[0]!;
    const ty = typeScalar(rel.table.columns[ci]!.type);
    const bp = detectPkBound(filter, rel.offset + ci, ty);
    const eqs: RExpr[] = [];
    if (bp !== null) {
      for (const t of bp.terms) if (t.op === "eq") eqs.push(t.src);
    }
    if (eqs.length > 0) {
      const tailTypes = idx.columns.slice(1).map((c) => typeScalar(rel.table.columns[c]!.type));
      return { kind: "index", index: { nameKey: idx.name.toLowerCase(), colType: ty, eqs, tailTypes } };
    }
  }
  return null;
}

// indexEntryKey builds a secondary-index entry key (spec/design/indexes.md §3): each
// indexed column as the encoding.md §2.2 nullable slot — 0x00 + the type's bare
// order-preserving key bytes when present, the lone 0x01 for NULL (always tagged, even
// for a NOT NULL column) — then the row's storage key as the suffix. Indexable types are
// fixed-width and never spill, so the values are always resident (never unfetched).
function indexEntryKey(columns: Column[], def: IndexDef, storageKey: Uint8Array, row: Row): Uint8Array {
  const parts: Uint8Array[] = [];
  for (const ci of def.columns) {
    const v = row[ci]!;
    if (v.kind === "null") {
      parts.push(Uint8Array.of(0x01));
    } else if (v.kind === "int") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.int));
    } else if (v.kind === "bool") {
      parts.push(Uint8Array.of(0x00), encodeBool(v.value));
    } else if (v.kind === "uuid") {
      parts.push(Uint8Array.of(0x00), v.bytes);
    } else if (v.kind === "timestamp" || v.kind === "timestamptz") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.micros));
    } else if (v.kind === "date") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.days));
    } else {
      throw new Error("an index column is a key-encodable type (CREATE INDEX gate)");
    }
  }
  parts.push(storageKey);
  const total = parts.reduce((acc, b) => acc + b.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const b of parts) {
    out.set(b, off);
    off += b.length;
  }
  return out;
}

// indexEntryKeys returns the index entries a row contributes (spec/design/gin.md §4/§5): exactly
// one for an ordered (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL
// element for a GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index
// uniformly as "a row maps to a set of entries."
function indexEntryKeys(columns: Column[], def: IndexDef, storageKey: Uint8Array, row: Row): Uint8Array[] {
  if (def.kind === "gin") return ginEntries(columns, def, storageKey, row);
  return [indexEntryKey(columns, def, storageKey, row)];
}

// ginEntries builds a GIN index's entry keys for one row (spec/design/gin.md §4): one entry per
// DISTINCT non-NULL array element — encode(element) ‖ storage_key, NO presence tag (a term is never
// NULL) and an empty payload. A NULL array column value and an empty array yield no entries (so
// they appear in no posting list). Returned sorted by term (= encoded-byte order for the integer
// element types). This slice: a single integer-element array column.
function ginEntries(columns: Column[], def: IndexDef, storageKey: Uint8Array, row: Row): Uint8Array[] {
  const ci = def.columns[0]!;
  const colType = columns[ci]!.type;
  if (colType.kind !== "array") throw new Error("a GIN index column is an array (CREATE INDEX gate)");
  const elemTy = typeScalar(colType.elem);
  const v = row[ci]!;
  if (v.kind !== "array") return [];
  const seen = new Set<bigint>();
  const vals: bigint[] = [];
  for (const el of v.elements) {
    if (el.kind !== "int") continue; // a NULL element (or any non-integer — impossible) is no term
    if (!seen.has(el.int)) {
      seen.add(el.int);
      vals.push(el.int);
    }
  }
  vals.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return vals.map((n) => {
    const term = encodeInt(elemTy, n);
    const out = new Uint8Array(term.length + storageKey.length);
    out.set(term, 0);
    out.set(storageKey, term.length);
    return out;
  });
}

// bytesDiff returns the entries in a that are not in b (set difference over byte strings),
// preserving a's order — the UPDATE symmetric-difference for GIN / B-tree maintenance (gin.md §5).
function bytesDiff(a: Uint8Array[], b: Uint8Array[]): Uint8Array[] {
  return a.filter((x) => !b.some((y) => bytesEq(x, y)));
}

// indexPrefixKey builds a row's UNIQUENESS PROBE KEY for one unique index
// (spec/design/indexes.md §8): the §3 entry key's slot prefix — without the storage-key
// suffix — or null when any component is NULL (NULLS DISTINCT: such a tuple never
// conflicts). Two rows conflict iff they yield the same non-null prefix.
function indexPrefixKey(columns: Column[], def: IndexDef, row: Row): Uint8Array | null {
  const parts: Uint8Array[] = [];
  for (const ci of def.columns) {
    const v = row[ci]!;
    if (v.kind === "null") {
      return null;
    } else if (v.kind === "int") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.int));
    } else if (v.kind === "bool") {
      parts.push(Uint8Array.of(0x00), encodeBool(v.value));
    } else if (v.kind === "uuid") {
      parts.push(Uint8Array.of(0x00), v.bytes);
    } else if (v.kind === "timestamp" || v.kind === "timestamptz") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.micros));
    } else if (v.kind === "date") {
      parts.push(Uint8Array.of(0x00), encodeInt(typeScalar(columns[ci]!.type), v.days));
    } else {
      throw new Error("an index column is a key-encodable type (CREATE INDEX gate)");
    }
  }
  const total = parts.reduce((acc, b) => acc + b.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const b of parts) {
    out.set(b, off);
    off += b.length;
  }
  return out;
}

// uniqueProbeBound is the half-open byte range [prefix, byte-successor(prefix)) — every
// index entry whose slot prefix equals prefix (the suffix makes tree keys unique, so
// equal prefixes sit adjacent). The uniqueness probes range over it (indexes.md §8).
function uniqueProbeBound(prefix: Uint8Array): KeyBound {
  return { lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false };
}

// bytesEq reports byte equality of two keys.
function bytesEq(a: Uint8Array, b: Uint8Array): boolean {
  return compareBytes(a, b) === 0;
}

// encodeKeyValue is the order-preserving key bytes for one keyable value (encoding.md §2),
// matching the PK / index encoders. `value` is non-NULL and of a keyable type (a foreign-key
// column always is — its type equals a PK/UNIQUE parent column, CREATE TABLE §6.2).
function encodeKeyValue(ty: ScalarType, value: Value): Uint8Array {
  if (value.kind === "int") return encodeInt(ty, value.int);
  if (value.kind === "bool") return encodeBool(value.value);
  if (value.kind === "uuid") return value.bytes.slice();
  if (value.kind === "timestamp" || value.kind === "timestamptz") return encodeInt(ty, value.micros);
  if (value.kind === "date") return encodeInt(ty, value.days);
  throw new Error("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)");
}

// FkProbe is a built foreign-key probe (spec/design/constraints.md §6.4/§6.8): the bytes to look
// up in the parent, tagged with which physical tree to probe — the parent's PK store (bare member
// encodings concatenated, in PK key order) or a parent unique index's prefix (0x00-tagged slots,
// in index-key order, plus the lowercased index name). A discriminated union (the TS idiom), with
// fkProbeBytes returning the raw bytes for batch-membership comparison.
type FkProbe =
  | { kind: "pk"; bytes: Uint8Array }
  | { kind: "unique"; index: string; prefix: Uint8Array };

// fkProbeBytes returns the raw probe bytes — used to compare against this statement's batch end
// state (§6.4). Two probes of one FK share the same byte space (a given FK always probes the PK or
// always a fixed unique index), so byte equality is a valid set membership test.
function fkProbeBytes(p: FkProbe): Uint8Array {
  return p.kind === "pk" ? p.bytes : p.prefix;
}

// fkProbe builds the parent-key probe for `fk` from `row`, taking each referenced parent column's
// value from `row[ordinals[i]]` where `ordinals[i]` supplies `fk.refColumns[i]`. So the child side
// passes `ordinals = fk.columns` (local columns), and a self-reference batch entry passes
// `ordinals = fk.refColumns` (the row viewed as a parent). Returns null when any supplied value is
// NULL (MATCH SIMPLE exempt — §6.3). The probe uses the parent's PK when the referenced set is the
// PK, else the matching unique index (re-derived deterministically — §6.8).
function fkProbe(fk: ForeignKey, parent: Table, row: Row, ordinals: number[]): FkProbe | null {
  // MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
  for (const o of ordinals) {
    if (row[o]!.kind === "null") return null;
  }
  // The value supplying parent column `pcol` (the fk pairing: refColumns[i] ⇄ ordinals[i]).
  const valueFor = (pcol: number): Value => {
    const i = fk.refColumns.indexOf(pcol);
    return row[ordinals[i]!]!;
  };
  const refSet = sortedUnique(fk.refColumns);
  const pkSet = sortedUnique(parent.pk);
  if (parent.pk.length > 0 && sameSet(pkSet, refSet)) {
    const parts: Uint8Array[] = [];
    for (const pcol of parent.pk) {
      const ty = typeScalar(parent.columns[pcol]!.type);
      parts.push(encodeKeyValue(ty, valueFor(pcol)));
    }
    return { kind: "pk", bytes: concatBytes(parts) };
  }
  const idx = parent.indexes.find((i) => i.unique && sameSet(sortedUnique(i.columns), refSet))!;
  const parts: Uint8Array[] = [];
  for (const pcol of idx.columns) {
    parts.push(Uint8Array.of(0x00));
    const ty = typeScalar(parent.columns[pcol]!.type);
    parts.push(encodeKeyValue(ty, valueFor(pcol)));
  }
  return { kind: "unique", index: idx.name.toLowerCase(), prefix: concatBytes(parts) };
}

// concatBytes concatenates a list of byte arrays into one (the key-build helper).
function concatBytes(parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((acc, b) => acc + b.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const b of parts) {
    out.set(b, off);
    off += b.length;
  }
  return out;
}

// sortedUnique returns a column-ordinal list as a sorted, deduplicated set (for the
// order-independent FK referenced-columns ⇄ PK/unique-key set comparison —
// spec/design/constraints.md §6.2).
function sortedUnique(v: number[]): number[] {
  const s = [...v].sort((a, b) => a - b);
  const out: number[] = [];
  for (const x of s) {
    if (out.length === 0 || out[out.length - 1] !== x) out.push(x);
  }
  return out;
}

// sameSet reports whether two already-sorted-unique ordinal lists are equal.
function sameSet(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

// fkTypesEqual reports structural equality of two catalog Types — the FK same-type pairing gate
// (spec/design/constraints.md §6.2). An FK column is always a scalar (a key-encodable PK/UNIQUE
// type), but the comparison is full structural for completeness, mirroring Rust's `ty == ty`.
function fkTypesEqual(a: Type, b: Type): boolean {
  if (a.kind === "scalar" && b.kind === "scalar") return a.scalar === b.scalar;
  if (a.kind === "composite" && b.kind === "composite") return a.name === b.name;
  if (a.kind === "array" && b.kind === "array") return fkTypesEqual(a.elem, b.elem);
  return false;
}

// fkAction maps a parsed referential action to its persisted form, rejecting the unsupported
// write-actions (CASCADE / SET NULL / SET DEFAULT) as 0A000 (spec/design/constraints.md §6.6).
// `clause` is "DELETE" or "UPDATE" for the message.
function fkAction(a: RefAction, clause: string): FkAction {
  if (a === "noAction") return "noAction";
  if (a === "restrict") return "restrict";
  const word = a === "cascade" ? "CASCADE" : a === "setNull" ? "SET NULL" : "SET DEFAULT";
  throw engineError("feature_not_supported", "ON " + clause + " " + word + " is not supported");
}

// prefixSuccessor is the byte-successor of a prefix: the smallest byte string greater
// than every string that extends p. Increment the last non-0xFF byte and truncate after
// it; an all-0xFF prefix has no successor (null ⇒ unbounded high end).
function prefixSuccessor(p: Uint8Array): Uint8Array | null {
  let end = p.length;
  while (end > 0 && p[end - 1] === 0xff) end--;
  if (end === 0) return null;
  const s = p.slice(0, end);
  s[end - 1]!++;
  return s;
}

// BoundTerm is one `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is the LEFT side
// (a `5 < pk` flips to `pk > 5`). src is the constant/parameter operand node.
type BoundTerm = { op: BinaryOp; src: RExpr };

// PkBound is the plan-time result of PK analysis: the PK's storage type + the bound terms. The concrete
// key range is built per execution by buildKeyBound.
type PkBound = { pkType: ScalarType; terms: BoundTerm[] };

// BoundKey is the outcome of encoding a const-source into the PK key space: a usable key, a NULL const
// (the comparison is 3VL-unknown ⇒ empty range), or an out-of-range integer (drop this half-bound).
type BoundKey = { kind: "key"; key: Uint8Array } | { kind: "null" } | { kind: "outOfRange" };

// detectPkBound flattens the WHERE's top-level AND-chain (an OR is never descended — a disjunction is
// not a contiguous range) and collects every `pk <cmp> const-source` conjunct. null ⇒ full scan.
// Conservative + sound: an unrecognized conjunct contributes no bound and stays in the residual filter.
function detectPkBound(filter: RExpr, pkIdx: number, pkType: ScalarType): PkBound | null {
  const terms: BoundTerm[] = [];
  const walk = (e: RExpr): void => {
    if (e.kind === "and") {
      walk(e.lhs);
      walk(e.rhs);
      return;
    }
    const t = asBoundTerm(e, pkIdx, pkType);
    if (t !== null) terms.push(t);
  };
  walk(filter);
  return terms.length === 0 ? null : { pkType, terms };
}

// asBoundTerm recognizes a single PK comparison conjunct: a comparison (=,<,<=,>,>=) with the bare LOCAL
// PK column ("column" at pkIdx — a correlated "outerColumn" is a different kind, so it never matches) on
// one side and a const-source of the PK's own type on the other (a promoted comparison — e.g. intpk = 2.5
// → a constDecimal — does not match, so it stays residual). The op is flipped when the PK is on the right.
function asBoundTerm(e: RExpr, pkIdx: number, pkType: ScalarType): BoundTerm | null {
  if (e.kind !== "compare") return null;
  if (e.op !== "eq" && e.op !== "lt" && e.op !== "le" && e.op !== "gt" && e.op !== "ge") return null;
  const isPk = (x: RExpr): boolean => x.kind === "column" && x.index === pkIdx;
  if (isPk(e.lhs) && isConstSource(e.rhs, pkType)) return { op: e.op, src: e.rhs };
  if (isPk(e.rhs) && isConstSource(e.lhs, pkType)) return { op: flipCmp(e.op), src: e.lhs };
  return null;
}

// isConstSource reports whether e is constant for the whole scan AND of a type that encodes into the PK
// key space: a same-family const literal, a NULL literal (⇒ a provably empty range), a bind parameter,
// or a bare correlated "outerColumn" — its value is a runtime constant for a given outer row, so the
// inner subquery's PK is bounded by the current outer row's column and seeks instead of re-scanning the
// whole inner table per outer row (cost.md §3 "bounded scan", grammar.md §26). A type-mismatched outer
// reference is wrapped in a cast by the resolver (as for a const literal), so it never arrives here bare.
function isConstSource(e: RExpr, pkType: ScalarType): boolean {
  switch (e.kind) {
    case "param":
    case "constNull":
    case "outerColumn":
      return true;
    case "constInt":
      return isInteger(pkType);
    case "constBool":
      return isBool(pkType);
    case "constUuid":
      return isUuid(pkType);
    case "constTimestamp":
      return isTimestamp(pkType);
    case "constTimestamptz":
      return isTimestamptz(pkType);
    case "constDate":
      return isDate(pkType);
    default:
      return false;
  }
}

// flipCmp swaps a comparison's sense (for `const <op> pk` ⇒ `pk <flipped> const`). eq is symmetric.
function flipCmp(op: BinaryOp): BinaryOp {
  switch (op) {
    case "lt":
      return "gt";
    case "le":
      return "ge";
    case "gt":
      return "lt";
    case "ge":
      return "le";
    default:
      return op;
  }
}

// buildKeyBound turns the plan-time terms into a concrete key range at exec time: encode each
// const-source and intersect the half-bounds. null ⇒ the range admits no key (a NULL const — 3VL — or
// contradictory bounds), so the scan reads nothing. An out-of-range integer const drops only its own
// half-bound (a wider, still sound, scan).
// outer carries the enclosing rows (innermost last) so a correlated "outerColumn" source resolves to
// the current outer row's value; it is empty for a top-level statement.
function buildKeyBound(bp: PkBound, params: Value[], outer: Row[]): KeyBound | null {
  const b = unboundedBound();
  for (const t of bp.terms) {
    const r = encodeBoundKey(bp.pkType, t.src, params, outer);
    if (r.kind === "null") return null;
    if (r.kind === "outOfRange") continue;
    const key = r.key;
    switch (t.op) {
      case "eq":
        intersectLo(b, key, true);
        intersectHi(b, key, true);
        break;
      case "gt":
        intersectLo(b, key, false);
        break;
      case "ge":
        intersectLo(b, key, true);
        break;
      case "lt":
        intersectHi(b, key, false);
        break;
      case "le":
        intersectHi(b, key, true);
        break;
    }
  }
  return boundEmpty(b) ? null : b;
}

// encodeBoundKey encodes a const-source's value into the PK's storage key (the same codec INSERT uses —
// encodeInt for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte bool-byte for boolean).
// param/outerColumn resolve to a runtime Value first (the param table / the enclosing outer row) and
// then encode through the shared path.
function encodeBoundKey(pkType: ScalarType, src: RExpr, params: Value[], outer: Row[]): BoundKey {
  switch (src.kind) {
    case "constNull":
      return { kind: "null" };
    case "constInt":
      return inRange(pkType, src.value) ? { kind: "key", key: encodeInt(pkType, src.value) } : { kind: "outOfRange" };
    case "constBool":
      return { kind: "key", key: encodeBool(src.value) };
    case "constUuid":
      return { kind: "key", key: src.value.slice() };
    case "constTimestamp":
    case "constTimestamptz":
    case "constDate":
      return { kind: "key", key: encodeInt(pkType, src.value) };
    case "param":
      return encodeValueKey(pkType, params[src.index]!);
    case "outerColumn":
      // A correlated reference: column index of the enclosing row level hops out — the same indexing
      // the evaluator uses for "outerColumn" (innermost outer row is last).
      return encodeValueKey(pkType, outer[outer.length - src.level]![src.index]!);
    default:
      return { kind: "outOfRange" };
  }
}

// encodeValueKey encodes a runtime Value (a bound param or a resolved outer column) into the PK's storage
// key. A NULL value makes the comparison 3VL-unknown (an empty range); a value of a kind no key can hold
// (or an integer outside the PK width) drops its half-bound, widening — still sound.
function encodeValueKey(pkType: ScalarType, v: Value): BoundKey {
  if (v.kind === "null") return { kind: "null" };
  if (v.kind === "bool") return { kind: "key", key: encodeBool(v.value) };
  if (v.kind === "uuid") return { kind: "key", key: v.bytes.slice() };
  if (v.kind === "int")
    return inRange(pkType, v.int) ? { kind: "key", key: encodeInt(pkType, v.int) } : { kind: "outOfRange" };
  if (v.kind === "date") return { kind: "key", key: encodeInt(pkType, v.days) };
  if (v.kind === "timestamp" || v.kind === "timestamptz")
    return { kind: "key", key: encodeInt(pkType, v.micros) };
  return { kind: "outOfRange" };
}

// intersectLo tightens b's lower bound to the more restrictive of (current, key); at an equal key an
// exclusive bound (inc=false) wins.
function intersectLo(b: KeyBound, key: Uint8Array, inc: boolean): void {
  if (b.lo === null) {
    b.lo = key;
    b.loInc = inc;
    return;
  }
  const c = compareBytes(key, b.lo);
  if (c > 0 || (c === 0 && !inc)) {
    b.lo = key;
    b.loInc = inc;
  }
}

// intersectHi tightens b's upper bound to the more restrictive of (current, key); at an equal key an
// exclusive bound wins.
function intersectHi(b: KeyBound, key: Uint8Array, inc: boolean): void {
  if (b.hi === null) {
    b.hi = key;
    b.hiInc = inc;
    return;
  }
  const c = compareBytes(key, b.hi);
  if (c < 0 || (c === 0 && !inc)) {
    b.hi = key;
    b.hiInc = inc;
  }
}

// boundEmpty reports whether the bound admits no key: lo above hi, or lo == hi with a non-inclusive
// endpoint.
function boundEmpty(b: KeyBound): boolean {
  if (b.lo === null || b.hi === null) return false;
  const c = compareBytes(b.lo, b.hi);
  if (c > 0) return true;
  if (c === 0) return !(b.loInc && b.hiInc);
  return false;
}

// mutationPkBound detects a single-table UPDATE/DELETE's PK pushdown bound; null ⇒ full scan.
function mutationPkBound(table: Table, filter: RExpr | null): PkBound | null {
  if (filter === null) return null;
  const pkIdx = primaryKeyIndex(table);
  if (pkIdx < 0) return null;
  return detectPkBound(filter, pkIdx, typeScalar(table.columns[pkIdx]!.type));
}

// scanEntries returns the (key,row) entries a mutation scans + the page_read node count: a primary-key
// bound seeks/ranges, an empty bound yields entries=null (the caller charges nothing and mutates
// nothing), and no bound is the full scan (cost.md §3 "bounded scan").
function scanEntries(
  store: TableStore,
  pkBound: PkBound | null,
  params: Value[],
  mask: boolean[],
): { entries: Entry[] | null; overlap: number; slabs: number } {
  if (pkBound !== null) {
    // Top-level statement: no enclosing query, so the bound never has a correlated source.
    const b = buildKeyBound(pkBound, params, []);
    if (b === null) return { entries: null, overlap: 0, slabs: 0 };
    const u = store.rangeScanWithUnits(b, mask);
    return { entries: u.entries, overlap: u.pages, slabs: u.slabs };
  }
  const u = store.scanWithUnits(mask);
  return { entries: u.entries, overlap: u.pages, slabs: u.slabs };
}

// ---- Subquery helpers (spec/design/grammar.md §26) ----------------------

// queryPlanReferencesOuter reports whether a plan references any scope STRICTLY OUTSIDE itself —
// i.e. it is correlated (spec/design/grammar.md §26). depth is how many nested-subquery frames we
// have descended INTO this plan (0 = its own clauses); an outerColumn at level points above iff
// level > depth. The fold pass calls it with depth 0 on a subquery's sub-plan to fold (uncorrelated)
// or leave (correlated) it.
function queryPlanReferencesOuter(plan: QueryPlan, depth: number): boolean {
  if (plan.kind === "setOp") {
    return queryPlanReferencesOuter(plan.lhs, depth) || queryPlanReferencesOuter(plan.rhs, depth);
  }
  if (plan.kind === "values") {
    // A VALUES body is planned parent=null, so its values hold no outer reference of their own; a
    // folded-in subquery, however, may correlate to the target scope.
    for (const row of plan.rows) for (const e of row) if (rexprReferencesOuter(e, depth)) return true;
    return false;
  }
  for (const j of plan.joins) if (j.on !== null && rexprReferencesOuter(j.on, depth)) return true;
  if (plan.filter !== null && rexprReferencesOuter(plan.filter, depth)) return true;
  if (plan.having !== null && rexprReferencesOuter(plan.having, depth)) return true;
  for (const s of plan.aggSpecs) if (s.operand !== null && rexprReferencesOuter(s.operand, depth)) return true;
  for (const p of plan.projections) if (rexprReferencesOuter(p, depth)) return true;
  // A set-returning relation's arguments may carry a correlated reference (an implicitly-lateral SRF
  // arg sees params / outer / an earlier sibling — functions.md §10, grammar.md §44), making the
  // enclosing query correlated. A LATERAL derived table's body is one frame deeper; a reference in it
  // back into this query's outer counts here too (§44).
  for (const rel of plan.rels) {
    if (rel.srf !== undefined) {
      for (const a of rel.srf.args) if (rexprReferencesOuter(a, depth)) return true;
    }
    if (rel.derived !== undefined && queryPlanReferencesOuter(rel.derived, depth + 1)) return true;
  }
  return false;
}

function rexprReferencesOuter(e: RExpr, depth: number): boolean {
  switch (e.kind) {
    case "outerColumn":
      return e.level > depth;
    case "subquery":
      // A nested subquery's own clauses are one frame deeper; its IN lhs is at this frame.
      return (
        (e.lhs !== null && rexprReferencesOuter(e.lhs, depth)) ||
        queryPlanReferencesOuter(e.plan, depth + 1)
      );
    case "inValues":
      return rexprReferencesOuter(e.lhs, depth);
    case "quantified":
      return rexprReferencesOuter(e.lhs, depth) || rexprReferencesOuter(e.array, depth);
    case "cast":
    case "neg":
    case "not":
    case "isNull":
      return rexprReferencesOuter(e.operand, depth);
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "distinct":
    case "like":
      return rexprReferencesOuter(e.lhs, depth) || rexprReferencesOuter(e.rhs, depth);
    case "case":
      return (
        e.arms.some((arm) => rexprReferencesOuter(arm.cond, depth) || rexprReferencesOuter(arm.result, depth)) ||
        rexprReferencesOuter(e.els, depth)
      );
    case "scalarFunc":
    case "arrayFunc":
    case "variadic":
      return e.args.some((a) => rexprReferencesOuter(a, depth));
    case "row":
      return e.fields.some((f) => rexprReferencesOuter(f, depth));
    case "array":
      return e.elements.some((el) => rexprReferencesOuter(el, depth));
    case "field":
      return rexprReferencesOuter(e.base, depth);
    case "subscript":
      return rexprReferencesOuter(e.base, depth) || rSubscriptBounds(e.subscripts).some((b) => rexprReferencesOuter(b, depth));
    default:
      return false; // leaves: column, param, const*
  }
}

// rSubscriptBounds is the bound RExprs of a list of resolved subscript specs (each index, or a
// slice's present lower/upper bounds) — for the RExpr tree walkers (spec/design/array.md §6).
function rSubscriptBounds(subs: RSubscript[]): RExpr[] {
  const out: RExpr[] = [];
  for (const s of subs) {
    if (!s.isSlice) out.push(s.index);
    else {
      if (s.lower !== null) out.push(s.lower);
      if (s.upper !== null) out.push(s.upper);
    }
  }
  return out;
}

// collectTouched marks the combined-row columns an expression STATICALLY references — the
// touched set (cost.md §3 "The touched set"; large-values.md §14). Depth bookkeeping mirrors
// rexprReferencesOuter: walking the target plan's own clauses is depth 0 (a column touches);
// inside a nested subquery a column indexes the subquery's own row (ignored) and an outer
// column with level === depth is a correlated reference back into the target scope (touches).
// Purely syntactic — a never-taken CASE branch still touches — so the set is deterministic and
// cross-core identical (a §8 contract).
function collectTouched(e: RExpr, depth: number, touched: boolean[]): void {
  switch (e.kind) {
    case "column":
      if (depth === 0) touched[e.index] = true;
      return;
    case "outerColumn":
      if (e.level === depth && depth > 0) touched[e.index] = true;
      return;
    case "subquery":
      if (e.lhs !== null) collectTouched(e.lhs, depth, touched);
      collectTouchedPlan(e.plan, depth + 1, touched);
      return;
    case "inValues":
      collectTouched(e.lhs, depth, touched);
      return;
    case "quantified":
      collectTouched(e.lhs, depth, touched);
      collectTouched(e.array, depth, touched);
      return;
    case "cast":
    case "neg":
    case "not":
    case "isNull":
      collectTouched(e.operand, depth, touched);
      return;
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "distinct":
    case "like":
      collectTouched(e.lhs, depth, touched);
      collectTouched(e.rhs, depth, touched);
      return;
    case "case":
      for (const arm of e.arms) {
        collectTouched(arm.cond, depth, touched);
        collectTouched(arm.result, depth, touched);
      }
      collectTouched(e.els, depth, touched);
      return;
    case "scalarFunc":
    case "arrayFunc":
    case "variadic":
      for (const a of e.args) collectTouched(a, depth, touched);
      return;
    case "row":
      for (const f of e.fields) collectTouched(f, depth, touched);
      return;
    case "array":
      for (const el of e.elements) collectTouched(el, depth, touched);
      return;
    case "field":
      collectTouched(e.base, depth, touched);
      return;
    case "subscript":
      collectTouched(e.base, depth, touched);
      for (const b of rSubscriptBounds(e.subscripts)) collectTouched(b, depth, touched);
      return;
    default: // leaves: param, const*
  }
}

// collectTouchedPlan walks a nested plan's expression surfaces for outer references back into
// the target scope — the same five surfaces selectPlanReferencesOuter checks (slot lists like
// group keys / ORDER BY index the nested plan's own rows and can never reach outward).
function collectTouchedPlan(plan: QueryPlan, depth: number, touched: boolean[]): void {
  if (plan.kind === "select") {
    for (const j of plan.joins) if (j.on !== null) collectTouched(j.on, depth, touched);
    if (plan.filter !== null) collectTouched(plan.filter, depth, touched);
    if (plan.having !== null) collectTouched(plan.having, depth, touched);
    for (const s of plan.aggSpecs) if (s.operand !== null) collectTouched(s.operand, depth, touched);
    for (const p of plan.projections) collectTouched(p, depth, touched);
  } else if (plan.kind === "values") {
    for (const row of plan.rows) for (const e of row) collectTouched(e, depth, touched);
  } else {
    collectTouchedPlan(plan.lhs, depth, touched);
    collectTouchedPlan(plan.rhs, depth, touched);
  }
}

// valueToRExpr builds the constant rExpr for a folded subquery value (§26). The static type is
// carried separately (the node's type), so a NULL value here is just constNull.
function valueToRExpr(v: Value): RExpr {
  switch (v.kind) {
    case "int":
      return { kind: "constInt", value: v.int };
    case "bool":
      return { kind: "constBool", value: v.value };
    case "text":
      return { kind: "constText", value: v.text };
    case "decimal":
      return { kind: "constDecimal", value: v.dec };
    case "bytea":
      return { kind: "constBytea", value: v.bytes };
    case "uuid":
      return { kind: "constUuid", value: v.bytes };
    case "timestamp":
      return { kind: "constTimestamp", value: v.micros };
    case "timestamptz":
      return { kind: "constTimestamptz", value: v.micros };
    case "date":
      return { kind: "constDate", value: v.days };
    case "interval":
      return { kind: "constInterval", value: v.iv };
    case "composite":
      // A folded composite constant: fold each field and wrap in a ROW node so eval rebuilds the
      // composite value (spec/design/composite.md).
      return { kind: "row", fields: v.fields.map(valueToRExpr) };
    case "array":
      // A folded array constant — preserve its full shape (dims/lbounds) in a const node.
      return { kind: "constArray", value: v };
    default:
      return { kind: "constNull" };
  }
}

// distinctRowKey encodes a projected row into a collision-free string key for DISTINCT
// dedup. Each field carries a type tag (n/i/b) and a payload, joined by a separator no
// field can contain, so e.g. (1,23) and (12,3) do not collide (spec/design/grammar.md §11).
// NULL == NULL falls out (both encode "n"), matching the NULL-safe DISTINCT rule. Ints use
// bigint.toString() — exact, never the lossy `number` path (CLAUDE.md §8).
function distinctRowKey(row: Value[]): string {
  return row.map(distinctValueKey).join("|");
}

// distinctValueKey encodes one value into a collision-free string key for DISTINCT/GROUP BY dedup
// (spec/design/grammar.md §11). A composite recurses element-wise under a length-prefixed 'c' tag,
// so composites group/dedup structurally — NULL-safe, with NULL fields included (spec/design/composite.md
// §5). Shared by distinctRowKey (which joins the per-field keys with a separator no scalar key can
// contain).
function distinctValueKey(v: Value): string {
  {
      switch (v.kind) {
        case "composite":
          // Length-prefix the field count and each field's key so a composite never collides with a
          // scalar key and nested composites stay unambiguous.
          return "c" + v.fields.length.toString() + ":" + v.fields.map((f) => distinctValueKey(f)).join(",");
        case "array":
          // An array keys structurally INCLUDING its shape (spec/design/array.md §5): the dims and
          // lower bounds (so [2:4]={1,2,3} and {1,2,3} bucket apart — array_eq considers them), then
          // each element's own key. NULL elements key as 'n' (btree equality — NULLs mutually equal).
          return (
            "a" +
            v.dims.join(":") +
            ";" +
            v.lbounds.join(";") +
            "=" +
            v.elements.map((el) => distinctValueKey(el)).join(",")
          );
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
        case "timestamp":
          // The i64 microsecond instant under a distinct 's' tag: two literals for the same
          // instant (12:00:00 and 12:00:00.0) share micros and bucket together; the infinity
          // sentinels are ordinary int values with their own buckets.
          return "s" + v.micros.toString();
        case "timestamptz":
          // The i64 UTC-instant micros under a distinct 'z' tag: offsets are normalized to UTC
          // at parse, so +00 and +05-of-the-same-instant bucket together.
          return "z" + v.micros.toString();
        case "interval":
          // The canonical 128-bit span as a decimal string under a distinct 'v' tag, so
          // span-equal intervals ('1 mon' / '30 days' / '720:00:00') collapse to one DISTINCT/
          // GROUP BY bucket while each value renders its own fields (spec/design/interval.md §2).
          return "v" + intervalSpan(v.iv).toString();
        case "f32":
        case "f64":
          // The TOTAL-order canonical key so -0 and +0 collapse to one bucket and all NaNs to one
          // (float.md §3): canonicalize -0 → +0, map every NaN to a single sentinel string. Distinct
          // tags 'f'/'g' per width so a f32 never collides with a f64 of the same number
          // (they are different typed columns; the tag keeps the key total). The number's toString
          // is the shortest round-trip — unique per binary value, so distinct values get distinct
          // keys after the -0/NaN normalization.
          return (
            (v.kind === "f32" ? "f" : "g") +
            (Number.isNaN(v.value) ? "NaN" : renderFloat(canonFloat(v.value)))
          );
        default:
          // unfetched never reaches a projected dedup row (the scan layer resolves touched columns).
          throw new Error("BUG: unfetched large value escaped the storage layer");
      }
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
  // The float family (spec/design/float.md): ty is f32 or f64. A strict island — no
  // implicit cross-family coercion (int/decimal ⊕ float is 42804); within-family widening
  // (f32 → f64) is the only implicit edge. ty carries the width so arithmetic rounds
  // at the right precision (Math.fround for f32) and the on-disk codec picks the codec.
  | { kind: "float"; ty: ScalarType }
  | { kind: "bool" }
  | { kind: "text" } // the text family (one collation, C); does not promote
  | { kind: "decimal" } // the decimal family (one type; the per-column typmod is separate)
  | { kind: "bytea" } // the bytea family (raw bytes); does not promote
  | { kind: "uuid" } // the uuid family (fixed 16 bytes); does not promote. The first non-integer key.
  | { kind: "timestamp" } // zoneless instant; does not compare/cast to timestamptz
  | { kind: "timestamptz" } // UTC instant; does not compare/cast to timestamp
  | { kind: "date" } // calendar date (i32 days); strict island, no compare/cast to timestamp
  | { kind: "interval" } // a span; compares only with itself, by the canonical span
  // A composite (row) type (spec/design/composite.md §5). `name` is non-null for a named catalog
  // type — rendered in the `# types:` output and the basis for cross-comparability — or null for an
  // anonymous ROW(...) result. `fields` are the resolved (name, type) pairs in declaration order
  // (the basis for field access — S4 — and structural assignability).
  | { kind: "composite"; name: string | null; fields: { name: string; type: ResolvedType }[] }
  // An array type (spec/design/array.md §2), carrying its resolved element type. Two arrays are
  // comparable iff their element types are comparable; assignable to an array column of the same
  // element type.
  | { kind: "array"; elem: ResolvedType }
  | { kind: "null" };

// RSubscript is one resolved subscript spec in a "subscript" RExpr (spec/design/array.md §6): an
// index `a[i]`, or a slice `a[m:n]` whose bounds may be null (omitted: `a[:n]`/`a[m:]`/`a[:]`).
type RSubscript =
  | { isSlice: false; index: RExpr }
  | { isSlice: true; lower: RExpr | null; upper: RExpr | null };

// RExpr is a resolved expression over fixed column indices. Arithmetic/neg nodes carry
// their (promotion-tower) result type so the computed value can be range-checked.
type RExpr =
  | { kind: "column"; index: number }
  // A bind parameter, by 0-based index into the bound-values array passed to evalExpr. Its
  // static type was inferred from context at resolve (spec/design/api.md §5); the value is
  // supplied (and coerced) before evaluation.
  | { kind: "param"; index: number }
  | { kind: "constInt"; value: bigint }
  // A float constant: `ty` is the width (f32/f64); for f32 `value` is already
  // Math.fround'd (spec/design/float.md §4).
  | { kind: "constFloat"; ty: ScalarType; value: number }
  | { kind: "constBool"; value: boolean }
  | { kind: "constText"; value: string }
  | { kind: "constDecimal"; value: Decimal }
  | { kind: "constBytea"; value: Uint8Array }
  | { kind: "constUuid"; value: Uint8Array }
  | { kind: "constTimestamp"; value: bigint }
  | { kind: "constTimestamptz"; value: bigint }
  | { kind: "constDate"; value: bigint }
  | { kind: "constInterval"; value: Interval }
  | { kind: "constNull" }
  // A ROW(...) constructor (spec/design/composite.md §1): evaluate each field and assemble a
  // composite value. Also the folded form of a composite constant (valueToRExpr wraps each field's
  // constant). One operator_eval per node (cost.md §9).
  | { kind: "row"; fields: RExpr[] }
  // An ARRAY[...] constructor (spec/design/array.md §1): evaluate each element and assemble an array
  // value. `nested` stacks sub-arrays into one higher dimension (§4). One operator_eval per node.
  | { kind: "array"; elements: RExpr[]; nested: boolean }
  // A folded array constant (the valueToRExpr form), preserving its shape; evaluates to it directly.
  | { kind: "constArray"; value: Value }
  // Field selection `(composite).field` (spec/design/composite.md §S4): evaluate `base` to a
  // composite value and return its `index`-th field (the field ordinal, fixed at resolve). A
  // whole-value-NULL composite yields NULL for any field. One operator_eval per node.
  | { kind: "field"; base: RExpr; index: number }
  // Array subscript `base[..][..]` (spec/design/array.md §6): one or more subscript specs applied to
  // `base`. All-index access reads one element (NULL when the subscript count ≠ ndim or any index is
  // out of range); a slice (any spec a slice) returns a sub-array, with a scalar index i meaning 1:i.
  // A NULL array or any NULL bound yields NULL. One operator_eval per node.
  | { kind: "subscript"; base: RExpr; subscripts: RSubscript[]; isSlice: boolean }
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
  | { kind: "case"; arms: { cond: RExpr; result: RExpr }[]; els: RExpr; coerceDecimal: boolean }
  // A scalar-function call (spec/design/functions.md §9, float.md §8), evaluated per row in any
  // context. `result` is the static result type — for abs over an integer/float it is the
  // operand's own type (range-checked / fround'd at that width), for round over int/decimal it is
  // decimal, and for the float math functions (ceil/floor/.../sqrt/exp/.../tan) it is f64 per
  // the catalog (only abs is operand-typed). `argWidth` carries the operand's float width so the
  // evaluator can Math.fround a f32 result of abs (the only width-preserving float func).
  | {
      kind: "scalarFunc";
      func: ScalarFuncName;
      args: RExpr[];
      result: ScalarType;
      argWidth?: ScalarType;
    }
  // A polymorphic array-function call (spec/design/array-functions.md §3), evaluated per row.
  // Distinct from "scalarFunc": it resolves over anyarray/anyelement (§2) and its builders return an
  // array; the result type lives in the surrounding ResolvedType (carried out of resolve), not on the
  // node. NULL handling is per-kernel (the introspectors propagate, the builders are non-strict), so
  // — unlike "scalarFunc" — there is no blanket NULL short-circuit at eval.
  | { kind: "arrayFunc"; func: ArrayFuncName; args: RExpr[] }
  // A VARIADIC argument-counting call (spec/design/array-functions.md §12 — num_nulls/num_nonnulls).
  // Non-strict (null = "none"), like "arrayFunc": no blanket NULL short-circuit. `arrayForm` records
  // the call shape — false = the spread form (count `args`' null-ness directly, never NULL); true =
  // the VARIADIC-array form (one `args` operand — a NULL array → NULL, else count its flattened
  // elements' null-ness). Result is always i32.
  | { kind: "variadic"; func: VariadicFuncName; args: RExpr[]; arrayForm: boolean }
  // A correlated column reference (spec/design/grammar.md §26): column `index` of the enclosing
  // row `level` hops out (1 = immediate parent). A leaf — reads from the outer-row environment.
  | { kind: "outerColumn"; level: number; index: number }
  // A CORRELATED subquery, re-executed once per outer row at eval (uncorrelated ones are folded to
  // a constant / inValues before exec). `lhs`/`negated` apply to the IN form.
  | {
      kind: "subquery";
      plan: QueryPlan;
      subKind: SubqueryKind;
      lhs: RExpr | null;
      negated: boolean;
      // For subKind "quantified" (array-functions.md §11.6): the comparison op + ALL flag, so the
      // body's single column folds through quantifiedMembership exactly like the array form.
      op?: BinaryOp;
      all?: boolean;
    }
  // A folded uncorrelated `IN (subquery)`: the subquery ran once yielding `list`; per row it tests
  // `lhs` for three-valued membership (empty → negated; a NULL with no positive match → NULL).
  | { kind: "inValues"; lhs: RExpr; list: Value[]; negated: boolean }
  // A quantified array comparison `lhs op ANY/ALL(array)` (spec/design/array-functions.md §11) — the
  // array spelling of IN. At eval `lhs` is evaluated once, `array` once; then a 3-valued fold over the
  // array's flattened elements (ANY = OR-fold, ALL = AND-fold), charging per element like "inValues".
  | { kind: "quantified"; op: BinaryOp; all: boolean; lhs: RExpr; array: RExpr };

// SubqueryKind selects which subquery form a "subquery" RExpr is (spec/design/grammar.md §26).
type SubqueryKind = "scalar" | "exists" | "in" | "quantified";

// ScalarFuncName is the internal identity of a scalar-function node. abs/round span int/decimal
// AND float overloads; the rest (ceil…tan) are float-only (spec/design/float.md §8). The
// exact-vs-transcendental split is a conformance-layer concern (the R tag + the determinism
// ledger), not a code distinction here — all are ordinary per-row function nodes.
type ScalarFuncName =
  | "abs"
  | "round"
  | "ceil"
  | "floor"
  | "trunc"
  | "sqrt"
  | "exp"
  | "ln"
  | "log10"
  | "pow"
  | "sin"
  | "cos"
  | "tan"
  // make_interval — builds an interval from its (named/defaulted) integer components plus the
  // f64 secs (spec/design/functions.md §11). The one scalar function returning interval.
  | "make_interval"
  // uuid extractors (spec/design/functions.md §12): pure inspectors of a uuid's bits.
  // uuid_extract_version → i16 (NULL off-RFC-variant); uuid_extract_timestamp → timestamptz
  // (the embedded instant for v1/v7, else NULL).
  | "uuid_extract_version"
  | "uuid_extract_timestamp"
  // uuid generators (spec/design/entropy.md §3): volatile. uuidv4 → random; uuidv7 → ms timestamp
  // + monotonic counter + random, with an optional interval shift.
  | "uuidv4"
  | "uuidv7"
  // current-time functions (spec/design/entropy.md §5): now → timestamptz, the statement clock read
  // ONCE and reused (STABLE; current_timestamp is parser sugar for it); clock_timestamp →
  // timestamptz, the clock seam read on EVERY call (VOLATILE).
  | "now"
  | "clock_timestamp"
  // Sequence value functions (spec/design/sequences.md §4/§6). nextval → i64, advance the named
  // sequence (VOLATILE; MUTATES the working snapshot via pendingSeq, so the statement is a write).
  // currval → i64, the value nextval/setval last produced for the named sequence IN THIS SESSION.
  // setval → i64, set the counter (also a write); lastval → i64, the most-recent-nextval value.
  | "nextval"
  | "currval"
  | "setval"
  | "lastval";

// ArrayFuncName is the internal identity of a polymorphic array-function node
// (spec/design/array-functions.md §3). Each name is single-arity; the kernel recovers everything
// from the operand values (the array's own shape header).
type ArrayFuncName =
  | "array_ndims"
  | "array_length"
  | "array_lower"
  | "array_upper"
  | "cardinality"
  | "array_dims"
  | "array_append"
  | "array_prepend"
  | "array_cat"
  | "array_remove"
  | "array_replace"
  | "array_position"
  | "array_positions"
  // The containment/overlap operators `@>`/`<@`/`&&` (array-functions.md §10) — not catalog function
  // names; resolved via resolveContainment, which selects these kernel ids directly.
  | "contains"
  | "contained_by"
  | "overlaps";

// VariadicFuncName is the internal identity of a VARIADIC counting-function node
// (spec/design/array-functions.md §12). Both return i32; the call form lives on the node.
type VariadicFuncName = "num_nulls" | "num_nonnulls";

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). planQuery (the resolve half of the old runSelect)
// produces a QueryPlan; execQueryPlan (the execute half) consumes it against an outer-row
// environment. The split lets a subquery be resolved ONCE — so its structural/type errors fire
// even over an empty outer — yet executed many times (spec/design/grammar.md §26).
// ============================================================================

// PlanRel is one relation in a SELECT plan: the table name (looked up in the store at exec), the
// flat offset of its first column, and its column count (for NULL-padding). When `srf` is set the
// relation is a COMPUTED set-returning function (generate_series) rather than a base table:
// tableName is then the function name (never looked up in the store) and the executor generates
// the rows instead of scanning (spec/design/functions.md §10).
// When `cte` is set, the relation is a reference to common-table-expression `cte` (the index into
// the statement's CTE list — spec/design/cte.md), not a base table: `tableName` is then the CTE
// name (never looked up in the store) and the executor delivers its rows from the per-statement
// CteCtx (a materialized buffer, or the inlined body run in place).
// A `derived` plan marks a DERIVED TABLE — `FROM (SELECT …) [AS] t` (grammar.md §42): a
// parenthesized subquery used as a relation, mechanically an anonymous always-inlined
// single-reference CTE. tableName is the alias (never looked up in the store); the executor runs
// this plan in place (it was planned parent=null, so it reads no outer row), charging its intrinsic
// cost — no cte_scan_row.
// `lateral` is true when this relation is a CORRELATED LATERAL item (spec/design/grammar.md §44):
// its derived body / SRF args reference an earlier sibling (or an enclosing query), so the executor
// re-materializes it ONCE PER combined left-hand row (with that row pushed as its immediate outer —
// the correlated-subquery mechanism), rather than materializing it once. Always false for the first
// relation; only a srf or derived relation is ever lateral.
type PlanRel = { tableName: string; offset: number; colCount: number; srf?: SrfPlan; cte?: number; derived?: QueryPlan; lateral?: boolean };

// SrfKind selects which set-returning function an SrfPlan is, picking the row generator at exec
// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core.
//   "generate_series" — generate_series(start, stop[, step]), an integer series (functions.md §10).
//   "unnest"          — unnest(anyarray), one row per array element, flattened (array-functions.md §9).
type SrfKind = "generate_series" | "unnest";

// SrfPlan is a resolved set-returning-function row source (spec/design/functions.md §10,
// array-functions.md §9). kind selects the generator: generate_series(start, stop[, step]) (args =
// 2 or 3 integers) or unnest(anyarray) (args = the single array expression). Non-LATERAL, so each
// arg evaluates against the params/outer environment with no local row. The produced column's type
// lives on the synthetic relation (built in resolveSRF).
type SrfPlan = { kind: SrfKind; args: RExpr[] };

// srfTable builds a set-returning function's SYNTHETIC one-column relation (spec/design/functions.md
// §10). The table's name is the function name (the un-aliased label fallback); the lone column's
// NAME follows PostgreSQL's single-column function-alias rule — the table alias when one is given,
// else the function name — and its TYPE is colTy (the promoted integer for generate_series, the
// bound element type for unnest).
function srfTable(funcName: string, alias: string | null, colTy: Type): Table {
  return {
    name: funcName,
    columns: [{ name: alias ?? funcName, type: colTy, decimal: null, primaryKey: false, notNull: false, default: null, defaultExpr: null }],
    pk: [],
    checks: [],
    indexes: [],
    fks: [],
  };
}

// PlanJoin is one join in a SELECT plan: its kind and resolved ON predicate (null for CROSS). The
// right relation is rels[k+1].
type PlanJoin = { kind: JoinKind; on: RExpr | null };

// OrderSlot is a resolved ORDER BY key: a flat/synthetic slot + per-key direction flags.
type OrderSlot = { idx: number; descending: boolean; nullsFirst: boolean };

// SelectPlan is a resolved SELECT, executable against an outer-row environment (the execute half
// of the old runSelect, lifted to a value so a correlated subquery can re-run it per outer row).
type SelectPlan = {
  kind: "select";
  rels: PlanRel[];
  joins: PlanJoin[];
  filter: RExpr | null;
  isAgg: boolean;
  groupKeys: number[];
  aggSpecs: AggSpec[];
  having: RExpr | null;
  order: OrderSlot[];
  projections: RExpr[];
  columnNames: string[];
  columnTypes: ResolvedType[];
  distinct: boolean;
  limit: bigint | null;
  offset: bigint | null;
  // Primary-key predicate pushdown, ONE entry per relation in rels: the WHERE conjuncts that bound
  // that relation's storage key, so its scan seeks/ranges instead of walking the whole B-tree
  // (cost.md §3 "bounded scan"). null ⇒ a full scan of that relation. In a JOIN each base table is
  // bounded independently by the WHERE predicates on its OWN primary key against a CONSTANT
  // (literal/param/outer) — a cross-relation `b.pk = a.x` is the index-nested-loop case (a
  // follow-on). The residual filter stays the WHOLE `filter`, re-applied after the join.
  relBounds: (ScanBound | null)[];
  // relMasks is the TOUCHED SET per relation (cost.md §3 "The touched set"; large-values.md §14):
  // which of its columns this query statically references. Drives the chain-pageRead /
  // valueDecompress portion of the scan's up-front cost block — an untouched spilled or
  // compressed column charges nothing, however many records the bound admits.
  relMasks: boolean[][];
};

// SetOpPlan is a resolved set operation: both operands planned with the same parent scope, the
// unified output types, and the trailing ORDER BY / LIMIT / OFFSET resolved by output column.
type SetOpPlan = {
  kind: "setOp";
  op: SetOpKind;
  all: boolean;
  lhs: QueryPlan;
  rhs: QueryPlan;
  columnNames: string[];
  columnTypes: ResolvedType[];
  order: OrderSlot[];
  limit: bigint | null;
  offset: bigint | null;
};

// ValuesPlan is a resolved VALUES-body relation (spec/design/grammar.md §42), executable to its
// literal rows — the FROM-position sibling of INSERT … VALUES. rows[r][c] is row r, column c, each
// resolved as a CONSTANT (the body is non-LATERAL, planned parent=null, so it reads no row).
// columnTypes is the per-column type unified across the rows like a set operation (§25), and
// columnNames is column1, column2, … (PostgreSQL; the derived table's optional column-rename list
// overrides them at the synthetic relation). All rows have columnTypes.length values.
type ValuesPlan = {
  kind: "values";
  rows: RExpr[][];
  columnTypes: ResolvedType[];
  columnNames: string[];
};

// QueryPlan is a resolved query expression: a SELECT plan, a set-op plan, or a VALUES-body relation
// (mirrors QueryExpr's bodies). A VALUES plan is only ever produced as a derived-table body.
type QueryPlan = SelectPlan | SetOpPlan | ValuesPlan;

// CteMode is how a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE
// from its reference count and [NOT] MATERIALIZED hint: a single-reference CTE is "inline", a
// multi-reference (or MATERIALIZED) one is "materialize".
//   "inline":      run the body in place at each reference (re-evaluates per outer row under
//                  correlation, matching PostgreSQL); charges the body's intrinsic cost, no
//                  cte_scan_row.
//   "materialize": run the body once, buffer the rows; each reference scans the buffer, charging
//                  cte_scan_row per buffered row.
type CteMode = "inline" | "materialize";

// CteBinding is a planned common table expression (spec/design/cte.md), built by runWith for the
// whole statement so the scopes that reference its synthetic `table` can see it. `name` is
// lowercased for case-insensitive FROM matching; `table` is the synthetic relation exposing the
// body's output columns; `plan` is the planned body; `hint` is the [NOT] MATERIALIZED override
// (true/false/null); `refs` counts the FROM references resolved to it during planning (the
// inline-vs-materialize decision — cost.md §3).
type CteBinding = {
  name: string;
  table: Table;
  plan: QueryPlan;
  hint: boolean | null;
  refs: number;
};

// CteCtx is the per-statement CTE execution context, threaded through exec_* and EvalEnv so a FROM
// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). `modes` and
// `plans` are fixed after planning; `buffers` is filled before the main query runs — one slot per
// CTE in list order, holding the materialized rows of a "materialize" CTE (an empty placeholder for
// an "inline" one, whose body is run in place from `plans` instead). EMPTY_CTE_CTX is the empty
// context for every non-WITH execution path.
type CteCtx = { modes: CteMode[]; plans: QueryPlan[]; buffers: Row[][] };
const EMPTY_CTE_CTX: CteCtx = { modes: [], plans: [], buffers: [] };

// EvalEnv is the environment threaded into the per-row evaluator (spec/design/grammar.md §26): the
// bound parameters, the stack of enclosing rows (innermost LAST) a correlated reference reads, and
// a runSubquery callback (a correlated subquery re-runs its inner plan against the pushed stack).
// outer is empty at the top level; an outerColumn at frame `level` reads outer[outer.length-level].
type EvalEnv = {
  params: Value[];
  outer: Row[];
  runSubquery(plan: QueryPlan, outer: Row[]): SelectResult;
  // The entropy+clock seam (spec/design/entropy.md §5): `seam` is the handle's injected random/clock
  // functions (a reference to the Database's Seam — handle-scoped); `rng` is the per-statement
  // uuidv7 counter + once-resolved clock. Only the volatile uuid generators touch either.
  seam: Seam;
  rng: StmtRng;
  // The statement's CTE execution context (spec/design/cte.md §5), so a FROM reference at any
  // nesting depth delivers a CTE's rows. EMPTY_CTE_CTX for every non-WITH statement.
  ctes: CteCtx;
  // The executing Database handle, so the sequence value functions (nextval/currval — sequences.md
  // §4/§6) can resolve a name to a catalog sequence and advance/read it (mirrors Rust's env.exec —
  // the same access the clock seam already uses). Only nextval/currval touch it.
  exec: Database;
};

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
  | "sumInt" // SUM(i16|i32) — accumulate i64, result i64 (trap at i64)
  | "sumDecimal" // SUM(i64|decimal) — accumulate decimal, result decimal
  | "avg" // AVG — decimal sum + count; result sum/count (NULL if count 0)
  // SUM/AVG over float: the ORDER-INDEPENDENT CANONICAL-ORDER FOLD (float.md §7). Inputs are
  // COLLECTED (not streamed), then at finalize: resolve NaN/±Inf, -0-canonicalize + sort the finite
  // values by the total order, fold left at the width (fround per add for f32), trapping 22003
  // on overflow. Result keeps the input width (same_as_input). avgFloat divides the sum by the
  // count, rounded once. The width rides on the Acc.
  | "sumFloat"
  | "avgFloat"
  | "min"
  | "max";

// AggSpec is one resolved aggregate: its plan and its resolved argument (evaluated per input
// row against the real row). operand is null for COUNT(*).
type AggSpec = { plan: AggPlan; operand: RExpr | null; floatWidth?: ScalarType };

// AggCtx threads the aggregate-resolution mode through resolve. collecting === false is the
// Forbidden mode (a funcCall is 42803; columns resolve normally); collecting === true is an
// aggregate query's projection (a funcCall collects into specs and resolves to a synthetic slot
// groupKeys.length + index; a column resolves to its position among groupKeys if it is a
// grouping key, else 42803). groupKeys holds the resolved flat indices of the GROUP BY columns
// (empty for whole-table aggregation). The synthetic row is [group_key_values..., agg_results...].
type AggCtx = { collecting: boolean; groupKeys: number[]; specs: AggSpec[] };

// Acc is a running aggregate accumulator (one per AggSpec), folded per input row then finalized.
// For float SUM/AVG the inputs are COLLECTED in `floats` (the canonical fold needs all values up
// front — float.md §7), with `floatWidth` the input width fixed at resolve.
type Acc = {
  plan: AggPlan;
  count: bigint;
  sumInt: bigint;
  sumDec: Decimal;
  seen: boolean;
  cur: Value | null;
  floats: number[];
  floatWidth: ScalarType;
};

function newAcc(plan: AggPlan, floatWidth: ScalarType = "f64"): Acc {
  return {
    plan,
    count: 0n,
    sumInt: 0n,
    sumDec: Decimal.fromBigInt(0n),
    seen: false,
    cur: null,
    floats: [],
    floatWidth,
  };
}

// foldAcc folds one input value into the accumulator. NULL arguments are skipped (COUNT(*)
// ignores the value and always counts). Traps 22003 on SUM overflow at the result bound.
// A decimal SUM/AVG fold charges size-scaled decimal_work against the running accumulator
// (the `+` formula — spec/design/cost.md §3 "decimal_work"); MIN/MAX folds are direct Value
// compares like the sort's and stay unmetered.
function foldAcc(a: Acc, v: Value, m: Meter): void {
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
        if (!inRange("i64", s)) throw overflow("i64");
        a.sumInt = s;
        a.seen = true;
      }
      break;
    case "sumDecimal":
      if (v.kind !== "null") {
        const inc = toDecimal(v);
        m.charge(COSTS.decimalWork * BigInt(workLinear(a.sumDec, inc) - 1));
        m.guard();
        // Uncapped: the running sum may exceed the §2 format cap mid-fold; only the FINAL
        // result is cap-checked (in finalizeAcc), matching PG and making the trap
        // order-independent (spec/design/decimal.md §2, determinism.md §7).
        a.sumDec = a.sumDec.addUncapped(inc);
        a.seen = true;
      }
      break;
    case "avg":
      if (v.kind !== "null") {
        const inc = toDecimal(v);
        m.charge(COSTS.decimalWork * BigInt(workLinear(a.sumDec, inc) - 1));
        m.guard();
        // Uncapped (as sumDecimal): the average's final divide brings the value back in range,
        // so AVG never traps on an over-cap intermediate sum the way PG does not.
        a.sumDec = a.sumDec.addUncapped(inc);
        a.count += 1n;
      }
      break;
    case "sumFloat":
    case "avgFloat":
      // Float SUM/AVG: COLLECT the inputs for the canonical-order fold at finalize (float.md §7).
      // NULL is skipped (every aggregate). The fold itself (sort + width-correct add) runs once at
      // finalize, so per-row cost stays the structural aggregate_accumulate already charged.
      if (v.kind === "f32" || v.kind === "f64") {
        a.floats.push(v.value);
        a.count += 1n;
        a.seen = true;
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
      // checkCap is the only cap check for the fold: the FINAL sum traps 22003 if over the §2
      // cap (PG's make_result), but no intermediate does (decimal.md §2).
      return a.seen ? decimalValue(a.sumDec.checkCap()) : nullValue();
    case "avg":
      // div cap-checks its (in-range) result; the over-cap-capable running sum is never surfaced
      // directly, so AVG matches PG even when SUM would overflow.
      return a.count === 0n ? nullValue() : decimalValue(a.sumDec.div(Decimal.fromBigInt(a.count)));
    case "sumFloat": {
      if (!a.seen) return nullValue(); // empty / all-NULL group → NULL
      const s = floatCanonicalSum(a.floats, a.floatWidth);
      return a.floatWidth === "f32" ? float32Value(s) : float64Value(s);
    }
    case "avgFloat": {
      if (a.count === 0n) return nullValue();
      // AVG = SUM / count, the division ROUNDED ONCE at the input width (float.md §7). count is
      // exact; Number(count) is safe for any plausible group size.
      const s = floatCanonicalSum(a.floats, a.floatWidth);
      const avg = s / Number(a.count);
      const r = a.floatWidth === "f32" ? Math.fround(avg) : avg;
      return a.floatWidth === "f32" ? float32Value(r) : float64Value(r);
    }
    case "min":
    case "max":
      return a.cur ?? nullValue();
  }
}

// floatCanonicalSum is the ORDER-INDEPENDENT CANONICAL-ORDER FOLD (float.md §7), bit-identical
// across cores and across any serial/parallel plan. Steps:
//   1. Special values first (order-independent): any NaN → NaN; else if both +Inf and -Inf → NaN;
//      else if +Inf present → +Inf; else if -Inf present → -Inf; else all-finite → step 2.
//   2. Canonicalize each finite value -0 → +0, then SORT by the total order (floatTotalCmp).
//   3. FOLD LEFT with width-correct IEEE addition (Math.fround each add for f32). A running
//      total overflowing to ±Inf traps 22003 (the finite-overflow rule; PG yields ±Inf — a
//      documented divergence). `caller` builds the f32/f64 Value.
function floatCanonicalSum(values: number[], width: ScalarType): number {
  let anyNaN = false;
  let posInf = false;
  let negInf = false;
  const finite: number[] = [];
  for (const v of values) {
    if (Number.isNaN(v)) anyNaN = true;
    else if (v === Infinity) posInf = true;
    else if (v === -Infinity) negInf = true;
    else finite.push(canonFloat(v)); // -0 → +0
  }
  if (anyNaN) return NaN;
  if (posInf && negInf) return NaN;
  if (posInf) return Infinity;
  if (negInf) return -Infinity;
  // All finite: sort by the total order (after -0 canonicalization, distinct values have distinct
  // keys, so the sort is total and deterministic — every core sees the same sequence).
  finite.sort(floatTotalCmp);
  const f32 = width === "f32";
  let acc = 0; // +0 start; adding to it preserves the first value's sign correctly under IEEE
  for (const v of finite) {
    acc = acc + v;
    if (f32) acc = Math.fround(acc);
    if (!Number.isFinite(acc)) throw overflow(width); // running total overflowed to ±Inf
  }
  return acc;
}

// itemsHaveAggregate reports whether any select item contains an aggregate call.
function itemsHaveAggregate(items: SelectItems): boolean {
  if (items.kind === "all") return false;
  return items.items.some((it) => exprHasAggregate(it.expr));
}

// isAggregateName reports whether name (case-insensitive) is one of the five aggregates.
function isAggregateName(name: string): boolean {
  const lname = name.toLowerCase();
  return AGGREGATES.some((a) => a.surface.toLowerCase() === lname);
}

// astSubscriptExprs is the sub-expressions of a list of AST subscript specs (each index, or a
// slice's present bounds) — for the Expr tree walkers (spec/design/array.md §6).
function astSubscriptExprs(subs: SubscriptSpec[]): Expr[] {
  const out: Expr[] = [];
  for (const s of subs) {
    if (!s.isSlice) out.push(s.index);
    else {
      if (s.lower !== null) out.push(s.lower);
      if (s.upper !== null) out.push(s.upper);
    }
  }
  return out;
}

// exprHasAggregate reports whether an expression tree contains an AGGREGATE call anywhere. A
// scalar-function call is not itself an aggregate but may CONTAIN one (abs(sum(x))), so its
// arguments are walked.
function exprHasAggregate(e: Expr): boolean {
  switch (e.kind) {
    case "funcCall":
      return isAggregateName(e.name) || e.args.some(exprHasAggregate);
    case "cast":
      return exprHasAggregate(e.inner);
    case "unary":
      return exprHasAggregate(e.operand);
    case "isNull":
      return exprHasAggregate(e.operand);
    case "binary":
    case "isDistinct":
      return exprHasAggregate(e.lhs) || exprHasAggregate(e.rhs);
    case "in":
      return exprHasAggregate(e.lhs) || e.list.some(exprHasAggregate);
    case "between":
      return exprHasAggregate(e.lhs) || exprHasAggregate(e.lo) || exprHasAggregate(e.hi);
    case "like":
      return exprHasAggregate(e.lhs) || exprHasAggregate(e.rhs);
    case "case":
      return (
        (e.operand !== null && exprHasAggregate(e.operand)) ||
        e.whens.some((w) => exprHasAggregate(w.cond) || exprHasAggregate(w.result)) ||
        (e.els !== null && exprHasAggregate(e.els))
      );
    case "row":
      return e.fields.some(exprHasAggregate);
    case "array":
      return e.elements.some(exprHasAggregate);
    case "fieldAccess":
    case "fieldStar":
      return exprHasAggregate(e.base);
    case "subscript":
      return exprHasAggregate(e.base) || astSubscriptExprs(e.subscripts).some(exprHasAggregate);
    case "quantified":
      return exprHasAggregate(e.lhs) || exprHasAggregate(e.array);
    default:
      return false;
  }
}

// NamedCheck is one statement-resolved CHECK constraint: its name (for the 23514 message)
// and the resolved expression evaluated per candidate row.
type NamedCheck = { name: string; node: RExpr };

// evalChecks evaluates a row's CHECK constraints in name order (constraints.md §4.4):
// TRUE and NULL pass; the first FALSE aborts with 23514 and PG's message. Shared by the
// INSERT and UPDATE write paths.
function evalChecks(checks: NamedCheck[], relation: string, row: Row, env: EvalEnv, meter: Meter): void {
  for (const c of checks) {
    const v = evalExpr(c.node, row, env, meter);
    if (v.kind === "bool" && !v.value) {
      throw engineError(
        "check_violation",
        "new row for relation " + relation + " violates check constraint " + c.name,
      );
    }
  }
}

// rejectCheckStructure applies the structural CHECK-expression rejections
// (spec/design/constraints.md §4.1) in a single depth-first pre-order walk before
// resolution: a subquery is 0A000, an aggregate call 42803, a bind parameter 42P02 — PG's
// codes and messages (oracle-probed; PG interleaves these with resolution in parse order,
// a documented micro-order divergence).
function rejectCheckStructure(e: Expr): void {
  switch (e.kind) {
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      throw engineError("feature_not_supported", "cannot use subquery in check constraint");
    case "param":
      throw engineError("undefined_parameter", "there is no parameter $" + e.index.toString());
    case "funcCall":
      if (isAggregateName(e.name)) {
        throw engineError("grouping_error", "aggregate functions are not allowed in check constraints");
      }
      for (const a of e.args) rejectCheckStructure(a);
      return;
    case "cast":
      return rejectCheckStructure(e.inner);
    case "unary":
    case "isNull":
      return rejectCheckStructure(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
      rejectCheckStructure(e.lhs);
      return rejectCheckStructure(e.rhs);
    case "in":
      rejectCheckStructure(e.lhs);
      for (const elem of e.list) rejectCheckStructure(elem);
      return;
    case "between":
      rejectCheckStructure(e.lhs);
      rejectCheckStructure(e.lo);
      return rejectCheckStructure(e.hi);
    case "case":
      if (e.operand !== null) rejectCheckStructure(e.operand);
      for (const w of e.whens) {
        rejectCheckStructure(w.cond);
        rejectCheckStructure(w.result);
      }
      if (e.els !== null) rejectCheckStructure(e.els);
      return;
    case "row":
      for (const f of e.fields) rejectCheckStructure(f);
      return;
    case "array":
      for (const el of e.elements) rejectCheckStructure(el);
      return;
    case "fieldAccess":
    case "fieldStar":
      return rejectCheckStructure(e.base);
    case "subscript":
      rejectCheckStructure(e.base);
      for (const x of astSubscriptExprs(e.subscripts)) rejectCheckStructure(x);
      return;
    case "quantified":
      rejectCheckStructure(e.lhs);
      return rejectCheckStructure(e.array);
    default: // column, qualifiedColumn, literal
      return;
  }
}

// rejectDefaultStructure is the structural pre-walk for a DEFAULT expression
// (spec/design/constraints.md §2), run before name/type resolution (the same micro-order
// divergence from PG that rejectCheckStructure carries). A default extends the CHECK rejections
// with one more: it may NOT reference a column (it is computed before the row exists). Codes
// match PostgreSQL (oracle-probed): a column reference / subquery is 0A000, an aggregate 42803,
// a parameter 42P02.
function rejectDefaultStructure(e: Expr): void {
  switch (e.kind) {
    case "column":
    case "qualifiedColumn":
      throw engineError("feature_not_supported", "cannot use column reference in DEFAULT expression");
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      throw engineError("feature_not_supported", "cannot use subquery in DEFAULT expression");
    case "param":
      throw engineError("undefined_parameter", "there is no parameter $" + e.index.toString());
    case "funcCall":
      if (isAggregateName(e.name)) {
        throw engineError("grouping_error", "aggregate functions are not allowed in DEFAULT expressions");
      }
      for (const a of e.args) rejectDefaultStructure(a);
      return;
    case "cast":
      return rejectDefaultStructure(e.inner);
    case "unary":
    case "isNull":
      return rejectDefaultStructure(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
      rejectDefaultStructure(e.lhs);
      return rejectDefaultStructure(e.rhs);
    case "in":
      rejectDefaultStructure(e.lhs);
      for (const elem of e.list) rejectDefaultStructure(elem);
      return;
    case "between":
      rejectDefaultStructure(e.lhs);
      rejectDefaultStructure(e.lo);
      return rejectDefaultStructure(e.hi);
    case "case":
      if (e.operand !== null) rejectDefaultStructure(e.operand);
      for (const w of e.whens) {
        rejectDefaultStructure(w.cond);
        rejectDefaultStructure(w.result);
      }
      if (e.els !== null) rejectDefaultStructure(e.els);
      return;
    case "row":
      for (const f of e.fields) rejectDefaultStructure(f);
      return;
    case "array":
      for (const el of e.elements) rejectDefaultStructure(el);
      return;
    case "fieldAccess":
    case "fieldStar":
      return rejectDefaultStructure(e.base);
    case "subscript":
      rejectDefaultStructure(e.base);
      for (const x of astSubscriptExprs(e.subscripts)) rejectDefaultStructure(x);
      return;
    case "quantified":
      rejectDefaultStructure(e.lhs);
      return rejectDefaultStructure(e.array);
    default: // literal, typedLiteral
      return;
  }
}

// checkReferencedColumns returns the distinct columns a CHECK expression references, as
// indices into columns — the input to PG's auto-naming rule (constraints.md §4.3: exactly
// one distinct column → <table>_<col>_check). Resolution already validated every
// reference, so an unknown name is simply skipped; a qualified reference counts its column
// like a bare one (oracle-probed).
function checkReferencedColumns(e: Expr, columns: Column[]): number[] {
  const out: number[] = [];
  const note = (name: string): void => {
    const lower = name.toLowerCase();
    const i = columns.findIndex((c) => c.name.toLowerCase() === lower);
    if (i >= 0 && !out.includes(i)) out.push(i);
  };
  const walk = (e: Expr): void => {
    switch (e.kind) {
      case "column":
      case "qualifiedColumn":
        note(e.name);
        return;
      case "cast":
        return walk(e.inner);
      case "unary":
      case "isNull":
        return walk(e.operand);
      case "binary":
      case "isDistinct":
      case "like":
        walk(e.lhs);
        return walk(e.rhs);
      case "in":
        walk(e.lhs);
        for (const elem of e.list) walk(elem);
        return;
      case "between":
        walk(e.lhs);
        walk(e.lo);
        return walk(e.hi);
      case "case":
        if (e.operand !== null) walk(e.operand);
        for (const w of e.whens) {
          walk(w.cond);
          walk(w.result);
        }
        if (e.els !== null) walk(e.els);
        return;
      case "funcCall":
        for (const a of e.args) walk(a);
        return;
      case "row":
        for (const f of e.fields) walk(f);
        return;
      case "array":
        for (const el of e.elements) walk(el);
        return;
      case "fieldAccess":
      case "fieldStar":
        return walk(e.base);
      case "subscript":
        walk(e.base);
        for (const x of astSubscriptExprs(e.subscripts)) walk(x);
        return;
      case "quantified":
        walk(e.lhs);
        return walk(e.array);
      default: // literal, param; subqueries unreachable in a validated check
        return;
    }
  };
  walk(e);
  return out;
}

// resolveAggregate resolves an aggregate call into a synthetic-row reference, collecting its
// AggSpec. Valid only in collect mode; in Forbidden mode (WHERE/ON/nested) it is 42803. The
// operand resolves in a fresh Forbidden sub-context (a nested aggregate is 42803; its columns
// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
function resolveAggregate(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
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
  // The input float width for a float SUM/AVG (so the canonical fold rounds at the right width).
  let floatWidth: ScalarType | undefined;
  if (e.star) {
    // Only COUNT has a star overload (aggregates.md §3); SUM(*) etc. is a syntax error.
    if (!aggregateHasStar(name)) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    plan = "countStar";
    operand = null;
    result = { kind: "int", ty: "i64" };
  } else {
    // One operand, resolved in a fresh Forbidden sub-context. The registry validates the (surface,
    // operand-family) overload exists (else 42883) and yields its result code; the plan + result
    // type follow from it (the PG widening). Each aggregate takes exactly one argument.
    if (e.args.length !== 1) {
      throw engineError("undefined_function", "no aggregate function matches the given argument count");
    }
    const r = resolve(scope, e.args[0], null, sub, params);
    operand = r.node;
    const desc = lookupAggregateOverload(name, r.type);
    if (!desc) throw noAggOverload(name);
    [plan, result, floatWidth] = aggregatePlan(name, desc.result, r.type);
  }
  // Aggregate results follow the group-key values in the synthetic row.
  const slot = ag.groupKeys.length + ag.specs.length;
  ag.specs.push({ plan, operand, floatWidth });
  return { node: { kind: "column", index: slot }, type: result };
}

// collectColumn resolves a column reference (already at real flat index `idx`) under an
// aggregate context. In Forbidden mode it reads the real row directly; in collect mode it must
// be a grouping key — resolved to its synthetic-row slot (its position among the group keys) —
// else 42803.
function collectColumn(scope: Scope, ag: AggCtx, idx: number, name: string): { node: RExpr; type: ResolvedType } {
  const type = resolvedTypeOfCol(scope.columnAt(idx).type, scope.catalog);
  if (!ag.collecting) return { node: { kind: "column", index: idx }, type };
  const pos = ag.groupKeys.indexOf(idx);
  if (pos < 0) throw groupingErrorColumn(name);
  return { node: { kind: "column", index: pos }, type };
}

// noAggOverload is 42883 — an aggregate over an operand family it has no overload for.
function noAggOverload(fn: string): EngineError {
  return engineError("undefined_function", "no " + fn + " aggregate for that argument type");
}

// noFuncOverload is 42883 — a scalar function over argument types it has no overload for.
function noFuncOverload(fn: string): EngineError {
  return engineError("undefined_function", "no " + fn + " function for those argument types");
}

// === Function registry (spec/design/extensibility.md §5) ============================
// Resolution for the named scalar functions and the aggregates is DATA-DRIVEN: instead of
// re-encoding the name set in hand-written switches (the old known-name gate + result-type match
// + name→variant match), it consults the generated catalog descriptor tables (OPERATORS rows
// with kind === "function", and AGGREGATES) through the lookups below, keyed by (name,
// arg_families). The per-row KERNEL is still reached by id (the `func` name / the `plan`) and
// hand-written per core — §5 forbids codegenning the kernels. The only function-specific
// hand-written data are the result-code / plan interpreters; the spec_constants test proves them
// total over the catalog. Host-registered functions would extend these lookups.

// argFamily is the family a resolved type satisfies, for matching a catalog argFamilies slot.
// null for the NULL family: an untyped NULL matches no *concrete* family (so abs(NULL)/sum(NULL)
// find no overload — 42883), and only the wildcard "any" slot accepts it.
function argFamily(t: ResolvedType): string | null {
  switch (t.kind) {
    case "int":
      return "integer";
    case "decimal":
      return "decimal";
    case "float":
      return "float";
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    case "composite":
      // No catalog function takes a composite this slice; it matches no concrete family (only the
      // wildcard "any" slot, via familyMatches) — spec/design/composite.md.
      return null;
    case "array":
      // No built-in function/aggregate argument family for arrays this slice.
      return null;
    case "null":
      return null;
  }
}

// familyMatches reports whether a resolved argument satisfies one catalog family slot. "any"
// accepts everything (NULL included); a concrete family matches only its own type.
function familyMatches(slot: string, t: ResolvedType): boolean {
  return slot === "any" || argFamily(t) === slot;
}

// isScalarFuncName reports whether name (lowercased) is a registered scalar function (catalog
// kind === "function") — the data-driven replacement for the old hand-written known-name gate.
function isScalarFuncName(name: string): boolean {
  return OPERATORS.some((o) => o.kind === "function" && o.name === name);
}

// isVariadicFuncName reports whether name (lowercased) is a VARIADIC scalar function
// (array-functions.md §12) — a kind === "function" row with `variadic` set (num_nulls/num_nonnulls).
function isVariadicFuncName(name: string): boolean {
  return OPERATORS.some((o) => o.kind === "function" && o.variadic && o.name === name);
}

// lookupScalarOverload returns the matched scalar-function overload row for name over the resolved
// argument types: the kind === "function" catalog row whose argFamilies agree by arity + per-slot
// family. undefined ⇒ no overload (42883). make_interval resolves on its own path (§11).
function lookupScalarOverload(name: string, tys: ResolvedType[]): OperatorDesc | undefined {
  return OPERATORS.find(
    (o) =>
      o.kind === "function" &&
      o.name === name &&
      o.argFamilies.length === tys.length &&
      o.argFamilies.every((slot, i) => familyMatches(slot, tys[i]!)),
  );
}

// resolvedScalarType is the concrete ScalarType carried by a numeric resolved type (for the
// "promoted" / "same_as_input" result rules). Only reached for the numeric families they admit.
function resolvedScalarType(t: ResolvedType): ScalarType {
  switch (t.kind) {
    case "int":
    case "float":
      return t.ty;
    case "decimal":
      return "decimal";
    default:
      throw new Error("resolvedScalarType: non-numeric operand");
  }
}

// scalarResultType is the result ScalarType of a scalar function from its catalog result code
// (functions.md §9): "promoted" = the (single) operand's own type; otherwise the code is a literal
// scalar-type id (e.g. "decimal", "f64", "interval", "i16", "timestamptz", "uuid").
function scalarResultType(code: string, tys: ResolvedType[]): ScalarType {
  if (code === "promoted") return resolvedScalarType(tys[0]!);
  const ty = scalarTypeFromName(code);
  if (ty === undefined) throw new Error("scalarResultType: unknown result code " + code);
  return ty;
}

// aggregateHasStar reports whether aggregate surface (lowercased) has a COUNT(*)-style star
// overload — only COUNT does. The data-driven replacement for the special-cased star arm.
function aggregateHasStar(surface: string): boolean {
  return AGGREGATES.some((a) => a.surface.toLowerCase() === surface && a.arg === "star");
}

// lookupAggregateOverload returns the matched aggregate overload row for surface (lowercased) over
// a single operand of resolved type t: the arg === "expr" catalog row whose lone argFamilies slot
// matches. undefined ⇒ no overload (42883, e.g. SUM(text)). MIN/MAX/COUNT take "any".
function lookupAggregateOverload(surface: string, t: ResolvedType): AggregateDesc | undefined {
  return AGGREGATES.find(
    (a) =>
      a.surface.toLowerCase() === surface &&
      a.arg === "expr" &&
      a.argFamilies.length === 1 &&
      familyMatches(a.argFamilies[0]!, t),
  );
}

// aggregatePlan is the runtime plan + result type (+ the float width that rides on the Acc) for an
// aggregate over operand type t, from the matched overload's surface + catalog result code (the PG
// widening — aggregates.md §3). The plan is the aggregate's kernel id (fold/finalize switch on it);
// selecting it from the registered result code keeps the name gate + overload validation
// data-driven while the kernel stays hand-written (§5). surface is the lowercased call name.
function aggregatePlan(
  surface: string,
  code: string,
  t: ResolvedType,
): [AggPlan, ResolvedType, ScalarType | undefined] {
  if (surface === "count") return ["count", { kind: "int", ty: "i64" }, undefined];
  if (surface === "sum" && code === "sum_widen") {
    // SUM(i16|i32) → i64; SUM(i64) → decimal (PG widening).
    if (t.kind === "int" && t.ty === "i64") return ["sumDecimal", { kind: "decimal" }, undefined];
    return ["sumInt", { kind: "int", ty: "i64" }, undefined];
  }
  if (surface === "sum" && code === "decimal") return ["sumDecimal", { kind: "decimal" }, undefined];
  if (surface === "sum" && code === "same_as_input" && t.kind === "float") {
    // SUM/AVG over float stay the input width (the canonical-order fold — float.md §7).
    return ["sumFloat", { kind: "float", ty: t.ty }, t.ty];
  }
  if (surface === "avg" && code === "decimal") return ["avg", { kind: "decimal" }, undefined];
  if (surface === "avg" && code === "same_as_input" && t.kind === "float") {
    return ["avgFloat", { kind: "float", ty: t.ty }, t.ty];
  }
  if (surface === "min" && code === "same_as_input") return ["min", t, undefined];
  if (surface === "max" && code === "same_as_input") return ["max", t, undefined];
  throw new Error(`aggregatePlan: unhandled (${surface}, ${code})`);
}

// resolveFuncCall resolves a function call: an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar
// function (abs/round/…, spec/design/functions.md §9), the named/defaulted make_interval (§11), or
// 42883 for any other name. Aggregates and scalar functions share the call syntax (grammar.md §17);
// they are distinguished here. Named notation (name => value) is valid only for a function that
// declares parameter names (make_interval); on every other function it is 42883.
function resolveFuncCall(
  scope: Scope,
  e: { name: string; args: Expr[]; argNames: (string | null)[]; star: boolean; variadic: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const lname = e.name.toLowerCase();
  // The VARIADIC keyword is valid only on a VARIADIC function (array-functions.md §12); on any
  // other (non-variadic) name it is 42883 (no such overload). Caught before the per-kind dispatch.
  if (e.variadic && !isVariadicFuncName(lname)) throw noFuncOverload(lname);
  if (isVariadicFuncName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveVariadicFunc(scope, e, ag, params);
  }
  // make_interval is the one named/defaulted function — it keeps its own resolver (§11).
  if (lname === "make_interval") return resolveMakeInterval(scope, e, ag, params);
  // Otherwise the registry (the catalog descriptor tables) decides whether the name is an
  // aggregate, a scalar function, or undefined — no hand-written name lists (extensibility.md §5).
  if (isAggregateName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveAggregate(scope, e, ag, params);
  }
  // The polymorphic array functions (array-functions.md §2) are also kind === "function", so they
  // must be intercepted BEFORE the generic scalar path — their anyarray/anyelement slots need §2
  // unification, which lookupScalarOverload's exact-family match cannot do.
  if (isArrayFuncName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveArrayFunc(scope, e, ag, params);
  }
  if (isScalarFuncName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveScalarFunc(scope, e, ag, params);
  }
  throw engineError("undefined_function", "function does not exist: " + e.name);
}

// rejectNamed throws 42883 if any argument is named — named notation is valid only for a function
// that declares parameter names (PG's "function ... has no parameter named X").
function rejectNamed(name: string, argNames: (string | null)[]): void {
  for (const n of argNames) {
    if (n !== null) {
      throw engineError("undefined_function", 'function ' + name + ' has no parameter named "' + n + '"');
    }
  }
}

// scalarFuncDesc returns the lone scalar-function catalog row of this name (e.g. make_interval),
// reading named/default/family metadata for named-notation resolution (functions.md §11) from the
// generated catalog table (CLAUDE.md §5) rather than re-hardcoding it.
function scalarFuncDesc(name: string): OperatorDesc | undefined {
  return OPERATORS.find((o) => o.kind === "function" && o.name === name);
}

// familyHint is the type context offered to an untyped literal in a function-argument slot of the
// given family, so it adapts (functions.md §11): an integer slot offers i64, a float slot offers
// f64 (so a bare 0/1.5 becomes f64 for secs). Other families offer no hint (null).
function familyHint(family: string): ScalarType | null {
  if (family === "integer") return "i64";
  if (family === "float") return "f64";
  return null;
}

// defaultExpr materializes a catalog DEFAULT (an integer-literal string, verify.rb-checked) as an
// Expr so an omitted trailing argument resolves through the normal literal path — adapting to its
// slot's family (e.g. "0" → f64 for secs). functions.md §11.
function defaultExpr(lit: string): Expr {
  return { kind: "literal", literal: { kind: "int", int: BigInt(lit) } };
}

// normalizeNamedArgs maps a call's positional + named arguments onto a function's positional
// parameter slots, filling omitted trailing slots from desc.argDefaults (PostgreSQL named notation
// + DEFAULTs, functions.md §11). Returns the positional Expr array of length desc.arity. Errors:
// 42601 a positional arg after a named one (also caught at parse) or a duplicated name; 42883 an
// unknown parameter name, too many arguments, or a missing non-defaulted slot (no overload).
function normalizeNamedArgs(desc: OperatorDesc, args: Expr[], argNames: (string | null)[]): Expr[] {
  const arity = desc.arity;
  const slots: (Expr | null)[] = new Array(arity).fill(null);
  const namesEmpty = argNames.length === 0;
  let seenNamed = false;
  for (let i = 0; i < args.length; i++) {
    const nm = namesEmpty ? null : argNames[i];
    if (nm === null || nm === undefined) {
      if (seenNamed) {
        throw engineError("syntax_error", "positional argument cannot follow named argument");
      }
      if (i >= arity) throw noFuncOverload(desc.name); // too many positional arguments
      slots[i] = args[i]!;
      continue;
    }
    seenNamed = true;
    const idx = desc.argNames.findIndex((p) => p.toLowerCase() === nm.toLowerCase());
    if (idx < 0) {
      throw engineError("undefined_function", 'function ' + desc.name + ' has no parameter named "' + nm + '"');
    }
    if (slots[idx] !== null) {
      throw engineError("syntax_error", 'argument name "' + nm + '" used more than once');
    }
    slots[idx] = args[i]!;
  }
  const firstDefaulted = arity - desc.argDefaults.length;
  const out: Expr[] = [];
  for (let i = 0; i < arity; i++) {
    const slot = slots[i];
    if (slot !== null) {
      out.push(slot);
    } else if (i >= firstDefaulted) {
      out.push(defaultExpr(desc.argDefaults[i - firstDefaulted]!));
    } else {
      throw noFuncOverload(desc.name); // missing required argument
    }
  }
  return out;
}

// resolveMakeInterval resolves make_interval(years, months, weeks, days, hours, mins, secs) — the
// engine's first named + defaulted function (functions.md §11). Normalize named/positional args +
// defaults onto the seven slots, resolve each with its declared family as the type hint (so a bare
// numeric literal adapts to the f64 secs slot), and emit a make_interval node. The arguments
// keep their families (no promotion); a wrong family in a slot is 42883.
function resolveMakeInterval(
  scope: Scope,
  e: { name: string; args: Expr[]; argNames: (string | null)[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const desc = scalarFuncDesc("make_interval");
  if (desc === undefined) throw new Error("make_interval is in the catalog");
  const positional = normalizeNamedArgs(desc, e.args, e.argNames);
  const rargs: RExpr[] = [];
  for (let i = 0; i < positional.length; i++) {
    const fam = desc.argFamilies[i]!;
    const r = resolve(scope, positional[i]!, familyHint(fam), ag, params);
    // Type-check against the declared family. A NULL adapts (NULL propagates); a f32 secs is
    // read at eval and widened losslessly to f64 (no cast node — cost matches the cores).
    const ok =
      r.type.kind === "null" ||
      (fam === "integer" && r.type.kind === "int") ||
      (fam === "float" && r.type.kind === "float");
    if (!ok) throw noFuncOverload("make_interval");
    rargs.push(r.node);
  }
  return scalarFuncNode("make_interval", rargs, "interval", undefined);
}

// f64ToMicros converts make_interval's secs (double precision) to a microsecond count: one
// correctly-rounded multiply, rounded half-away-from-zero to a bigint (the engine's one mode —
// float.md §6, via floatToIntHalfAway). A non-finite or out-of-i64-range product traps 22008
// (interval out of range), matching PG and the other cores.
function f64ToMicros(secs: number): bigint {
  const p = secs * 1_000_000;
  if (!Number.isFinite(p)) throw engineError("datetime_field_overflow", "interval out of range");
  const r = floatToIntHalfAway(p); // bigint, half-away-from-zero
  if (r < -9223372036854775808n || r > 9223372036854775807n) {
    throw engineError("datetime_field_overflow", "interval out of range");
  }
  return r;
}

// resolveScalarFunc resolves a scalar-function call (abs/round) into a per-row scalarFunc node.
// Unlike an aggregate it is legal in any context, so its arguments resolve in the SAME ag
// context (a nested aggregate is still collected in a projection and 42803 in WHERE). The
// overload is picked by the argument families; no match is 42883. spec/design/functions.md §9.
function resolveScalarFunc(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase() as ScalarFuncName;
  const rargs: RExpr[] = [];
  const tys: ResolvedType[] = [];
  for (const a of e.args) {
    const r = resolve(scope, a, null, ag, params);
    rargs.push(r.node);
    tys.push(r.type);
  }
  // Pick the overload by argument families and its result type by the catalog `result` code
  // (extensibility.md §5) — replacing the old hand-written chain of (name, arg-types) checks.
  const desc = lookupScalarOverload(name, tys);
  if (!desc) throw noFuncOverload(name);
  // Every float function computes at the operand's float width (argWidth), so a f32 operand
  // rounds at binary32 even where the catalog's result is f64; abs(float) also keeps that width
  // as its result. Non-float args carry no width. pow is the one (float, float) function — it
  // promotes its mixed-width pair to a common width and widens both arguments to it.
  let argWidth: ScalarType | undefined;
  if (name === "pow" && tys[0].kind === "float" && tys[1].kind === "float") {
    argWidth = promoteFloat(tys[0].ty, tys[1].ty);
    rargs[0] = widenFloatTo(rargs[0]!, tys[0].ty, argWidth);
    rargs[1] = widenFloatTo(rargs[1]!, tys[1].ty, argWidth);
  } else if (tys.length >= 1 && tys[0].kind === "float") {
    argWidth = tys[0].ty;
  }
  const result = scalarResultType(desc.result, tys);
  return scalarFuncNode(name, rargs, result, argWidth);
}

// scalarFuncNode builds a resolved scalar-function node + its public type.
function scalarFuncNode(
  func: ScalarFuncName,
  args: RExpr[],
  result: ScalarType,
  argWidth: ScalarType | undefined,
): { node: RExpr; type: ResolvedType } {
  return { node: { kind: "scalarFunc", func, args, result, argWidth }, type: resolvedTypeOf(result) };
}

// resolveVariadicFunc resolves a VARIADIC scalar-function call (num_nulls/num_nonnulls —
// array-functions.md §12). The lone catalog row's last parameter is variadic; the call is EITHER a
// spread of trailing arguments OR (with the VARIADIC keyword) a single array passed directly.
// Non-strict (null = "none"): the node carries no blanket NULL short-circuit. The result type is the
// catalog `result` (i32 here), independent of the arguments.
function resolveVariadicFunc(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean; variadic: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase() as VariadicFuncName;
  const desc = scalarFuncDesc(name)!;
  const k = desc.arity; // declared parameter count (the last is variadic)
  const varFamily = desc.argFamilies[k - 1]!; // the variadic element family (last slot)
  const rargs: RExpr[] = [];

  if (e.variadic) {
    // VARIADIC-array form: exactly k args (fixed params + the one array). The fixed params match
    // their concrete families; the last operand MUST be an array (else 42804).
    if (e.args.length !== k) throw noFuncOverload(name);
    for (let i = 0; i < e.args.length; i++) {
      const r = resolve(scope, e.args[i]!, null, ag, params);
      if (i + 1 === k) {
        // the variadic (array) operand
        if (r.type.kind !== "array") {
          // A non-array operand (incl. a bare untyped NULL) is 42804 — PG's exact code.
          throw engineError("datatype_mismatch", "VARIADIC argument must be an array");
        }
        // "any" accepts any element type; a concrete variadic family must match.
        if (varFamily !== "any" && !familyMatches(varFamily, r.type.elem)) throw noFuncOverload(name);
      } else if (!familyMatches(desc.argFamilies[i]!, r.type)) {
        throw noFuncOverload(name);
      }
      rargs.push(r.node);
    }
  } else {
    // Spread form: at least k args (so a variadic function needs ≥1 variadic arg — num_nulls() is
    // 42883). The fixed params match their concrete families; every argument from the variadic slot
    // onward matches the variadic element family ("any" ⇒ all).
    if (e.args.length < k) throw noFuncOverload(name);
    for (let i = 0; i < e.args.length; i++) {
      const r = resolve(scope, e.args[i]!, null, ag, params);
      const slot = i < k - 1 ? desc.argFamilies[i]! : varFamily;
      if (!familyMatches(slot, r.type)) throw noFuncOverload(name);
      rargs.push(r.node);
    }
  }

  const result = scalarResultType(desc.result, []);
  return { node: { kind: "variadic", func: name, args: rargs, arrayForm: e.variadic }, type: resolvedTypeOf(result) };
}

// === Polymorphic array-function resolution (spec/design/array-functions.md §2) ======
// The anyarray/anyelement pseudo-families are not real families (argFamily returns null for an
// array), so the generic lookupScalarOverload cannot match an array function. These helpers add the
// unification: one type variable ELEM, bound from an anyarray slot's element type and an anyelement
// slot's type by structural equality, read back into the reserved result codes anyarray (= ELEM[])
// and anyelement (= ELEM).

// isArrayFuncName reports whether name (lowercased) is a polymorphic array function — a
// kind === "function" catalog row whose argFamilies mention anyarray/anyelement. Data-driven.
function isArrayFuncName(name: string): boolean {
  return OPERATORS.some(
    (o) => o.kind === "function" && o.name === name && o.argFamilies.some((f) => f === "anyarray" || f === "anyelement"),
  );
}

// resolvedTypeEqual reports structural equality of two resolved types (the unification check):
// integers/floats by width, arrays recursively by element type, composites by name + field types,
// everything else by kind.
function resolvedTypeEqual(a: ResolvedType, b: ResolvedType): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "int" || a.kind === "float") return a.ty === (b as { ty: ScalarType }).ty;
  if (a.kind === "array") return resolvedTypeEqual(a.elem, (b as { elem: ResolvedType }).elem);
  if (a.kind === "composite") {
    const bc = b as { name: string | null; fields: { name: string; type: ResolvedType }[] };
    if (a.name !== bc.name || a.fields.length !== bc.fields.length) return false;
    return a.fields.every((f, i) => resolvedTypeEqual(f.type, bc.fields[i]!.type));
  }
  return true;
}

// matchPoly matches an overload's slots (which may contain anyarray/anyelement) against the resolved
// argument types, returning { elem, matched }. When matched, elem is null if every polymorphic arg was
// an untyped NULL (ELEM undeterminable). Three passes: anyarray (binds ELEM := the element type),
// anyelement (may precede its binding array — array_prepend), then concrete family slots.
function matchPoly(slots: readonly string[], tys: ResolvedType[]): { elem: ResolvedType | null; matched: boolean } {
  let elem: ResolvedType | null = null;
  const unify = (x: ResolvedType): boolean => {
    if (elem === null) {
      elem = x;
      return true;
    }
    return resolvedTypeEqual(elem, x);
  };
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] === "anyarray") {
      const t = tys[j]!;
      if (t.kind === "array") {
        if (!unify(t.elem)) return { elem: null, matched: false };
      } else if (t.kind !== "null") {
        return { elem: null, matched: false }; // a non-array where anyarray is required
      }
    }
  }
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] === "anyelement" && tys[j]!.kind !== "null") {
      if (!unify(tys[j]!)) return { elem: null, matched: false };
    }
  }
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] !== "anyarray" && slots[j] !== "anyelement" && !familyMatches(slots[j]!, tys[j]!)) {
      return { elem: null, matched: false };
    }
  }
  return { elem, matched: true };
}

// polyResultType is the result ResolvedType of an array function from its catalog result code and the
// bound ELEM: anyarray → ELEM[], anyelement → ELEM (both 42P18 if ELEM is undeterminable); any other
// code is a concrete scalar id (i32, text).
function polyResultType(code: string, elem: ResolvedType | null): ResolvedType {
  if (code === "anyarray") {
    if (elem === null) throw indeterminatePoly();
    return { kind: "array", elem };
  }
  if (code === "anyelement") {
    if (elem === null) throw indeterminatePoly();
    return elem;
  }
  // A concrete array result `<scalar>[]` (array_positions → "i32[]"): the element type is fixed
  // (independent of ELEM), so the result is Array(scalar) (array-functions.md §8).
  if (code.endsWith("[]")) {
    const base = code.slice(0, -2);
    const bty = scalarTypeFromName(base);
    if (bty === undefined) throw new Error("polyResultType: unknown array element " + base);
    return { kind: "array", elem: resolvedTypeOf(bty) };
  }
  const ty = scalarTypeFromName(code);
  if (ty === undefined) throw new Error("polyResultType: unknown result code " + code);
  return resolvedTypeOf(ty);
}

// indeterminatePoly is the 42P18 raised when an array function's polymorphic type cannot be
// determined because every polymorphic argument was an untyped NULL (array_append(NULL, NULL)).
function indeterminatePoly(): EngineError {
  return engineError("indeterminate_datatype", "could not determine polymorphic type because input has type unknown");
}

// elemScalarHint is the element type's ScalarType, for the literal-adaptation hint
// (array-functions.md §2): the bound array element type is threaded back as the ctx when re-resolving
// the polymorphic args, so a bare integer/decimal literal element adapts (with range-checking) to it.
// null for a composite/array/NULL element.
function elemScalarHint(t: ResolvedType): ScalarType | null {
  switch (t.kind) {
    case "int":
    case "float":
      return t.ty;
    case "decimal":
      return "decimal";
    case "text":
      return "text";
    case "bool":
      return "boolean";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    default:
      return null;
  }
}

// resolveArrayFunc resolves a polymorphic array function call (array-functions.md §3): resolve the
// arguments, unify ELEM across the anyarray/anyelement slots to pick the overload (42883 on no match),
// and compute the result type from the matched result code. Two passes (§2): pass 1 resolves the
// arguments with no hint to discover the array's element type; if that element is a scalar, pass 2
// re-resolves the polymorphic-slot arguments with it as the ctx, so an untyped literal element (or an
// ARRAY[…] constructor argument) adapts to the array's element type, with a range check. The kernel id
// is the name; NULL handling lives in the eval kernel.
function resolveArrayFunc(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase() as ArrayFuncName;
  // Each array-function name is single-overload; find its row by (name, arity). A wrong argument count
  // matches no overload (42883), exactly as a missing scalar overload does.
  const desc = OPERATORS.find((o) => o.kind === "function" && o.name === name && o.arity === e.args.length);
  if (!desc) throw noFuncOverload(name);
  const slots = desc.argFamilies;

  const rargs: RExpr[] = [];
  const tys: ResolvedType[] = [];
  for (const a of e.args) {
    const r = resolve(scope, a, null, ag, params);
    rargs.push(r.node);
    tys.push(r.type);
  }
  // Pass 2: adapt the polymorphic args to the array's element type, if it is a scalar. The hint is the
  // element type of the first anyarray argument.
  let hint: ScalarType | null = null;
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] === "anyarray" && tys[j]!.kind === "array") {
      hint = elemScalarHint((tys[j] as { elem: ResolvedType }).elem);
      break;
    }
  }
  if (hint !== null) {
    for (let j = 0; j < slots.length; j++) {
      if (slots[j] === "anyarray" || slots[j] === "anyelement") {
        const r = resolve(scope, e.args[j]!, hint, ag, params);
        rargs[j] = r.node;
        tys[j] = r.type;
      }
    }
  }
  const { elem, matched } = matchPoly(slots, tys);
  if (!matched) throw noFuncOverload(name);
  const type = polyResultType(desc.result, elem);
  return { node: { kind: "arrayFunc", func: name, args: rargs }, type };
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
// for case-insensitive matching), the table, and the flat offset of its first column. A
// qualifierOnly relation is visible ONLY to qualified references — the RETURNING old/new
// row-version pseudo-relations (grammar.md §32): bare-column resolution skips it (no new
// ambiguity), every other statement never builds one.
// `cte` is set (to the CTE list index) when this relation is a reference to a CTE
// (spec/design/cte.md) rather than a base table — its `table` is the binding's synthetic relation
// and exec delivers its rows from the CteCtx. Undefined for a base table / SRF / pseudo-relation.
type ScopeRel = { label: string; table: Table; offset: number; qualifierOnly?: boolean; cte?: number };

// Resolved is how a column reference resolved against the scope CHAIN (spec/design/grammar.md
// §26): level === 0 is a LOCAL column of this query (a flat index into the joined row); level >= 1
// is a correlated OUTER reference to an enclosing query (level hops outward, index the flat column
// index within that ancestor's row).
type Resolved = { level: number; index: number };

// outerOf lifts a parent-scope resolution into the child's frame: one more hop outward.
function outerOf(r: Resolved): Resolved {
  return { level: r.level + 1, index: r.index };
}

class Scope {
  rels: ScopeRel[];
  // parent is the enclosing query's scope, for correlated resolution (null at top level).
  parent: Scope | null;
  // catalog lets a subquery's inner FROM tables be looked up during planning.
  catalog: Database;
  // allowSubquery is true inside a SELECT (and its nested subqueries), false for UPDATE/DELETE
  // (a subquery there is 0A000 this slice).
  allowSubquery: boolean;
  // The statement's CTE bindings visible here (spec/design/cte.md §2). Inherited DIRECTLY down into
  // nested scopes (a subquery sees the same `ctes`), NOT via the `parent` chain — so CTE lookup
  // never counts as a correlation level. Empty for every non-WITH statement.
  ctes: CteBinding[];
  constructor(
    rels: ScopeRel[],
    catalog: Database,
    parent: Scope | null,
    allowSubquery: boolean,
    ctes: CteBinding[] = [],
  ) {
    this.rels = rels;
    this.catalog = catalog;
    this.parent = parent;
    this.allowSubquery = allowSubquery;
    this.ctes = ctes;
  }

  // single builds a one-relation scope with no parent (the single-table UPDATE / DELETE case).
  // Subqueries ARE allowed: a correlated reference resolves to the target row via the per-row
  // outer environment (the subquery's parent is this scope), an uncorrelated one folds once
  // (spec/design/grammar.md §26). SELECT builds its own scope in planSelect.
  static single(catalog: Database, t: Table): Scope {
    return new Scope([{ label: t.name.toLowerCase(), table: t, offset: 0 }], catalog, null, true);
  }

  // empty is the column-less scope a DEFAULT expression resolves against (constraints.md §2): a
  // default may not reference a column (rejected as 0A000 by the structural pre-walk before
  // resolution) and may not contain a subquery, so there are no relations and subqueries are
  // disallowed.
  static empty(catalog: Database): Scope {
    return new Scope([], catalog, null, false);
  }

  // returning is the scope a RETURNING list resolves against (grammar.md §32): the target
  // table at offset 0 (bare and table-qualified references read the BASE row), plus the
  // old/new row-version pseudo-relations as QUALIFIER-ONLY rels over the concatenated
  // projection row [base | other]. baseIsOld says which version the base row is: false for
  // INSERT/UPDATE (base = the new row, `old` reads the other half), true for DELETE (base =
  // the old row, `new` reads the other half) — the absent version is the all-NULL row the
  // caller appends. A target table literally named old/new SHADOWS that qualifier (the
  // pseudo-relation is suppressed; PostgreSQL's probed rule — its WITH (OLD AS o, ...)
  // aliasing escape stays deferred).
  static returning(catalog: Database, t: Table, baseIsOld: boolean): Scope {
    const n = t.columns.length;
    const label = t.name.toLowerCase();
    const oldOffset = baseIsOld ? 0 : n;
    const newOffset = baseIsOld ? n : 0;
    const rels: ScopeRel[] = [{ label, table: t, offset: 0 }];
    for (const pseudo of [
      { label: "old", offset: oldOffset },
      { label: "new", offset: newOffset },
    ]) {
      if (label !== pseudo.label) {
        rels.push({ label: pseudo.label, table: t, offset: pseudo.offset, qualifierOnly: true });
      }
    }
    return new Scope(rels, catalog, null, true);
  }

  // resolveBare resolves a bare column name against THIS scope, then OUTWARD through the parent
  // chain. Within one scope: two+ relations have it → 42702 ambiguous; exactly one → local; none
  // → fall through to the parent. A name found only in an ancestor is an outer reference (nearest
  // scope wins). 42703 only if no scope in the chain has it.
  // A qualifier-only rel (the RETURNING old/new pseudo-relations) is invisible here — no
  // new ambiguity (grammar.md §32).
  resolveBare(name: string): Resolved {
    let found = -1;
    for (const r of this.rels) {
      if (r.qualifierOnly) continue;
      // Count EVERY matching column, not just the first per relation: a synthetic relation (a CTE or
      // derived table) may carry two columns of the same name, and a bare reference to that name is
      // ambiguous (42702) exactly as a match across two relations is (cte.md §2, grammar.md §42).
      // Base tables have unique column names, so this only fires for a duplicate-output-name relation.
      const lower = name.toLowerCase();
      for (let local = 0; local < r.table.columns.length; local++) {
        if (r.table.columns[local]!.name.toLowerCase() === lower) {
          if (found >= 0) throw ambiguousColumn(name);
          found = r.offset + local;
        }
      }
    }
    if (found >= 0) return { level: 0, index: found };
    if (this.parent !== null) return outerOf(this.parent.resolveBare(name));
    throw undefinedColumn(name);
  }

  // resolveQualified resolves a qualified rel.col against THIS scope, then outward. A qualifier
  // naming a relation here binds — a missing column is then 42703 (no fall-through). Only an
  // unknown qualifier walks outward (42P01 if no ancestor has it).
  resolveQualified(qualifier: string, name: string): Resolved {
    const q = qualifier.toLowerCase();
    for (const r of this.rels) {
      if (r.label === q) {
        const local = columnIndex(r.table, name);
        if (local < 0) throw undefinedColumn(name);
        return { level: 0, index: r.offset + local };
      }
    }
    if (this.parent !== null) return outerOf(this.parent.resolveQualified(qualifier, name));
    throw missingFromEntry(qualifier);
  }

  // columnAt returns the column at a flat index in THIS scope (index known valid).
  columnAt(flat: number): Column {
    for (const r of this.rels) {
      const n = r.table.columns.length;
      if (flat >= r.offset && flat < r.offset + n) return r.table.columns[flat - r.offset]!;
    }
    throw new Error("a resolved flat column index is always in range");
  }

  // ancestor returns the scope `level` hops outward (1 = immediate parent).
  ancestor(level: number): Scope {
    let s: Scope = this;
    for (let i = 0; i < level; i++) s = s.parent!;
    return s;
  }

  // columnOf returns the column a resolution refers to — local here, or outer in an ancestor.
  columnOf(r: Resolved): Column {
    return this.ancestor(r.level).columnAt(r.index);
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
  if (isFloat(ty)) return { kind: "float", ty };
  if (isBytea(ty)) return { kind: "bytea" };
  if (isUuid(ty)) return { kind: "uuid" };
  if (isTimestamp(ty)) return { kind: "timestamp" };
  if (isTimestamptz(ty)) return { kind: "timestamptz" };
  if (isDate(ty)) return { kind: "date" };
  if (isInterval(ty)) return { kind: "interval" };
  return { kind: "int", ty };
}

// resolvedTypeOfCol is the resolved (static) type of a column of catalog type `ty` — a scalar via
// resolvedTypeOf, or a composite resolved to a CompositeRType (its name + the resolved field types,
// recursing) against the database's composite-type catalog (spec/design/composite.md §5). The
// composite reference is guaranteed to resolve (CREATE TYPE / the two-pass load validated it).
function resolvedTypeOfCol(ty: Type, db: Database): ResolvedType {
  if (ty.kind === "scalar") return resolvedTypeOf(ty.scalar);
  if (ty.kind === "array") return { kind: "array", elem: resolvedTypeOfCol(ty.elem, db) };
  const def = db.compositeType(ty.name);
  if (def === undefined) throw new Error("composite type reference resolved at load / CREATE TYPE");
  return {
    kind: "composite",
    name: def.name,
    fields: def.fields.map((f) => ({ name: f.name, type: resolvedTypeOfCol(f.type, db) })),
  };
}

// assignableTo reports whether a projected value of type `t` is assignable to a `colTy` column
// for storage — the FAMILY-level gate INSERT ... SELECT applies up front (spec/design/grammar.md
// §24), before any row is produced (so it fires even over an empty source). It is the
// family-level subset of storeValue and MUST agree with it: an integer assigns to an integer or
// decimal column (int→decimal widens), a decimal only to a decimal column (decimal→int is
// explicit-CAST only), text to text/uuid/bytea/timestamp/timestamptz (the documented text
// adaptation — the per-row store then parses, trapping 22P02/22007 on malformed input),
// boolean→boolean, uuid→uuid, bytea→bytea, a timestamp only to a timestamp column and a
// timestamptz only to a timestamptz column (the two never cross — they do not even compare,
// timestamp.md), and a NULL-typed projection to any column (a NOT NULL target then traps 23502
// per row). A non-assignable pair is a 42804.
function assignableTo(t: ResolvedType, colTy: ScalarType): boolean {
  switch (t.kind) {
    case "null":
      return true;
    // A composite source never assigns to a scalar column (the composite-target case is handled
    // structurally at the call site — spec/design/composite.md §4).
    case "composite":
      return false;
    // An array source never assigns to a scalar column (INSERT ... SELECT into an array column is
    // deferred — spec/design/array.md §12).
    case "array":
      return false;
    case "int":
      return isInteger(colTy) || isDecimal(colTy);
    case "decimal":
      return isDecimal(colTy);
    case "float":
      // A float assigns only to a float column, within-family WIDENING only (f32 → f64 is
      // lossless/implicit; f64 → f32 is lossy and needs an explicit CAST — float.md §2/§6).
      // No int/decimal ↔ float storage adaptation (a strict island). storeValue mirrors this.
      return isFloat(colTy) && promoteFloat(t.ty, colTy) === colTy;
    case "bool":
      return isBool(colTy);
    case "text":
      return (
        isText(colTy) ||
        isUuid(colTy) ||
        isBytea(colTy) ||
        isTimestamp(colTy) ||
        isTimestamptz(colTy) ||
        isInterval(colTy) ||
        isDate(colTy)
      );
    case "bytea":
      return isBytea(colTy);
    case "uuid":
      return isUuid(colTy);
    case "timestamp":
      return isTimestamp(colTy);
    case "timestamptz":
      return isTimestamptz(colTy);
    case "date":
      return isDate(colTy);
    case "interval":
      return isInterval(colTy);
  }
}

// rtName is `t`'s type name, for a 42804 assignability message (the integer width is exact).
// typeNames renders a projection's resolved types as their canonical names for the public
// Outcome columnTypes — the `# types:` directive's assertion surface (spec/design/conformance.md
// §7). Same names as the 42804 message (rtName): the exact integer width, the unconstrained
// "decimal".
function typeNames(ts: ResolvedType[]): string[] {
  return ts.map(rtName);
}

function rtName(t: ResolvedType): string {
  switch (t.kind) {
    case "int":
      return canonicalName(t.ty);
    case "float":
      return canonicalName(t.ty);
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "decimal":
      return "decimal";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    case "composite":
      // A named composite is its type name; an anonymous ROW(...) is `record` (PG).
      return t.name ?? "record";
    case "array":
      return rtName(t.elem) + "[]";
    case "null":
      return "unknown";
  }
}

// cteSyntheticTable builds the synthetic relation a CTE reference resolves against
// (spec/design/cte.md §2): one column per body output, named by the rename list (a count mismatch
// with MORE aliases is 42P10) or the body's own output names, typed from the planned body. The
// relation has no primary key / constraints — it is read-only and its rows come from the CTE
// context, never a store.
function cteSyntheticTable(name: string, plan: QueryPlan, rename: string[] | null): Table {
  const bodyTypes = plan.columnTypes;
  const bodyNames = plan.columnNames;
  let colNames: string[];
  if (rename !== null) {
    // PostgreSQL allows FEWER aliases than the body has columns — the first `rename.length` columns
    // take the aliases, the rest keep their body output names (a partial rename). Only MORE aliases
    // than columns is an error (42P10).
    if (rename.length > bodyTypes.length) {
      throw engineError(
        "invalid_column_reference",
        `WITH query "${name}" has ${bodyTypes.length} columns available but ${rename.length} columns specified`,
      );
    }
    colNames = bodyTypes.map((_t, i) => rename[i] ?? bodyNames[i]!);
  } else {
    colNames = bodyNames.slice();
  }
  const columns: Column[] = colNames.map((n, i) => ({
    name: n,
    type: typeFromResolved(bodyTypes[i]!),
    decimal: null,
    primaryKey: false,
    notNull: false,
    default: null,
    defaultExpr: null,
  }));
  return { name, columns, pk: [], checks: [], indexes: [], fks: [] };
}

// typeFromResolved is the catalog Type that round-trips a column's ResolvedType — used to give a
// CTE's synthetic columns a Type (spec/design/cte.md). An untyped NULL column maps to text
// (PostgreSQL's unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only
// CTE column (values flow through unchanged), so it is dropped. An anonymous ROW(...) composite has
// no catalog type to name — deferred (0A000), a corner not reached by the corpus.
function typeFromResolved(rt: ResolvedType): Type {
  switch (rt.kind) {
    case "int":
    case "float":
      return scalarT(rt.ty);
    case "bool":
      return scalarT("boolean");
    case "text":
    case "null":
      return scalarT("text");
    case "decimal":
      return scalarT("decimal");
    case "bytea":
      return scalarT("bytea");
    case "uuid":
      return scalarT("uuid");
    case "timestamp":
      return scalarT("timestamp");
    case "timestamptz":
      return scalarT("timestamptz");
    case "date":
      return scalarT("date");
    case "interval":
      return scalarT("interval");
    case "composite":
      if (rt.name !== null) return compositeT(rt.name);
      throw engineError("feature_not_supported", "an anonymous composite column in a CTE is not supported yet");
    case "array":
      return arrayT(typeFromResolved(rt.elem));
  }
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

// stmtIsWrite reports whether a statement mutates the database (so autocommit must capture +
// durably persist it). Reads (SELECT, set operations) run with no transaction (transactions.md
// §4.1) — UNLESS the read-shaped statement calls a sequence-mutating function (nextval), which
// makes it a write (spec/design/sequences.md §4).
export function stmtIsWrite(stmt: Statement): boolean {
  return (
    stmt.kind === "createTable" ||
    stmt.kind === "dropTable" ||
    stmt.kind === "createIndex" ||
    stmt.kind === "dropIndex" ||
    stmt.kind === "createType" ||
    stmt.kind === "dropType" ||
    stmt.kind === "createSequence" ||
    stmt.kind === "alterSequence" ||
    stmt.kind === "dropSequence" ||
    stmt.kind === "insert" ||
    stmt.kind === "update" ||
    stmt.kind === "delete" ||
    // A read-shaped statement that calls nextval/setval IS a write (sequences.md §4): it must take
    // the write gate, stage the advance, and commit (autocommit) — and is 25006 in a READ ONLY
    // transaction, exactly like any other write.
    stmtCallsSeqMutator(stmt)
  );
}

// stmtCallsSeqMutator reports whether stmt's expression trees contain a sequence-MUTATING function
// call (nextval; in S2, setval) anywhere — which makes an otherwise read-shaped statement a write
// (sequences.md §4). Only the read-shaped statements need checking: INSERT/UPDATE/DELETE/DDL are
// already writes (stmtIsWrite short-circuits before this), and an INSERT VALUES slot is literal-only
// (no function call). currval is a pure read and is NOT counted.
function stmtCallsSeqMutator(stmt: Statement): boolean {
  switch (stmt.kind) {
    case "select":
      return selectCallsSeqMutator(stmt);
    case "setOp":
      return setOpCallsSeqMutator(stmt);
    case "with":
      return stmt.ctes.some((c) => queryCallsSeqMutator(c.query)) || queryCallsSeqMutator(stmt.body);
    default:
      return false;
  }
}

function queryCallsSeqMutator(qe: QueryExpr): boolean {
  return qe.kind === "setOp" ? setOpCallsSeqMutator(qe) : selectCallsSeqMutator(qe);
}

function setOpCallsSeqMutator(so: SetOp): boolean {
  return queryCallsSeqMutator(so.lhs) || queryCallsSeqMutator(so.rhs);
}

function selectCallsSeqMutator(s: Select): boolean {
  const itemCalls =
    s.items.kind === "list" && s.items.items.some((i) => exprCallsSeqMutator(i.expr));
  return (
    itemCalls ||
    (s.from !== null && tableRefCallsSeqMutator(s.from)) ||
    s.joins.some(
      (j) => tableRefCallsSeqMutator(j.table) || (j.on !== null && exprCallsSeqMutator(j.on)),
    ) ||
    (s.filter !== null && exprCallsSeqMutator(s.filter)) ||
    s.groupBy.some(exprCallsSeqMutator) ||
    (s.having !== null && exprCallsSeqMutator(s.having))
  );
}

function tableRefCallsSeqMutator(t: TableRef): boolean {
  return (
    (t.args !== null && t.args.some(exprCallsSeqMutator)) ||
    (t.subquery !== undefined && queryCallsSeqMutator(t.subquery)) ||
    (t.values !== undefined && t.values.some((row) => row.some(exprCallsSeqMutator)))
  );
}

// exprCallsSeqMutator is exhaustive over Expr (every kind is matched): true iff the tree contains a
// sequence-mutating call (nextval or setval).
function exprCallsSeqMutator(e: Expr): boolean {
  switch (e.kind) {
    case "funcCall": {
      const n = e.name.toLowerCase();
      return n === "nextval" || n === "setval" || e.args.some(exprCallsSeqMutator);
    }
    case "column":
    case "qualifiedColumn":
    case "literal":
    case "typedLiteral":
    case "param":
      return false;
    case "row":
      return e.fields.some(exprCallsSeqMutator);
    case "array":
      return e.elements.some(exprCallsSeqMutator);
    case "fieldAccess":
    case "fieldStar":
      return exprCallsSeqMutator(e.base);
    case "subscript":
      return (
        exprCallsSeqMutator(e.base) ||
        e.subscripts.some((s) =>
          s.isSlice
            ? (s.lower !== null && exprCallsSeqMutator(s.lower)) ||
              (s.upper !== null && exprCallsSeqMutator(s.upper))
            : exprCallsSeqMutator(s.index),
        )
      );
    case "cast":
      return exprCallsSeqMutator(e.inner);
    case "unary":
    case "isNull":
      return exprCallsSeqMutator(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.rhs);
    case "in":
      return exprCallsSeqMutator(e.lhs) || e.list.some(exprCallsSeqMutator);
    case "between":
      return (
        exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.lo) || exprCallsSeqMutator(e.hi)
      );
    case "case":
      return (
        (e.operand !== null && exprCallsSeqMutator(e.operand)) ||
        e.whens.some((w) => exprCallsSeqMutator(w.cond) || exprCallsSeqMutator(w.result)) ||
        (e.els !== null && exprCallsSeqMutator(e.els))
      );
    case "scalarSubquery":
    case "exists":
      return queryCallsSeqMutator(e.query);
    case "inSubquery":
    case "quantifiedSubquery":
      return exprCallsSeqMutator(e.lhs) || queryCallsSeqMutator(e.query);
    case "quantified":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.array);
  }
}

// stmtKind is a short label for a statement kind, for the 25006 read-only-violation message (the
// message text is informational — never matched; spec/design/conformance.md §2).
function stmtKind(stmt: Statement): string {
  switch (stmt.kind) {
    case "createTable":
      return "CREATE TABLE";
    case "dropTable":
      return "DROP TABLE";
    case "createIndex":
      return "CREATE INDEX";
    case "dropIndex":
      return "DROP INDEX";
    case "createType":
      return "CREATE TYPE";
    case "dropType":
      return "DROP TYPE";
    case "createSequence":
      return "CREATE SEQUENCE";
    case "alterSequence":
      return "ALTER SEQUENCE";
    case "dropSequence":
      return "DROP SEQUENCE";
    case "insert":
      return "INSERT";
    case "update":
      return "UPDATE";
    case "delete":
      return "DELETE";
    default:
      return "statement";
  }
}

// cloneStores captures the committed stores cheaply for rollback-on-error: each store is an O(1)
// persistent-map clone (the catalog map of Table objects is shallow-copied by the caller, since
// Table objects are never mutated in place — only added/removed).
function cloneStores(stores: Map<string, TableStore>): Map<string, TableStore> {
  const out = new Map<string, TableStore>();
  for (const [k, s] of stores) out.set(k, s.clone());
  return out;
}

// dmlOutcome wraps a DML statement's completion: a query result projecting the returned rows
// when a RETURNING clause was resolved (retNames non-null — grammar.md §32; zero affected
// rows is an EMPTY query result, never a bare statement), else a bare statement result
// carrying the affected-row count (spec/design/api.md §4).
function dmlOutcome(
  retNames: string[] | null,
  retTypes: string[] | null,
  returned: Value[][] | null,
  affected: number,
  cost: bigint,
): Outcome {
  if (retNames !== null) {
    return { kind: "query", columnNames: retNames, columnTypes: retTypes ?? [], rows: returned ?? [], cost };
  }
  return { kind: "statement", cost, rowsAffected: affected };
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
): { nodes: RExpr[]; names: string[]; types: ResolvedType[] } {
  if (items.kind === "all") {
    // `*` with nothing to expand — a FROM-less SELECT — is PostgreSQL's exact error
    // (grammar.md §34). Qualifier-only rels don't count: they are RETURNING's old/new
    // pseudo-relations, and that scope always also carries the real relation.
    if (scope.rels.every((r) => r.qualifierOnly)) {
      throw engineError("syntax_error", "SELECT * with no tables specified is not valid");
    }
    const nodes: RExpr[] = [];
    const names: string[] = [];
    const types: ResolvedType[] = [];
    // The RETURNING old/new pseudo-relations are qualifier-only: `*` expands the real
    // relations' columns exactly as before (grammar.md §32).
    for (const r of scope.rels) {
      if (r.qualifierOnly) continue;
      r.table.columns.forEach((c, i) => {
        nodes.push({ kind: "column", index: r.offset + i });
        names.push(c.name);
        types.push(resolvedTypeOfCol(c.type, scope.catalog));
      });
    }
    return { nodes, names, types };
  }
  const nodes: RExpr[] = [];
  const names: string[] = [];
  const types: ResolvedType[] = [];
  for (const it of items.items) {
    // `(expr).*` expands a composite base into one output column per field, in declaration order
    // (spec/design/composite.md §S4). The base AST is re-resolved per field (Expr is plain data,
    // resolution is pure) — deterministic. A non-composite base is 42809.
    if (it.expr.kind === "fieldStar") {
      const base = it.expr.base;
      const { type: baseType } = resolve(scope, base, null, ag, params);
      if (baseType.kind !== "composite") {
        throw engineError(
          "wrong_object_type",
          "column notation .* applied to type " + rtName(baseType) + ", which is not a composite type",
        );
      }
      baseType.fields.forEach((f, i) => {
        const { node: bn } = resolve(scope, base, null, ag, params);
        nodes.push({ kind: "field", base: bn, index: i });
        names.push(f.name);
        types.push(f.type);
      });
      continue;
    }
    const { node, type } = resolve(scope, it.expr, null, ag, params);
    nodes.push(node);
    types.push(type);
    names.push(it.alias ?? outputName(scope, it.expr));
  }
  return { nodes, names, types };
}

// outputName is the output column name of an un-aliased select item (grammar.md §8/§15): a
// bare or qualified column reference takes the catalog's canonical name (never the qualifier,
// never the SELECT spelling); every other expression takes the fixed "?column?". The column is
// known to exist — resolve validated it.
function outputName(scope: Scope, e: Expr): string {
  // A bare/qualified column takes the catalog's canonical name, whether it resolves to a local
  // relation or (correlated) an enclosing one — columnOf handles both. A qualifier that names no
  // relation (the column.field ambiguity fallback) takes the written name (PG; matching Rust).
  if (e.kind === "column") {
    try {
      return scope.columnOf(scope.resolveBare(e.name)).name;
    } catch {
      return e.name;
    }
  }
  if (e.kind === "qualifiedColumn") {
    try {
      return scope.columnOf(scope.resolveQualified(e.qualifier, e.name)).name;
    } catch {
      return e.name;
    }
  }
  // An un-aliased aggregate call is named by its lowercased function name (PG; §8). A field
  // selection takes the FIELD name lowercased (PG names the output column after the field).
  if (e.kind === "funcCall") return e.name.toLowerCase();
  if (e.kind === "fieldAccess") return e.field.toLowerCase();
  // A subscript takes the base array's name (PG names `a[1]` after `a`); `a[1][2]` recurses to the
  // same base. A non-column base falls through to `?column?`.
  if (e.kind === "subscript") return outputName(scope, e.base);
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

// resolveColumnRef turns a chain resolution into a resolved node + type (§26). A Local column
// obeys the grouping rule (collectColumn); an Outer (correlated) reference is a per-outer-row
// CONSTANT, so it bypasses that rule and resolves to an outerColumn reading the enclosing row at
// eval; its type is the ancestor column's.
function resolveColumnRef(
  scope: Scope,
  ag: AggCtx,
  r: Resolved,
  name: string,
): { node: RExpr; type: ResolvedType } {
  if (r.level === 0) return collectColumn(scope, ag, r.index, name);
  return { node: { kind: "outerColumn", level: r.level, index: r.index }, type: resolvedTypeOfCol(scope.columnOf(r).type, scope.catalog) };
}

// resolveFieldOf resolves a composite field selection `base.field` (spec/design/composite.md §S4)
// given the already-resolved `base` node and its static type: `base` must be composite — else 42809
// (wrong_object_type, PG's "column notation applied to non-composite") — and `field` must name one
// of its fields case-insensitively (PG folds the identifier), else 42703 (undefined_column). Returns
// the `field` RExpr node carrying the fixed field ordinal, plus the field's static type.
function resolveFieldOf(
  baseNode: RExpr,
  baseType: ResolvedType,
  field: string,
): { node: RExpr; type: ResolvedType } {
  if (baseType.kind !== "composite") {
    throw engineError(
      "wrong_object_type",
      "column notation ." + field + " applied to type " + rtName(baseType) + ", which is not a composite type",
    );
  }
  const lower = field.toLowerCase();
  const idx = baseType.fields.findIndex((f) => f.name.toLowerCase() === lower);
  if (idx < 0) throw undefinedColumn(field);
  return { node: { kind: "field", base: baseNode, index: idx }, type: baseType.fields[idx]!.type };
}

// planSubquery plans a subquery operand against the scope chain (§26). Rejects a non-SELECT context
// (UPDATE/DELETE/INSERT — allowSubquery false) with 0A000. A $N inside the subquery is allowed: the
// shared params table is threaded into the inner plan, so a parameter typed by an inner context
// (WHERE inner.col = $1) infers statement-wide and unifies with any outer use of the same $N. A
// parameter with NO type context anywhere stays uninferred and finalize raises 42P18 (a documented
// divergence from PostgreSQL, which defaults such a $N to text — grammar.md §26). The inner query is
// resolved ONCE, with `scope` as its parent, so correlated references become outerColumn and errors
// fire even over an empty outer.
function planSubquery(scope: Scope, inner: QueryExpr, params: ParamTypes): QueryPlan {
  if (!scope.allowSubquery) {
    throw engineError("feature_not_supported", "subqueries are only supported in a SELECT statement");
  }
  // The subquery inherits the enclosing scope's CTE bindings directly (cte.md §2) — visible at any
  // nesting depth without counting as a correlation level.
  return scope.catalog.planQuery(inner, scope, scope.ctes, params);
}

// resolve resolves one Expr into an RExpr plus its static type. ctx (non-null) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); null
// defaults a bare literal to i64.
function resolve(
  scope: Scope,
  e: Expr,
  ctx: ScalarType | null,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  switch (e.kind) {
    case "row": {
      // A ROW(...) constructor (spec/design/composite.md §1): resolve each field with no type
      // context (its natural type), producing an ANONYMOUS composite (name = null, fields named
      // f1, f2, … per PG). Storing it into a named composite column matches structurally
      // (assignability at the store site coerces each field to the target's declared type).
      const nodes: RExpr[] = [];
      const fields: { name: string; type: ResolvedType }[] = [];
      for (let i = 0; i < e.fields.length; i++) {
        const { node, type } = resolve(scope, e.fields[i]!, null, ag, params);
        nodes.push(node);
        fields.push({ name: "f" + (i + 1), type });
      }
      return { node: { kind: "row", fields: nodes }, type: { kind: "composite", name: null, fields } };
    }
    case "array": {
      // An ARRAY[...] constructor (spec/design/array.md §1): resolve each element (natural type),
      // unify to a common element type, build an array node. A bare empty ARRAY[] has no element
      // type to infer — use '{}'::T[] instead (the cast supplies it).
      if (e.elements.length === 0) {
        throw typeError("cannot determine the element type of an empty ARRAY[]; write '{}'::T[]");
      }
      // An element-type hint (ctx) flows down to the elements so an array literal adapts its untyped
      // integer/decimal literals exactly as a scalar literal does — e.g. resolving ARRAY[7,8] with an
      // i32 context yields i32[], not the default i64[] (the polymorphic array functions pass the
      // bound element type here, array-functions.md §2). Almost every other caller passes null, so the
      // default 1-D unification is unchanged.
      const nodes: RExpr[] = [];
      const elemTypes: ResolvedType[] = [];
      for (const el of e.elements) {
        const { node, type } = resolve(scope, el, ctx, ag, params);
        nodes.push(node);
        elemTypes.push(type);
      }
      // If the items are themselves arrays, this is a nested (multidim-stacking) constructor and the
      // result type is the SAME array type (dimension-agnostic, §2/§4); otherwise a flat 1-D array.
      const common = unifyArrayElementTypes(elemTypes);
      if (common.kind === "array") {
        return { node: { kind: "array", elements: nodes, nested: true }, type: common };
      }
      return { node: { kind: "array", elements: nodes, nested: false }, type: { kind: "array", elem: common } };
    }
    case "column": {
      // Resolve against the scope CHAIN (§26). A Local match obeys the grouping rule; an Outer
      // (correlated) match is a per-outer-row constant exempt from it (resolveColumnRef).
      return resolveColumnRef(scope, ag, scope.resolveBare(e.name), e.name);
    }
    case "qualifiedColumn": {
      // A bare `rel.col` resolves STRICTLY against the FROM relations — `qualifier` MUST name a
      // relation (else 42P01), matching PostgreSQL. Composite field access on a column is the
      // PARENS-REQUIRED `(col).field` form (spec/design/composite.md §1/§S4), a fieldAccess node,
      // never this bare qualified-column path (PG raises 42P01 for the unparenthesized `col.field` /
      // `t.col.field` spellings).
      return resolveColumnRef(scope, ag, scope.resolveQualified(e.qualifier, e.name), e.name);
    }
    case "fieldAccess": {
      // `(expr).field` — composite field selection (spec/design/composite.md §S4).
      const { node, type } = resolve(scope, e.base, null, ag, params);
      return resolveFieldOf(node, type, e.field);
    }
    case "fieldStar":
      // `(expr).*` — whole-row expansion is a projection-list construct only; in a scalar
      // expression position it is unsupported (PG rejects row expansion here — 0A000).
      throw engineError("feature_not_supported", "row expansion (.*) is not supported in this context");
    case "subscript": {
      // `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array (else
      // 42804). Each subscript bound is an integer (a literal adapts; a non-integer is 42804). If any
      // spec is a slice the result is the array type (a sub-array); otherwise the element type. OOB /
      // NULL → NULL is an evaluation-time rule, not a resolve error.
      const base = resolve(scope, e.base, null, ag, params);
      if (base.type.kind !== "array") {
        throw typeError(`cannot subscript a value of type ${rtName(base.type)}, which is not an array`);
      }
      const resolveBound = (b: Expr): RExpr => {
        const r = resolve(scope, b, "i32", ag, params);
        if (r.type.kind !== "int" && r.type.kind !== "null") {
          throw typeError(`array subscript must be an integer, not ${rtName(r.type)}`);
        }
        return r.node;
      };
      let isSlice = false;
      const rsubs: RSubscript[] = e.subscripts.map((s) => {
        if (s.isSlice) {
          isSlice = true;
          return {
            isSlice: true,
            lower: s.lower === null ? null : resolveBound(s.lower),
            upper: s.upper === null ? null : resolveBound(s.upper),
          };
        }
        return { isSlice: false, index: resolveBound(s.index) };
      });
      // A slice yields a sub-array (the array type); all-index access yields an element.
      const type = isSlice ? base.type : base.type.elem;
      return { node: { kind: "subscript", base: base.node, subscripts: rsubs, isSlice }, type };
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
      return resolveFuncCall(scope, e, ag, params);
    case "typedLiteral": {
      // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
      // `type 'string'`, equal to CAST('string' AS type) over a string-literal operand. Resolve the
      // type by name (unknown → 42704) and coerce the string to it at resolve, context-free. No
      // typmod rides on the literal (the parser's one-token lookahead admits none).
      // A composite type name (`addr '(Main,90210)'`) coerces the string via record_in
      // (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
      const ct = scope.catalog.compositeType(e.typeName);
      if (ct !== undefined) return coerceStringToComposite(e.text, ct, scope.catalog);
      const [target] = resolveTypeAndTypmod(e.typeName, null);
      return coerceStringLiteral(e.text, target, null);
    }
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
          // A string literal is text by default (collation C). It adapts to a BYTEA context
          // (decode the hex input, 22P02 on bad hex) or a TIMESTAMP/TIMESTAMPTZ context (parse
          // the datetime, 22007/22008 — spec/design/timestamp.md). Any other context keeps it text.
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
          if (ctx !== null && isTimestamp(ctx)) {
            return {
              node: { kind: "constTimestamp", value: parseTimestamp(e.literal.text) },
              type: { kind: "timestamp" },
            };
          }
          if (ctx !== null && isTimestamptz(ctx)) {
            return {
              node: { kind: "constTimestamptz", value: parseTimestamptz(e.literal.text) },
              type: { kind: "timestamptz" },
            };
          }
          if (ctx !== null && isDate(ctx)) {
            // A string adapts to a DATE context (the ISO date, dropping any time/offset; date.md §2).
            return {
              node: { kind: "constDate", value: parseDate(e.literal.text) },
              type: { kind: "date" },
            };
          }
          if (ctx !== null && isInterval(ctx)) {
            // A string adapts to an INTERVAL context (parse the "unit + time" subset,
            // 22007/22008 — spec/design/interval.md), like timestamp adaptation.
            return {
              node: { kind: "constInterval", value: parseInterval(e.literal.text) },
              type: { kind: "interval" },
            };
          }
          return { node: { kind: "constText", value: e.literal.text }, type: { kind: "text" } };
        }
        case "decimal":
          // A decimal literal adapts to a FLOAT context (float.md §4): decimal → float at resolve
          // (the nearest binary64 to the exact decimal; Math.fround if the context is f32). The
          // exact-decimal string already round-trips IEEE conversion via Number(...). Otherwise it
          // is decimal — cap-checked here (an over-long coefficient/scale traps 22003 at resolve).
          if (ctx !== null && isFloat(ctx)) {
            return floatFromDecimalLiteral(e.literal.dec, ctx);
          }
          return { node: { kind: "constDecimal", value: e.literal.dec.checkCap() }, type: { kind: "decimal" } };
        default: {
          // An integer literal adapts to an integer context or — like a decimal literal — a FLOAT
          // context (int → float at resolve; float.md §4). A non-numeric context (a text/decimal
          // column or assignment target) does not apply — it defaults to i64, and the surrounding
          // check then reports the family mismatch (42804) or widens it (int→decimal), never a wrong
          // range check.
          if (ctx !== null && isFloat(ctx)) {
            const n = roundToWidth(ctx, Number(e.literal.int));
            return { node: { kind: "constFloat", ty: ctx, value: n }, type: { kind: "float", ty: ctx } };
          }
          const ty = ctx !== null && isInteger(ctx) ? ctx : "i64";
          if (!inRange(ty, e.literal.int)) throw overflow(ty);
          return { node: { kind: "constInt", value: e.literal.int }, type: { kind: "int", ty } };
        }
      }
    case "scalarSubquery": {
      // A subquery in expression position (§26): PLANNED ONCE against the scope chain here, so its
      // column-count / type errors fire even over an empty outer. planSubquery rejects a non-SELECT
      // context and a $N inside (both 0A000). The fold pass folds an uncorrelated one to a constant;
      // a correlated one is re-executed per outer row by the evaluator.
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery must return only one column");
      }
      return {
        node: { kind: "subquery", plan, subKind: "scalar", lhs: null, negated: false },
        type: plan.columnTypes[0]!,
      };
    }
    case "exists": {
      // EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT EXISTS
      // parses as the unary NOT wrapping this, so negated here is always false.
      const plan = planSubquery(scope, e.query, params);
      return {
        node: { kind: "subquery", plan, subKind: "exists", lhs: null, negated: false },
        type: { kind: "bool" },
      };
    }
    case "inSubquery": {
      // The LHS is an OUTER expression (resolved in the current scope / agg context); the subquery
      // yields the single membership column. The test is `lhs = element`, so the pair must be
      // comparable (42804), exactly like a literal IN.
      const { node: lhs, type: lt } = resolve(scope, e.lhs, null, ag, params);
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery has too many columns");
      }
      classifyComparable(lt, plan.columnTypes[0]!);
      return {
        node: { kind: "subquery", plan, subKind: "in", lhs, negated: e.negated },
        type: { kind: "bool" },
      };
    }
    case "quantifiedSubquery": {
      // The subquery spelling of the quantifier (array-functions.md §11.6) — the IN-subquery
      // pattern with the comparison + 3VL fold. Resolve the outer lhs, plan the body, require ONE
      // column (42601), and require comparability — reporting operator-not-found (42883) the way the
      // array quantifier does (§11.3), not the plain 42804. No 21000 cardinality limit.
      const { node: lhs, type: lt } = resolve(scope, e.lhs, null, ag, params);
      const plan = planSubquery(scope, e.query, params);
      if (plan.columnTypes.length !== 1) {
        throw engineError("syntax_error", "subquery has too many columns");
      }
      try {
        classifyComparable(lt, plan.columnTypes[0]!);
      } catch {
        throw engineError(
          "undefined_function",
          `operator does not exist: ${rtName(lt)} ${binaryOpSymbol(e.op)} ${rtName(plan.columnTypes[0]!)}`,
        );
      }
      return {
        node: { kind: "subquery", plan, subKind: "quantified", lhs, negated: false, op: e.op, all: e.all },
        type: { kind: "bool" },
      };
    }
    case "cast": {
      // An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the string-literal
      // form `'{…}'::T[]` and a bare NULL; every other array cast (runtime text→array, array→text,
      // element-wise array→array) is a documented 0A000 narrowing.
      if (e.typeName.endsWith("[]")) {
        const base = e.typeName.slice(0, -2);
        if (e.typeMod !== null) {
          throw engineError("feature_not_supported", "a type modifier on an array type is not supported yet");
        }
        const elemScalar = scalarTypeFromName(base);
        const baseComposite = scope.catalog.compositeType(base);
        let elemCol: ColType;
        let elemRt: ResolvedType;
        if (elemScalar !== undefined) {
          elemCol = { kind: "scalar", scalar: elemScalar };
          elemRt = resolvedTypeOf(elemScalar);
        } else if (baseComposite !== undefined) {
          const elemTy = compositeT(baseComposite.name);
          elemCol = scope.catalog.colTypeOf(elemTy);
          elemRt = resolvedTypeOfCol(elemTy, scope.catalog);
        } else {
          throw engineError("undefined_object", "type does not exist: " + base);
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
          const val = coerceStringToArray(e.inner.literal.text, elemCol);
          return { node: valueToRExpr(val), type: { kind: "array", elem: elemRt } };
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "null") {
          return { node: { kind: "constNull" }, type: { kind: "array", elem: elemRt } };
        }
        throw engineError("feature_not_supported", "casting to an array type is only supported from a string literal this slice");
      }
      // A composite cast target (`'(…)'::addr`) — a CREATE TYPE name, not a built-in scalar
      // (spec/design/composite.md §8). A STRING LITERAL operand coerces via record_in (the
      // `'(…)'::addr` headline); a bare NULL adapts to the composite; a same-named composite operand
      // is the identity. Every other operand (a runtime text expression, an anonymous `ROW(…)`) is a
      // documented 0A000 narrowing this slice — relaxable. A type modifier on a composite is
      // meaningless (0A000).
      const ct = scope.catalog.compositeType(e.typeName);
      if (ct !== undefined) {
        if (e.typeMod !== null) {
          throw engineError("feature_not_supported", "a type modifier is not supported on a composite type");
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
          return coerceStringToComposite(e.inner.literal.text, ct, scope.catalog);
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        if (inner.type.kind === "null") {
          return { node: inner.node, type: resolvedTypeOfCol({ kind: "composite", name: ct.name }, scope.catalog) };
        }
        // An identical named composite is the identity cast.
        if (inner.type.kind === "composite" && inner.type.name?.toLowerCase() === ct.name.toLowerCase()) {
          return inner;
        }
        throw engineError("feature_not_supported", "casting to a composite type is only supported from a string literal");
      }
      const [target, typmod] = resolveTypeAndTypmod(e.typeName, e.typeMod);
      // A string LITERAL operand is coerced to the target at resolve — CAST('42' AS int), the same
      // primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5). The ONLY
      // text→T cast admitted ahead of the general cast slice; a non-literal text operand still
      // falls through to the deferred 0A000 below.
      if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
        return coerceStringLiteral(e.inner.literal.text, target, typmod);
      }
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
      // timestamp casts are deferred (spec/design/timestamp.md §6): casting TO a datetime is 0A000.
      if (isTimestamp(target) || isTimestamptz(target)) {
        throw engineError("feature_not_supported", "casting to a timestamp type is not supported yet");
      }
      // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000.
      if (isInterval(target)) {
        throw engineError("feature_not_supported", "casting to an interval type is not supported yet");
      }
      // date casts are deferred (spec/design/date.md §5/§6): casting TO date is 0A000.
      if (isDate(target)) {
        throw engineError("feature_not_supported", "casting to a date type is not supported yet");
      }
      // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int` (and
      // `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
      // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
      // context (its value is range-checked / coerced against target at eval), so changing the
      // context only for a parameter leaves all existing CAST behavior untouched.
      const innerCtx = e.inner.kind === "param" ? target : null;
      const inner = resolve(scope, e.inner, innerCtx, ag, params);
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
      // Casting FROM a timestamp is likewise deferred (0A000).
      if (inner.type.kind === "timestamp" || inner.type.kind === "timestamptz") {
        throw engineError("feature_not_supported", "casting from a timestamp type is not supported yet");
      }
      // Casting FROM an interval is likewise deferred (0A000).
      if (inner.type.kind === "interval") {
        throw engineError("feature_not_supported", "casting from an interval type is not supported yet");
      }
      // Casting FROM a date is likewise deferred (0A000; date↔timestamp unblocks the cross-family comparison — date.md §4/§6).
      if (inner.type.kind === "date") {
        throw engineError("feature_not_supported", "casting from a date type is not supported yet");
      }
      // Casting FROM an array (array→text, element-wise array→array) is deferred (array.md §7/§12).
      if (inner.type.kind === "array") {
        throw engineError("feature_not_supported", "casting an array value is not supported yet");
      }
      // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
      // decimal→decimal (re-scale), the float casts (int↔float, decimal↔float, float↔float — all
      // explicit, float.md §6), and NULL are all castable. The CAST matrix (casts.toml) is strict:
      // these are exactly the legal (from, to) pairs across the int/decimal/float families.
      const resultType: ResolvedType = isDecimal(target)
        ? { kind: "decimal" }
        : isFloat(target)
          ? { kind: "float", ty: target }
          : { kind: "int", ty: target };
      return { node: { kind: "cast", target, typmod, operand: inner.node }, type: resultType };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(scope, e.operand, ctx, ag, params);
        if (type.kind === "decimal") {
          return { node: { kind: "neg", result: "decimal", operand: node }, type: { kind: "decimal" } };
        }
        if (type.kind === "float") {
          // Unary minus on a float flips the sign bit (no overflow); a NaN/Inf operand passes
          // through per IEEE (-NaN is NaN, -Inf is -Inf) — float.md §5. result keeps the width.
          return { node: { kind: "neg", result: type.ty, operand: node }, type: { kind: "float", ty: type.ty } };
        }
        if (type.kind === "interval") {
          // -interval (spec/design/interval.md §5).
          return { node: { kind: "neg", result: "interval", operand: node }, type: { kind: "interval" } };
        }
        let result: ScalarType;
        if (type.kind === "int") result = type.ty;
        else if (type.kind === "null") result = "i64"; // -NULL = NULL
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
    case "quantified":
      return resolveQuantified(scope, e.op, e.all, e.lhs, e.array, ag, params);
    case "in": {
      // An EMPTY list reaches here only from folding an IN-subquery whose result was empty
      // (grammar.md §26; the parser rejects literal `IN ()` → 42601). The value is a constant —
      // `x IN (empty)` = FALSE, `x NOT IN (empty)` = TRUE — for every x including NULL. Still
      // resolve the LHS so an undefined column / aggregate-context error fires, then return the
      // constant (a leaf — no operator_eval, cost.md §3).
      if (e.list.length === 0) {
        resolve(scope, e.lhs, null, ag, params);
        return { node: { kind: "constBool", value: e.negated }, type: { kind: "bool" } };
      }
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
      // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Checked
      // before the ±-only temporal rule below.
      const scaled = intervalScaleResult(op, p.lt.kind, p.rt.kind);
      if (scaled !== undefined) {
        return { node: { kind: "arith", op, result: scaled, lhs: p.rl, rhs: p.rr }, type: resolvedTypeOf(scaled) };
      }
      // Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz] ±
      // interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval. The
      // eval dispatches on the value kinds; here we settle the result type. A temporal operand in
      // any other combination is a 42804.
      const temporal = temporalArithResult(op, p.lt.kind, p.rt.kind);
      if (temporal !== undefined) {
        return { node: { kind: "arith", op, result: temporal, lhs: p.rl, rhs: p.rr }, type: resolvedTypeOf(temporal) };
      }
      // Float arithmetic (float.md §5): float ⊕ float → float for + - * / % (and unary - via the
      // neg path). A mixed-width pair PROMOTES to f64 (the higher rank), so the computation is
      // always at one width. NO cross-family promotion — int/decimal ⊕ float is 42804 (a float
      // operand with a non-float, non-null sibling falls through to requireNumericOperand, which
      // does NOT accept float, raising the type error). A float literal sibling already adapted via
      // ctxOf, so a literal+float pair is float×float here.
      if (p.lt.kind === "float" || p.rt.kind === "float") {
        if (p.lt.kind !== "float" || p.rt.kind !== "float") {
          throw typeError("arithmetic operators require operands of the same family");
        }
        const result = promoteFloat(p.lt.ty, p.rt.ty);
        const lhsW = widenFloatTo(p.rl, p.lt.ty, result);
        const rhsW = widenFloatTo(p.rr, p.rt.ty, result);
        return { node: { kind: "arith", op, result, lhs: lhsW, rhs: rhsW }, type: { kind: "float", ty: result } };
      }
      requireNumericOperand(p.lt);
      requireNumericOperand(p.rt);
      if (p.lt.kind === "decimal" || p.rt.kind === "decimal") {
        return { node: { kind: "arith", op, result: "decimal", lhs: p.rl, rhs: p.rr }, type: { kind: "decimal" } };
      }
      const result = promote(p.lt, p.rt);
      return { node: { kind: "arith", op, result, lhs: p.rl, rhs: p.rr }, type: { kind: "int", ty: result } };
    }
    case "eq":
    case "ne":
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
      // A mixed-width float comparison promotes the narrower operand to f64 (float.md §3), so
      // the runtime eq3/lt3/gt3 see one width (they require both sides the same float kind).
      let cl = p.rl;
      let cr = p.rr;
      if (p.lt.kind === "float" && p.rt.kind === "float") {
        const w = promoteFloat(p.lt.ty, p.rt.ty);
        cl = widenFloatTo(p.rl, p.lt.ty, w);
        cr = widenFloatTo(p.rr, p.rt.ty, w);
      }
      return { node: { kind: "compare", op, lhs: cl, rhs: cr }, type: { kind: "bool" } };
    }
    case "concat":
      return resolveConcat(scope, lhs, rhs, ag, params);
    case "contains":
      return resolveContainment(scope, lhs, rhs, "contains", ag, params);
    case "containedBy":
      return resolveContainment(scope, lhs, rhs, "contained_by", ag, params);
    case "overlaps":
      return resolveContainment(scope, lhs, rhs, "overlaps", ag, params);
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

// resolveConcat resolves the `||` array concatenation operator (array-functions.md §8): overload
// resolution over the three kind=="concat" catalog rows — (anyarray,anyarray) [array_cat],
// (anyarray,anyelement) [array_append], (anyelement,anyarray) [array_prepend] — tried IN CATALOG
// ORDER, first match wins. It is the operator spelling of the AF1 builders and reuses their kernels.
//
// Two passes like resolveArrayFunc, with one deliberate difference: a BARE untyped NULL operand is
// left un-adapted. matchPoly defers a bare NULL in an anyarray slot, so cat-first makes `arr || NULL`
// / `NULL || arr` resolve to array_cat (the NULL array = identity), matching PostgreSQL; adapting the
// bare NULL to a typed element would wrongly steer it into array_append.
function resolveConcat(
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const noOverload = (): EngineError =>
    engineError(
      "undefined_function",
      "operator does not exist: the || operands are not an array and a compatible element/array",
    );
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let rr = resolve(scope, rhs, null, ag, params);
  // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
  let hint: ScalarType | null = null;
  if (rl.type.kind === "array") hint = elemScalarHint(rl.type.elem);
  else if (rr.type.kind === "array") hint = elemScalarHint(rr.type.elem);
  // Pass 2: re-resolve the NON-NULL operands with the hint so a bare literal element / untyped
  // ARRAY[…] adapts. A bare NULL (pass-1 kind "null") is skipped — it must stay untyped so the
  // cat-first overload order matches PG (see the doc comment).
  if (hint !== null) {
    if (rl.type.kind !== "null") rl = resolve(scope, lhs, hint, ag, params);
    if (rr.type.kind !== "null") rr = resolve(scope, rhs, hint, ag, params);
  }
  // Try the three concat overloads in catalog order; the first whose slots unify wins.
  const tys: ResolvedType[] = [rl.type, rr.type];
  let chosen: { argFamilies: readonly string[]; result: string } | undefined;
  let elem: ResolvedType | null = null;
  for (const o of OPERATORS) {
    if (o.kind !== "concat") continue;
    const m = matchPoly(o.argFamilies, tys);
    if (m.matched) {
      chosen = o;
      elem = m.elem;
      break;
    }
  }
  if (!chosen) throw noOverload();
  const type = polyResultType(chosen.result, elem);
  // The matched overload's slot pattern selects the kernel; the operands stay in source order
  // (array_prepend's kernel already reads vals[0]=element, vals[1]=array).
  let func: ArrayFuncName;
  if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyarray") func = "array_cat";
  else if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyelement") func = "array_append";
  else func = "array_prepend";
  return { node: { kind: "arrayFunc", func, args: [rl.node, rr.node] }, type };
}

// resolveContainment resolves an array containment/overlap operator `@>` / `<@` / `&&`
// (array-functions.md §10): a polymorphic `anyarray <op> anyarray → boolean`. Like resolveConcat
// (§8.1) it resolves both operands, adapts a bare literal ARRAY[…] to the first array operand's
// element type, then unifies the two element types over the single (anyarray, anyarray) overload — a
// non-array operand or an element-type mismatch is 42883. The result is always boolean (so an
// all-untyped-NULL pair is NOT 42P18). The operators are strict (a NULL whole-array operand → NULL).
function resolveContainment(
  scope: Scope,
  lhs: Expr,
  rhs: Expr,
  func: ArrayFuncName,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const noOverload = (): EngineError =>
    engineError(
      "undefined_function",
      "operator does not exist: the containment/overlap operands are not arrays of a common element type",
    );
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let rr = resolve(scope, rhs, null, ag, params);
  // The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
  let hint: ScalarType | null = null;
  if (rl.type.kind === "array") hint = elemScalarHint(rl.type.elem);
  else if (rr.type.kind === "array") hint = elemScalarHint(rr.type.elem);
  // Pass 2: re-resolve the NON-NULL operands with the hint so a bare ARRAY[…] adapts. A bare NULL
  // (pass-1 kind "null") is left untyped — it defers in the anyarray slot, result is boolean anyway.
  if (hint !== null) {
    if (rl.type.kind !== "null") rl = resolve(scope, lhs, hint, ag, params);
    if (rr.type.kind !== "null") rr = resolve(scope, rhs, hint, ag, params);
  }
  // Both slots are anyarray: the element types must unify (a non-array / mismatch is 42883).
  const tys: ResolvedType[] = [rl.type, rr.type];
  if (!matchPoly(["anyarray", "anyarray"], tys).matched) throw noOverload();
  return { node: { kind: "arrayFunc", func, args: [rl.node, rr.node] }, type: { kind: "bool" } };
}

// resolveQuantified resolves a quantified array comparison `x op ANY/SOME/ALL(arr)`
// (array-functions.md §11): the array spelling of IN. `x` (lhs) and the array operand resolve with
// the SAME literal adaptation the comparison operators use — a bare-literal `x` adapts to the array's
// element type, a bare ARRAY[…] operand adapts its elements to `x`'s type. The right operand must be
// an array (a non-array side is 42809; a bare untyped NULL is 42P18); `x` and the element type must
// be comparable (else 42883, PG's operator-not-found). The result is always boolean.
function resolveQuantified(
  scope: Scope,
  op: BinaryOp,
  all: boolean,
  lhs: Expr,
  array: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let ra = resolve(scope, array, null, ag, params);
  // If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
  // bare ARRAY[…] constructor, re-resolve the array with `x`'s type as the element hint so the
  // constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
  // column / cast operand (it ignores the hint).
  if (!isAdaptableOperand(lhs)) {
    const h = ctxOf(rl.type);
    if (h !== null) ra = resolve(scope, array, h, ag, params);
  }
  // If the array resolved to E[] and `x` is an adaptable bare literal, adapt `x` to E (with a range
  // check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
  if (ra.type.kind === "array" && isAdaptableOperand(lhs)) {
    const h = elemScalarHint(ra.type.elem);
    if (h !== null) rl = resolve(scope, lhs, h, ag, params);
  }
  // The right operand must be an array.
  if (ra.type.kind === "null") {
    // A bare untyped NULL leaves the array type undeterminable — jed's polymorphic posture (§11; the
    // unnest(NULL) / §5 #6 precedent), a documented degenerate divergence from PG.
    throw engineError(
      "indeterminate_datatype",
      "could not determine the array element type of a NULL ANY/ALL operand",
    );
  }
  if (ra.type.kind !== "array") {
    throw engineError("wrong_object_type", "op ANY/ALL (array) requires array on right side");
  }
  const elem = ra.type.elem;
  // `x` and the element type must be comparable; PG reports operator-not-found (42883) here, NOT the
  // bare 42804 a plain `int = text` raises — matching AF4's element-mismatch posture (§10.2).
  try {
    classifyComparable(rl.type, elem);
  } catch {
    throw engineError(
      "undefined_function",
      `operator does not exist: ${rtName(rl.type)} ${binaryOpSymbol(op)} ${rtName(elem)}`,
    );
  }
  return { node: { kind: "quantified", op, all, lhs: rl.node, array: ra.node }, type: { kind: "bool" } };
}

// binaryOpSymbol is the infix symbol of a comparison/arithmetic operator, for an
// `operator does not exist` message (only the comparison operators reach resolveQuantified).
function binaryOpSymbol(op: BinaryOp): string {
  switch (op) {
    case "eq":
      return "=";
    case "ne":
      return "<>";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    case "add":
      return "+";
    case "sub":
      return "-";
    case "mul":
      return "*";
    case "div":
      return "/";
    case "mod":
      return "%";
    case "and":
      return "AND";
    case "or":
      return "OR";
    case "concat":
      return "||";
    case "contains":
      return "@>";
    case "containedBy":
      return "<@";
    case "overlaps":
      return "&&";
  }
}

// resolveOperandPair resolves the two operands of a binary operator, giving a bare
// *integer* literal the other operand's integer type as context (so `small + 1` types `1`
// as i16, and `small + 100000` traps 22003 at resolve). A text literal needs no context
// (it is always text); when the sibling is text, an integer literal gets no integer context
// (intTypeOf returns null) and defaults to i64 — the caller's family check then reports
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
    l = resolve(scope, lhs, "i64", ag, params);
    r = resolve(scope, rhs, "i64", ag, params);
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
  // Composite comparison is element-wise row comparison (spec/design/composite.md §5): two
  // composites are comparable iff they have the SAME field count and each corresponding field
  // pair is itself comparable (recursively — a nested composite recurses here, an anonymous
  // `ROW(…)` compares against a same-shape named type). A bare NULL is always comparable (the
  // comparison is unknown). A composite vs any non-composite, or a row-size mismatch, or an
  // incomparable field pair, is 42804 (S5; the old 0A000 narrowing is lifted).
  const compL = lt.kind === "composite";
  const compR = rt.kind === "composite";
  if (compL && compR) {
    if (lt.fields.length !== rt.fields.length) {
      throw typeError("cannot compare rows of different sizes");
    }
    for (let i = 0; i < lt.fields.length; i++) {
      classifyComparable(lt.fields[i]!.type, rt.fields[i]!.type);
    }
    return;
  }
  if ((compL || compR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a composite value with a value of a different type");
  }
  // Array comparison is element-wise (spec/design/array.md §5): two arrays are comparable iff their
  // element types are comparable (recursively). A bare NULL is always comparable; an array vs any
  // non-array is 42804.
  const arrL = lt.kind === "array";
  const arrR = rt.kind === "array";
  if (arrL && arrR) {
    classifyComparable(lt.elem, rt.elem);
    return;
  }
  if ((arrL || arrR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare an array value with a value of a different type");
  }
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
  // float is a STRICT island (float.md §3/§6): float compares ONLY with float (either width — a
  // mixed-width pair promotes to f64) or NULL. float vs int/decimal/text/anything-else is a
  // 42804 — NOT comparable (PG promotes the other operand; jed requires an explicit cast, a
  // documented divergence). The pair is promoted to f64 in resolveBinary before eval.
  const floatL = lt.kind === "float";
  const floatR = rt.kind === "float";
  if (floatL !== floatR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a float value with a value of a different type");
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
  // timestamp / timestamptz compare only within their own family (or with NULL). A mixed
  // timestamp × timestamptz pair, or a datetime vs any other family, would need a zone, so it
  // is a 42804 type error (spec/design/timestamp.md §5).
  const tsL = lt.kind === "timestamp" || lt.kind === "timestamptz";
  const tsR = rt.kind === "timestamp" || rt.kind === "timestamptz";
  if ((tsL || tsR) && lt.kind !== rt.kind && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a timestamp value with a value of a different type");
  }
  // date compares only within its own family (or with NULL); date vs any other family — incl.
  // timestamp, which would need a cast — is a 42804 (date is a strict island, spec/design/date.md §4).
  const dateL = lt.kind === "date";
  const dateR = rt.kind === "date";
  if (dateL !== dateR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a date value with a value of a different type");
  }
  // interval compares only with itself (or NULL); interval vs any other family is a 42804.
  const ivL = lt.kind === "interval";
  const ivR = rt.kind === "interval";
  if (ivL !== ivR && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare an interval value with a value of a different type");
  }
}

// isAdaptableOperand reports whether e is an adaptable operand — one that takes its type from its
// sibling: an integer, decimal, or string literal, or a bind parameter $N (spec/design/api.md §5,
// float.md §4). NULL and boolean literals do not take a sibling's context. A DECIMAL literal is
// adaptable so it can adopt a FLOAT sibling's context (`f = 1.5`, `f + 0.5` — float.md §4); in a
// non-float context the resolve decimal case ignores the context and stays decimal, so this widens
// adaptation ONLY for the float case (the int/decimal behavior is unchanged: a decimal literal
// against an int/decimal sibling still resolves to decimal).
function isAdaptableOperand(e: Expr): boolean {
  if (e.kind === "param") return true;
  return e.kind === "literal" && (e.literal.kind === "int" || e.literal.kind === "decimal" || e.literal.kind === "text");
}

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
function ctxOf(t: ResolvedType): ScalarType | null {
  if (t.kind === "int") return t.ty;
  // A float sibling offers its width so an integer/decimal literal adapts to a float context
  // (float.md §4): `f + 1.5` types `1.5` as the float width, `f = 2` types `2` as the float width.
  if (t.kind === "float") return t.ty;
  if (t.kind === "bytea") return "bytea";
  if (t.kind === "uuid") return "uuid";
  if (t.kind === "text") return "text";
  if (t.kind === "bool") return "boolean";
  if (t.kind === "decimal") return "decimal";
  if (t.kind === "timestamp") return "timestamp";
  if (t.kind === "timestamptz") return "timestamptz";
  if (t.kind === "interval") return "interval";
  if (t.kind === "date") return "date";
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

// LIT_WS is the ASCII whitespace set trimmed by the int/decimal/bool string coercions — EXACTLY
// Rust's is_ascii_whitespace (space, tab, LF, FF, CR; NO vertical tab), so the three cores trim
// byte-identically (a §8 determinism surface — JS's Unicode-aware String.trim would diverge).
const LIT_WS = /^[ \t\n\f\r]+|[ \t\n\f\r]+$/g;
const trimLit = (s: string): string => s.replace(LIT_WS, "");
const allAsciiDigits = (s: string): boolean => /^[0-9]+$/.test(s);

// floatFromDecimalLiteral converts an untyped decimal/integer literal adapting to a float context
// into a float constant (float.md §4): the nearest binary64 to the exact decimal value (round-
// ties-to-even — JS Number(...) of the canonical decimal string is exactly the IEEE conversion),
// then Math.fround if the context width is f32. The exact-decimal cap-check is NOT applied: a
// literal adapting to a float column is a float value, not a stored decimal. A magnitude beyond the
// binary64 range becomes ±Infinity here — but a finite literal is meant, so an out-of-range literal
// traps 22003 (the finite-overflow rule, §3) rather than silently yielding Infinity.
function floatFromDecimalLiteral(d: Decimal, ty: ScalarType): { node: RExpr; type: ResolvedType } {
  const exact = Number(d.render());
  if (!Number.isFinite(exact)) throw overflow(ty);
  const n = roundToWidth(ty, exact);
  if (!Number.isFinite(n)) throw overflow(ty); // f32 rounding pushed a finite double to ±Inf
  return { node: { kind: "constFloat", ty, value: n }, type: { kind: "float", ty } };
}

// coerceStringLiteral coerces a string literal's content to the named scalar target at resolve —
// the shared engine of the `type 'string'` typed literal and CAST(<string literal> AS target)
// (spec/design/grammar.md §36, types.md §5). Every scalar is reachable: the string-native types
// parse by their own input, text is identity, and int/decimal/boolean are the cast from text
// admitted only for a literal operand. 22P02 malformed / 22003 out of range / the type's parse
// code. typmod (decimal only) re-scales the result.
function coerceStringLiteral(
  s: string,
  target: ScalarType,
  typmod: DecimalTypmod | null,
): { node: RExpr; type: ResolvedType } {
  switch (target) {
    case "bytea":
      return { node: { kind: "constBytea", value: decodeByteaLiteral(s) }, type: { kind: "bytea" } };
    case "uuid":
      return { node: { kind: "constUuid", value: decodeUuidLiteral(s) }, type: { kind: "uuid" } };
    case "timestamp":
      return { node: { kind: "constTimestamp", value: parseTimestamp(s) }, type: { kind: "timestamp" } };
    case "timestamptz":
      return { node: { kind: "constTimestamptz", value: parseTimestamptz(s) }, type: { kind: "timestamptz" } };
    case "date":
      return { node: { kind: "constDate", value: parseDate(s) }, type: { kind: "date" } };
    case "interval":
      return { node: { kind: "constInterval", value: parseInterval(s) }, type: { kind: "interval" } };
    case "text":
      // text 'x' is identity — the string IS the value.
      return { node: { kind: "constText", value: s }, type: { kind: "text" } };
    case "boolean":
      return { node: { kind: "constBool", value: parseBoolLiteral(s) }, type: { kind: "bool" } };
    case "f32":
    case "f64": {
      const n = parseFloatLiteral(s, target);
      return { node: { kind: "constFloat", ty: target, value: n }, type: { kind: "float", ty: target } };
    }
    case "decimal": {
      let d = parseDecimalLiteral(s);
      d = typmod !== null ? d.coerceToTypmod(typmod.precision, typmod.scale) : d.checkCap();
      return { node: { kind: "constDecimal", value: d }, type: { kind: "decimal" } };
    }
    default: {
      // i16 / i32 / i64
      const n = parseIntLiteral(s, target);
      return { node: { kind: "constInt", value: n }, type: { kind: "int", ty: target } };
    }
  }
}

// coerceStringToComposite coerces a string literal to a named composite via record_in
// (spec/design/composite.md §8) — the shared primitive behind `'(…)'::addr` and the `addr '(…)'`
// typed literal. It tokenizes the text (a malformed literal or a field-count mismatch is 22P02
// "malformed record literal: …"), then coerces each token to its field's declared type: a NULL token
// (unquoted-empty) becomes a typed NULL; a scalar field reuses the same string-literal coercion as a
// typed literal (its own parse errors surface — e.g. 22P02 for a non-integer); a nested composite
// field recurses. Folds to a `row` RExpr of the coerced const field nodes, typed as the named
// composite (the TS-idiomatic equivalent of the Rust `RExpr::Row` over `ResolvedType::Composite`).
function coerceStringToComposite(
  text: string,
  ct: CompositeType,
  db: Database,
): { node: RExpr; type: ResolvedType } {
  const malformed = (): Error =>
    engineError("invalid_text_representation", `malformed record literal: "${text}" for type ${ct.name}`);
  const tokens = parseRecordTokens(text);
  if (tokens === null || tokens.length !== ct.fields.length) throw malformed();
  const nodes: RExpr[] = [];
  const fieldTypes: { name: string; type: ResolvedType }[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i]!;
    const f = ct.fields[i]!;
    if (tok === null) {
      // A NULL field: a NULL value, typed by the field's declared type.
      nodes.push({ kind: "constNull" });
      fieldTypes.push({ name: f.name, type: resolvedTypeOfCol(f.type, db) });
    } else if (f.type.kind === "composite") {
      const nested = db.compositeType(f.type.name);
      if (nested === undefined) throw new Error("nested composite type resolved at CREATE TYPE / load");
      const { node, type } = coerceStringToComposite(tok, nested, db);
      nodes.push(node);
      fieldTypes.push({ name: f.name, type });
    } else if (f.type.kind === "array") {
      // An array-typed field (spec/design/array.md §12): the token is an array text literal,
      // coerced through array_in against the element type — the same path a bare `'{…}'::T[]` cast
      // uses, one level down. Folds to a constant array.
      const elemCol = db.colTypeOf(f.type.elem);
      const val = coerceStringToArray(tok, elemCol);
      nodes.push(valueToRExpr(val));
      fieldTypes.push({ name: f.name, type: resolvedTypeOfCol(f.type, db) });
    } else {
      const { node, type } = coerceStringLiteral(tok, f.type.scalar, f.decimal);
      nodes.push(node);
      fieldTypes.push({ name: f.name, type });
    }
  }
  return { node: { kind: "row", fields: nodes }, type: { kind: "composite", name: ct.name, fields: fieldTypes } };
}

// parseIntLiteral parses a string literal's content as a signed integer of type ty — the
// text→integer coercion for INTEGER '42' / CAST('42' AS int) (grammar.md §36). jed's OWN
// integer-literal grammar: trimmed ASCII whitespace, optional +/-, then ASCII decimal digits
// (NO hex/octal/binary or underscores — 22P02, a documented PG divergence). Out of range → 22003.
function parseIntLiteral(s: string, ty: ScalarType): bigint {
  const invalid = (): Error =>
    engineError("invalid_text_representation", `invalid input syntax for type ${canonicalName(ty)}: "${s}"`);
  let t = trimLit(s);
  let neg = false;
  if (t.startsWith("-")) {
    neg = true;
    t = t.slice(1);
  } else if (t.startsWith("+")) {
    t = t.slice(1);
  }
  if (t === "" || !allAsciiDigits(t)) throw invalid();
  // BigInt holds an arbitrary-length digit run; range is checked below (out of range → 22003).
  const v = neg ? -BigInt(t) : BigInt(t);
  if (!inRange(ty, v)) throw overflow(ty);
  return v;
}

// parseDecimalLiteral parses a string literal's content as a decimal — the text→decimal coercion
// for NUMERIC '1.5' / CAST('1.5' AS numeric) (grammar.md §36). jed's OWN decimal-literal grammar:
// trimmed ASCII whitespace, optional sign, ASCII digits with at most one '.' and a digit on at
// least one side, plus optional scientific e-notation (numeric '1.5e3' → 1500) — built into the SAME
// (digits, scale) the lexer feeds Decimal.fromDigitsScale (via the shared decimalFromParts), so
// NUMERIC 'x' is byte-identical to writing x. NO NaN / Infinity and no hex/underscore (22P02).
// Caller applies typmod / cap-check.
function parseDecimalLiteral(s: string): Decimal {
  const invalid = (): Error =>
    engineError("invalid_text_representation", `invalid input syntax for type numeric: "${s}"`);
  let t = trimLit(s);
  let neg = false;
  if (t.startsWith("-")) {
    neg = true;
    t = t.slice(1);
  } else if (t.startsWith("+")) {
    t = t.slice(1);
  }
  // Split off an optional exponent. Unlike the lexer (which leaves a bare e for the next token), an
  // isolated string must be a COMPLETE numeric, so an e with no [+-]?digit+ after it is malformed
  // (22P02), matching PG's numeric_in.
  let mantissa = t;
  let exp: number | null = null;
  const ei = t.search(/[eE]/);
  if (ei >= 0) {
    mantissa = t.slice(0, ei);
    let e = t.slice(ei + 1);
    let eneg = false;
    if (e.startsWith("-")) {
      eneg = true;
      e = e.slice(1);
    } else if (e.startsWith("+")) {
      e = e.slice(1);
    }
    if (e === "" || !allAsciiDigits(e)) {
      throw invalid();
    }
    // Clamp the magnitude to EXP_LIMIT while accumulating (bounds the coefficient the shared
    // builder may materialize).
    let v = 0;
    for (let k = 0; k < e.length; k++) {
      if (v < EXP_LIMIT) {
        v = v * 10 + (e.charCodeAt(k) - 48);
        if (v > EXP_LIMIT) v = EXP_LIMIT;
      }
    }
    exp = eneg ? -v : v;
  }
  const dot = mantissa.indexOf(".");
  const intPart = dot < 0 ? mantissa : mantissa.slice(0, dot);
  const frac = dot < 0 ? "" : mantissa.slice(dot + 1);
  // A second '.' lands in frac (indexOf found the first); reject it.
  if (
    frac.includes(".") ||
    !(intPart === "" || allAsciiDigits(intPart)) ||
    !(frac === "" || allAsciiDigits(frac)) ||
    (intPart === "" && frac === "")
  ) {
    throw invalid();
  }
  const [digits, scale] = decimalFromParts(intPart, frac, exp);
  return Decimal.fromDigitsScale(neg, digits, scale);
}

// parseBoolLiteral parses a string literal's content as a boolean — the text→boolean coercion for
// BOOLEAN 'true' / CAST('t' AS boolean) (grammar.md §36). Matches PostgreSQL's boolin: trimmed
// ASCII whitespace, case-insensitive; t/tr/tru/true, y/ye/yes, on, 1 → true and f/fa/fal/fals/
// false, n/no, off, 0 → false; anything else 22P02.
function parseBoolLiteral(s: string): boolean {
  switch (trimLit(s).toLowerCase()) {
    case "t":
    case "tr":
    case "tru":
    case "true":
    case "y":
    case "ye":
    case "yes":
    case "on":
    case "1":
      return true;
    case "f":
    case "fa":
    case "fal":
    case "fals":
    case "false":
    case "n":
    case "no":
    case "off":
    case "0":
      return false;
    default:
      throw engineError("invalid_text_representation", `invalid input syntax for type boolean: "${s}"`);
  }
}

// FLOAT_GRAMMAR is jed's f64 string-input grammar (float.md §4 — PG's float8in subset): an
// optional sign, then either a finite decimal (digits with an optional point and optional
// e-notation) or one of the special words. It is validated explicitly — NOT via parseFloat, which
// is too lenient (it accepts "1.5xyz", leading junk after trim, etc.). Anchored to the whole
// (trimmed) string so trailing junk is rejected → 22P02.
const FLOAT_FINITE = /^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/;

// parseFloatLiteral parses a string literal's content as a float of type ty — the text→float
// coercion for `float '1.5'` / `real '1e10'` / CAST('Infinity' AS f64) (float.md §4). Grammar:
// trimmed ASCII whitespace (the shared LIT_WS), optional sign, finite decimal with optional point
// and e-notation, OR the case-insensitive specials Infinity/+Infinity/-Infinity/inf/+inf/-inf/NaN.
// Malformed input → 22P02; a finite value outside the binary64 range → 22003. For f32 the
// parsed binary64 is Math.fround'd; a finite value that frounds to ±Inf (beyond binary32 range)
// also traps 22003. NaN/±Infinity are first-class here (they enter ONLY via this path, casts, or
// stored values — float.md §3).
function parseFloatLiteral(s: string, ty: ScalarType): number {
  const invalid = (): Error =>
    engineError("invalid_text_representation", `invalid input syntax for type ${canonicalName(ty)}: "${s}"`);
  const t = trimLit(s);
  // Special words (case-insensitive), with an optional leading sign on the infinities.
  const lower = t.toLowerCase();
  let special: number | undefined;
  switch (lower) {
    case "nan":
      special = NaN;
      break;
    case "infinity":
    case "+infinity":
    case "inf":
    case "+inf":
      special = Infinity;
      break;
    case "-infinity":
    case "-inf":
      special = -Infinity;
      break;
  }
  if (special !== undefined) return ty === "f32" ? Math.fround(special) : special;
  if (!FLOAT_GRAMMAR_OK(t)) throw invalid();
  // Number(...) does the IEEE-correct decimal→binary64 conversion (round-ties-to-even). The grammar
  // already rejected junk, so a NaN here would only come from an empty/degenerate string the regex
  // also rejects; guard anyway.
  const d = Number(t);
  if (Number.isNaN(d)) throw invalid();
  // A finite literal that overflows the binary64 range parses to ±Infinity — trap 22003 rather than
  // yield Infinity (Infinity is input-only via the special words above, not via a finite literal).
  if (!Number.isFinite(d)) throw overflow(ty);
  const n = ty === "f32" ? Math.fround(d) : d;
  if (!Number.isFinite(n)) throw overflow(ty); // finite double beyond binary32 range
  return n;
}

// FLOAT_GRAMMAR_OK tests the finite-decimal grammar (a named wrapper so the regex's role is legible).
function FLOAT_GRAMMAR_OK(t: string): boolean {
  return FLOAT_FINITE.test(t);
}

// widenFloatTo wraps a float operand in an explicit widening cast when its width is narrower than
// the target (f32 → f64 is lossless — float.md §2), so a mixed-width float arithmetic /
// comparison node sees both sides at one width. Identity when from === to. Implemented as a `cast`
// RExpr (the evaluator's evalCast handles float→float widening), so no new node kind is needed.
function widenFloatTo(node: RExpr, from: ScalarType, to: ScalarType): RExpr {
  return from === to ? node : { kind: "cast", target: to, typmod: null, operand: node };
}

// promote is the promotion-tower result type of two arithmetic operands: the
// higher-ranked integer type, or i64 when both are untyped NULLs.
function promote(a: ResolvedType, b: ResolvedType): ScalarType {
  const ax = intTypeOf(a);
  const bx = intTypeOf(b);
  if (ax !== null && bx !== null) return rank(ax) >= rank(bx) ? ax : bx;
  if (ax !== null) return ax;
  if (bx !== null) return bx;
  return "i64";
}

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
function requireNumericOperand(t: ResolvedType): void {
  if (
    t.kind === "bool" ||
    t.kind === "text" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz" ||
    t.kind === "interval" ||
    t.kind === "date" ||
    // float is a strict island — it never mixes with int/decimal arithmetic (the both-float case
    // is handled before this; reaching here with a float means a cross-family int/decimal ⊕ float
    // pair → 42804, float.md §6).
    t.kind === "float"
  ) {
    throw typeError("arithmetic operators require numeric operands");
  }
}

// temporalArithResult gives the result type of a temporal +/- (spec/design/interval.md §5), or
// undefined when neither operand is temporal (then arithmetic falls through to the numeric path).
// A temporal operand in an unsupported combination throws 42804. A NULL operand adopts the other
// side's temporal type (so `timestamp ± NULL` types as timestamp and evaluates to NULL).
type RtKind = ResolvedType["kind"];

// intervalScaleResult gives the result type of an interval ×÷ number (spec/design/interval.md §5):
// interval * number, number * interval (commute), interval / number → interval. undefined when no
// interval is involved (or the op is not * / /). number / interval and interval × interval return
// undefined and fall to the ±-only temporal rule (which reports the 42804).
function intervalScaleResult(op: BinaryOp, lt: RtKind, rt: RtKind): ScalarType | undefined {
  const lIv = lt === "interval";
  const rIv = rt === "interval";
  if (!lIv && !rIv) return undefined;
  const numeric = (k: RtKind) => k === "int" || k === "decimal" || k === "null";
  if (op === "mul" && ((lIv && numeric(rt)) || (rIv && numeric(lt)))) return "interval";
  if (op === "div" && lIv && numeric(rt)) return "interval";
  return undefined;
}

// factorToFraction returns a numeric factor value as an exact fraction [num, den] with den > 0.
function factorToFraction(v: Value): [bigint, bigint] {
  if (v.kind === "int") return [v.int, 1n];
  if (v.kind === "decimal") return parseFactorDecimal(v.dec.render());
  throw typeError("internal: non-numeric interval-scale factor");
}

function temporalArithResult(op: BinaryOp, lt: RtKind, rt: RtKind): ScalarType | undefined {
  const temporal = (k: RtKind) => k === "interval" || k === "timestamp" || k === "timestamptz";
  if (!temporal(lt) && !temporal(rt)) return undefined;
  const l = lt === "null" ? rt : lt;
  const r = rt === "null" ? lt : rt;
  if ((op === "add" || op === "sub") && l === "interval" && r === "interval") return "interval";
  if (op === "add" && ((l === "timestamp" && r === "interval") || (l === "interval" && r === "timestamp"))) return "timestamp";
  if (op === "sub" && l === "timestamp" && r === "interval") return "timestamp";
  if (op === "add" && ((l === "timestamptz" && r === "interval") || (l === "interval" && r === "timestamptz"))) return "timestamptz";
  if (op === "sub" && l === "timestamptz" && r === "interval") return "timestamptz";
  if (op === "sub" && ((l === "timestamp" && r === "timestamp") || (l === "timestamptz" && r === "timestamptz"))) return "interval";
  throw typeError("unsupported operand types for temporal arithmetic");
}

function requireBool(t: ResolvedType, msg: string): void {
  if (
    t.kind === "int" ||
    t.kind === "float" ||
    t.kind === "text" ||
    t.kind === "decimal" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz" ||
    t.kind === "interval" ||
    t.kind === "date"
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

// unifyArrayElementTypes unifies the element types of an ARRAY[...] constructor into one element
// type (spec/design/array.md §1). All-NULL → text (the PG unknown rule). All integer → the widest
// via the promotion tower (no runtime coercion — every integer is a bigint). Otherwise every element
// must be the SAME family — a cross-family mix (including int + decimal) is a documented 42804
// narrowing this slice (the representation-changing coercion is deferred with numeric(p,s)[]).
function unifyArrayElementTypes(types: ResolvedType[]): ResolvedType {
  const nonNull = types.filter((t) => t.kind !== "null");
  if (nonNull.length === 0) return { kind: "text" };
  if (nonNull.every((t) => t.kind === "int")) {
    let acc = nonNull[0]!;
    for (const t of nonNull.slice(1)) acc = { kind: "int", ty: promote(acc, t) };
    return acc;
  }
  const first = nonNull[0]!;
  for (const t of nonNull.slice(1)) {
    if (t.kind !== first.kind) throw typeError("array elements must all be of the same type");
  }
  return first;
}

// arraySubscriptErr is a 2202E array-subscript error (spec/design/array.md §11).
function arraySubscriptErr(detail: string): Error {
  return engineError("array_subscript_error", detail);
}

// countNulls counts the NULL (when wantNulls) or non-NULL values in vals — the shared kernel of
// num_nulls / num_nonnulls (spec/design/array-functions.md §12), over either the spread arguments or
// a VARIADIC array's flattened elements.
function countNulls(vals: Value[], wantNulls: boolean): number {
  let n = 0;
  for (const v of vals) if ((v.kind === "null") === wantNulls) n++;
  return n;
}

// evalArrayFunc evaluates an array function over its already-evaluated argument values
// (spec/design/array-functions.md §3). The introspectors propagate NULL and return NULL for an
// out-of-shape request; the builders are non-strict (a NULL array argument is the identity/empty, NOT
// a propagated NULL). The resolver guarantees the array operand is an array or NULL.
function evalArrayFunc(func: ArrayFuncName, vals: Value[]): Value {
  switch (func) {
    case "array_ndims": {
      const a = vals[0]!;
      if (a.kind !== "array") return nullValue();
      return arrayNdim(a) === 0 ? nullValue() : intValue(BigInt(arrayNdim(a))); // empty → NULL (PG)
    }
    case "cardinality": {
      const a = vals[0]!;
      if (a.kind !== "array") return nullValue();
      return intValue(BigInt(a.elements.length)); // 0 for empty (NOT NULL)
    }
    case "array_dims": {
      const a = vals[0]!;
      if (a.kind !== "array" || arrayNdim(a) === 0) return nullValue();
      return textValue(arrayDimsText(a));
    }
    case "array_length":
    case "array_lower":
    case "array_upper": {
      const a = vals[0]!;
      const dimV = vals[1]!;
      if (a.kind !== "array" || dimV.kind === "null") return nullValue();
      const dim = (dimV as { int: bigint }).int;
      const nd = arrayNdim(a);
      if (nd === 0 || dim < 1n || dim > BigInt(nd)) return nullValue();
      const d = Number(dim) - 1;
      if (func === "array_length") return intValue(BigInt(a.dims[d]!));
      if (func === "array_lower") return intValue(BigInt(a.lbounds[d]!));
      return intValue(BigInt(arrayUbound(a, d)));
    }
    case "array_append":
      return arrayExtend(vals[0]!, vals[1]!, true);
    case "array_prepend":
      return arrayExtend(vals[1]!, vals[0]!, false);
    case "array_cat":
      return arrayCatValues(vals[0]!, vals[1]!);
    case "array_remove":
      return arrayRemoveValue(vals[0]!, vals[1]!);
    case "array_replace":
      return arrayReplaceValue(vals[0]!, vals[1]!, vals[2]!);
    case "array_position":
      return arrayPositionValue(vals[0]!, vals[1]!, vals.length > 2 ? vals[2]! : null);
    case "array_positions":
      return arrayPositionsValue(vals[0]!, vals[1]!);
    case "contains":
      return arrayContainsValue(vals[0]!, vals[1]!);
    case "contained_by":
      return arrayContainsValue(vals[1]!, vals[0]!);
    case "overlaps":
      return arrayOverlapsValue(vals[0]!, vals[1]!);
  }
}

// notDistinct is IS NOT DISTINCT FROM at the value level (array-functions.md §5 #10): jed's total
// element comparator, so NULL equals NULL and a non-NULL never equals NULL.
function notDistinct(a: Value, b: Value): boolean {
  return valueCmp(a, b) === 0;
}

// strictElemEq is STRICT element equality for the containment/overlap operators (array-functions.md
// §10): a NULL element equals NOTHING — including another NULL — the deliberate inverse of notDistinct
// (§5 #10). For two non-NULL values it is jed's total element comparator (valueCmp === 0).
function strictElemEq(a: Value, b: Value): boolean {
  return a.kind !== "null" && b.kind !== "null" && valueCmp(a, b) === 0;
}

// arrayContainsValue is a @> b (array-functions.md §10): does a CONTAIN b — is every element of b
// present in a under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
// whole-array operand → NULL. The empty array is contained by anything (a @> {} is true).
function arrayContainsValue(a: Value, b: Value): Value {
  if (a.kind !== "array" || b.kind !== "array") return nullValue();
  const contained = b.elements.every((eb) => a.elements.some((ea) => strictElemEq(ea, eb)));
  return boolValue(contained);
}

// arrayOverlapsValue is a && b (array-functions.md §10): do a and b OVERLAP — share at least one
// element under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
// whole-array operand → NULL. The empty array overlaps nothing.
function arrayOverlapsValue(a: Value, b: Value): Value {
  if (a.kind !== "array" || b.kind !== "array") return nullValue();
  const overlaps = a.elements.some((ea) => b.elements.some((eb) => strictElemEq(ea, eb)));
  return boolValue(overlaps);
}

// arrayRemoveValue is array_remove(a, e) (array-functions.md §8): drop every element NOT DISTINCT
// FROM e. NULL array → NULL; 1-D/empty only (a multidimensional array is 0A000); the lower bound is
// preserved and an all-removed result is the empty array {}.
function arrayRemoveValue(arr: Value, elem: Value): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError("feature_not_supported", "removing elements from multidimensional arrays is not supported");
  }
  const kept = arr.elements.filter((e) => !notDistinct(e, elem));
  if (kept.length === 0) return emptyArray();
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  return { kind: "array", dims: [kept.length], lbounds: [lb], elements: kept };
}

// arrayReplaceValue is array_replace(a, from, to) (array-functions.md §8): substitute every element
// NOT DISTINCT FROM `from` with `to`. Works on any dimensionality (the shape is preserved). NULL
// array → NULL.
function arrayReplaceValue(arr: Value, from: Value, to: Value): Value {
  if (arr.kind !== "array") return nullValue();
  const elements = arr.elements.map((e) => (notDistinct(e, from) ? to : e));
  return { kind: "array", dims: [...arr.dims], lbounds: [...arr.lbounds], elements };
}

// arrayPositionValue is array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the
// array's lower-bound space) of the first element NOT DISTINCT FROM e, NULL if absent. 1-D/empty only
// (a multidimensional array is 0A000); the optional start is a subscript to begin at, and a NULL
// start is 22004.
function arrayPositionValue(arr: Value, elem: Value, start: Value | null): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError("feature_not_supported", "searching for elements in multidimensional arrays is not supported");
  }
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  let begin = 0;
  if (start !== null) {
    if (start.kind === "null") throw engineError("null_value_not_allowed", "initial position must not be null");
    const off = Number((start as { int: bigint }).int) - lb;
    if (off > 0) begin = off;
  }
  for (let i = begin; i < arr.elements.length; i++) {
    if (notDistinct(arr.elements[i]!, elem)) return intValue(BigInt(lb + i));
  }
  return nullValue();
}

// arrayPositionsValue is array_positions(a, e) (array-functions.md §8): the i32[] of every match's
// subscript (in the array's lower-bound space), the empty array {} if none. NULL array → NULL;
// 1-D/empty only (a multidimensional array is 0A000).
function arrayPositionsValue(arr: Value, elem: Value): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError("feature_not_supported", "searching for elements in multidimensional arrays is not supported");
  }
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  const positions: Value[] = [];
  for (let i = 0; i < arr.elements.length; i++) {
    if (notDistinct(arr.elements[i]!, elem)) positions.push(intValue(BigInt(lb + i)));
  }
  return arrayValue(positions);
}

// arrayDimsText is the array_dims text form `[l1:u1][l2:u2]…` (no trailing `=`, unlike array_out's
// prefix — array-functions.md §3.1).
function arrayDimsText(a: { dims: number[]; lbounds: number[] }): string {
  let s = "";
  for (let d = 0; d < a.dims.length; d++) s += "[" + a.lbounds[d] + ":" + arrayUbound(a, d) + "]";
  return s;
}

// arrayExtend is array_append (atEnd=true) / array_prepend (array-functions.md §3.2). The array side
// is non-strict: a NULL or empty array yields the 1-D singleton {elem} (lower bound 1). A 1-D array
// grows by one element, preserving its lower bound; a multidimensional array is 22000.
function arrayExtend(arr: Value, elem: Value, atEnd: boolean): Value {
  if (arr.kind !== "array" || arr.dims.length === 0) return arrayValue([elem]);
  if (arr.dims.length !== 1) {
    throw engineError("data_exception", "argument must be empty or one-dimensional array");
  }
  const elements = atEnd ? [...arr.elements, elem] : [elem, ...arr.elements];
  return { kind: "array", dims: [arr.dims[0]! + 1], lbounds: [...arr.lbounds], elements };
}

// arrayCatValues is array_cat (array-functions.md §3.2): identity-aware concatenation along the outer
// dimension. NULL/empty is the identity (both NULL → NULL). Same dimensionality concatenates if the
// inner dims match; an off-by-one dimensionality appends/prepends the lower one as an outer slice; any
// other pairing — or an inner-dim mismatch — is 2202E. The flattened element list is always a ++ b
// (row-major, outer-first); the result lower bounds come from the higher-dim operand.
function arrayCatValues(a: Value, b: Value): Value {
  if (a.kind === "null" && b.kind === "null") return nullValue();
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind !== "array" || b.kind !== "array") return nullValue(); // unreachable (resolver gate)
  if (a.dims.length === 0) return b;
  if (b.dims.length === 0) return a;
  const mismatch = () => engineError("array_subscript_error", "cannot concatenate incompatible arrays");
  const eqInts = (x: number[], y: number[]): boolean => x.length === y.length && x.every((v, i) => v === y[i]);
  const elements = [...a.elements, ...b.elements];
  const na = a.dims.length;
  const nb = b.dims.length;
  if (na === nb) {
    if (!eqInts(a.dims.slice(1), b.dims.slice(1))) throw mismatch();
    const dims = [...a.dims];
    dims[0] = a.dims[0]! + b.dims[0]!;
    return { kind: "array", dims, lbounds: [...a.lbounds], elements };
  }
  if (na === nb + 1) {
    if (!eqInts(a.dims.slice(1), b.dims)) throw mismatch();
    const dims = [...a.dims];
    dims[0] = a.dims[0]! + 1;
    return { kind: "array", dims, lbounds: [...a.lbounds], elements };
  }
  if (nb === na + 1) {
    if (!eqInts(b.dims.slice(1), a.dims)) throw mismatch();
    const dims = [...b.dims];
    dims[0] = b.dims[0]! + 1;
    return { kind: "array", dims, lbounds: [...b.lbounds], elements };
  }
  throw mismatch();
}

// buildNestedArray stacks the evaluated elements of a nested ARRAY[...] constructor into a value of
// one higher dimension (spec/design/array.md §4). The resolver guarantees every item is an array; a
// NULL sub-array or a sub-array of differing shape is a 2202E. Stacking empty sub-arrays yields the
// empty array (PG: ARRAY['{}'::int[]] → {}).
function buildNestedArray(subs: Value[]): Value {
  const mismatch = "multidimensional arrays must have array expressions with matching dimensions";
  const arrs = subs.map((sv) => {
    if (sv.kind === "array") return sv;
    if (sv.kind === "null") throw arraySubscriptErr(mismatch);
    throw typeError("internal: nested array constructor over a non-array");
  });
  const eqNum = (a: number[], b: number[]): boolean => a.length === b.length && a.every((x, i) => x === b[i]);
  const dims0 = arrs[0]!.dims;
  const lbounds0 = arrs[0]!.lbounds;
  for (const a of arrs.slice(1)) {
    if (!eqNum(a.dims, dims0) || !eqNum(a.lbounds, lbounds0)) throw arraySubscriptErr(mismatch);
  }
  if (dims0.length === 0) return emptyArray(); // all sub-arrays empty → empty array
  const elements: Value[] = [];
  for (const a of arrs) elements.push(...a.elements);
  return { kind: "array", dims: [arrs.length, ...dims0], lbounds: [1, ...lbounds0], elements };
}

// evalSubscript evaluates an array subscript `base[..][..]` (spec/design/array.md §6). A NULL array
// or any NULL subscript bound yields NULL; element access returns the element (or NULL), slice
// access a (renumbered) sub-array.
function evalSubscript(
  e: { base: RExpr; subscripts: RSubscript[]; isSlice: boolean },
  row: Row,
  env: EvalEnv,
  m: Meter,
): Value {
  const base = evalExpr(e.base, row, env, m);
  if (base.kind === "null") return nullValue();
  if (base.kind !== "array") throw typeError("internal: subscript on a non-array value");
  if (e.isSlice) {
    // Per-dimension (lower, upper); a scalar index i becomes 1:i (PG), an omitted bound defers to
    // the array's own bound (null lo/hi). A NULL bound → NULL.
    const los: (bigint | null)[] = [];
    const his: (bigint | null)[] = [];
    for (const s of e.subscripts) {
      if (!s.isSlice) {
        const v = evalExpr(s.index, row, env, m);
        if (v.kind === "null") return nullValue();
        if (v.kind !== "int") throw typeError("internal: non-integer array subscript");
        los.push(1n); // scalar i → 1:i
        his.push(v.int);
      } else {
        const lo = evalOptBound(s.lower, row, env, m);
        if (lo === "null") return nullValue();
        const hi = evalOptBound(s.upper, row, env, m);
        if (hi === "null") return nullValue();
        los.push(lo);
        his.push(hi);
      }
    }
    return arrayGetSlice(base, los, his);
  }
  // Element access: every spec is an index.
  const idxs: bigint[] = [];
  for (const s of e.subscripts) {
    if (s.isSlice) throw typeError("internal: slice spec in element access");
    const v = evalExpr(s.index, row, env, m);
    if (v.kind === "null") return nullValue();
    if (v.kind !== "int") throw typeError("internal: non-integer array subscript");
    idxs.push(v.int);
  }
  return arrayGetElement(base, idxs);
}

// evalOptBound evaluates an optional slice-bound expression: null expr → null (defer to the array
// bound); a NULL value → "null" (the whole result is NULL); an integer → its bigint.
function evalOptBound(e: RExpr | null, row: Row, env: EvalEnv, m: Meter): bigint | null | "null" {
  if (e === null) return null;
  const v = evalExpr(e, row, env, m);
  if (v.kind === "null") return "null";
  if (v.kind !== "int") throw typeError("internal: non-integer array slice bound");
  return v.int;
}

// arrayGetElement reads a single array element by idxs (1-based per dimension, using the value's
// lower bounds) — spec/design/array.md §6. NULL when the subscript count ≠ ndim or any index is out
// of range.
function arrayGetElement(a: { dims: number[]; lbounds: number[]; elements: Value[] }, idxs: bigint[]): Value {
  const ndim = arrayNdim(a);
  if (idxs.length !== ndim || a.elements.length === 0) return nullValue();
  let flat = 0;
  let stride = 1;
  for (let d = ndim - 1; d >= 0; d--) {
    const lb = BigInt(a.lbounds[d]!);
    const ub = BigInt(arrayUbound(a, d));
    if (idxs[d]! < lb || idxs[d]! > ub) return nullValue();
    flat += Number(idxs[d]! - lb) * stride;
    stride *= a.dims[d]!;
  }
  return a.elements[flat]!;
}

// arrayGetSlice reads an array slice (spec/design/array.md §6): per-dimension requested (lower,
// upper) bounds (null defers to the value's own bound), clamped to each dimension's [lb,ub]. Too many
// subscripts, an empty source, or any empty clamped dimension yields the empty array; fewer
// subscripts than ndim leave the trailing dimensions at full range. The result is renumbered to lower
// bound 1 on every dimension (PG array_get_slice).
function arrayGetSlice(
  a: { dims: number[]; lbounds: number[]; elements: Value[] },
  los: (bigint | null)[],
  his: (bigint | null)[],
): Value {
  const ndim = arrayNdim(a);
  if (los.length > ndim || ndim === 0) return emptyArray();
  const newDims: number[] = new Array(ndim);
  const starts: number[] = new Array(ndim); // source 0-based start per dimension
  for (let d = 0; d < ndim; d++) {
    const lb = BigInt(a.lbounds[d]!);
    const ub = BigInt(arrayUbound(a, d));
    let reqLo = lb;
    let reqHi = ub;
    if (d < los.length) {
      if (los[d] !== null) reqLo = los[d]!;
      if (his[d] !== null) reqHi = his[d]!;
    }
    const lo = reqLo < lb ? lb : reqLo;
    const hi = reqHi > ub ? ub : reqHi;
    if (lo > hi) return emptyArray(); // any empty dimension → empty slice
    newDims[d] = Number(hi - lo + 1n);
    starts[d] = Number(lo - lb);
  }
  // Row-major strides over the SOURCE array.
  const strides: number[] = new Array(ndim);
  strides[ndim - 1] = 1;
  for (let d = ndim - 2; d >= 0; d--) strides[d] = strides[d + 1]! * a.dims[d + 1]!;
  let total = 1;
  for (const d of newDims) total *= d;
  const elements: Value[] = new Array(total);
  const counter: number[] = new Array(ndim).fill(0);
  for (let k = 0; k < total; k++) {
    let flat = 0;
    for (let d = 0; d < ndim; d++) flat += (starts[d]! + counter[d]!) * strides[d]!;
    elements[k] = a.elements[flat]!;
    for (let d = ndim - 1; d >= 0; d--) {
      counter[d]!++;
      if (counter[d]! < newDims[d]!) break;
      counter[d] = 0;
    }
  }
  return { kind: "array", dims: newDims, lbounds: new Array(ndim).fill(1), elements };
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
  // All float: the widest via the float tower (f32 + f64 → f64). A float mixed with a
  // non-float arm is a cross-family 42804 (caught by the same-family check below — float is a strict
  // island, no int/decimal reconciliation, float.md §6).
  if (nonNull.every((t) => t.kind === "float")) {
    let acc = (nonNull[0] as { kind: "float"; ty: ScalarType }).ty;
    for (const t of nonNull.slice(1)) acc = promoteFloat(acc, (t as { ty: ScalarType }).ty);
    return { kind: "float", ty: acc };
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

// setopName is the operator's name for an error message (PostgreSQL phrasing).
function setopName(op: SetOpKind): string {
  return op === "union" ? "UNION" : op === "intersect" ? "INTERSECT" : "EXCEPT";
}

// unifySetopColumn unifies one output column's type across the two operands of a set operation
// (spec/design/grammar.md §25, types.md §4): integer widths promote to the widest; integer with
// decimal -> decimal; a NULL-typed operand takes the other's type (an all-NULL column stays "null"
// — PostgreSQL would call a top-level one text, but the type is never observed in output); a
// same-family non-numeric pair gives that type; anything else is 42804. The set of unifiable pairs
// mirrors the comparability matrix (compare.toml).
// unifyValuesColumn unifies two row value types for the SAME VALUES-body column
// (spec/design/grammar.md §42), the set-operation rule (§25): integer widths widen, int+decimal ->
// decimal, anything + NULL keeps the other, and a same-type scalar pair (text, boolean, bytea, uuid,
// a timestamp / timestamptz, an interval, a same-width float) unifies to itself; any other pair —
// including a composite or array column across rows (a deferred edge) — is 42804. Enumerated
// EXPLICITLY (not a generic same-kind passthrough) so all three cores compute byte-identical
// results (CLAUDE.md §8).
function unifyValuesColumn(a: ResolvedType, b: ResolvedType): ResolvedType {
  if (a.kind === "null" && b.kind === "null") return { kind: "null" };
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind === "int" && b.kind === "int") return { kind: "int", ty: promote(a, b) };
  if ((a.kind === "int" || a.kind === "decimal") && (b.kind === "int" || b.kind === "decimal")) {
    return { kind: "decimal" };
  }
  if (a.kind === "float" && b.kind === "float" && a.ty === b.ty) return a;
  if (
    a.kind === b.kind &&
    (a.kind === "text" ||
      a.kind === "bool" ||
      a.kind === "bytea" ||
      a.kind === "uuid" ||
      a.kind === "timestamp" ||
      a.kind === "timestamptz" ||
      a.kind === "interval" ||
      a.kind === "date")
  ) {
    return a;
  }
  throw engineError("datatype_mismatch", `VALUES types ${rtName(a)} and ${rtName(b)} cannot be matched`);
}

// scalarForParamHint is the scalar type to note a bind parameter at, given its VALUES column's
// unified type (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array
// column has no scalar parameter type, so null is returned and the parameter stays untyped (42P18 at
// finalize).
function scalarForParamHint(rt: ResolvedType): ScalarType | null {
  switch (rt.kind) {
    case "int":
    case "float":
      return rt.ty;
    case "bool":
      return "boolean";
    case "text":
      return "text";
    case "decimal":
      return "decimal";
    case "bytea":
      return "bytea";
    case "uuid":
      return "uuid";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    case "interval":
      return "interval";
    default:
      return null;
  }
}

function unifySetopColumn(a: ResolvedType, b: ResolvedType, op: SetOpKind): ResolvedType {
  if (a.kind === "null" && b.kind === "null") return { kind: "null" };
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind === "int" && b.kind === "int") return { kind: "int", ty: promote(a, b) };
  if ((a.kind === "int" || a.kind === "decimal") && (b.kind === "int" || b.kind === "decimal")) {
    // at least one decimal (both-int handled above) -> decimal
    return { kind: "decimal" };
  }
  // Two floats unify to the widest (the float tower — f32 + f64 → f64; the narrower
  // operand's rows are widened in coerceSetopRows). float never reconciles with int/decimal.
  if (a.kind === "float" && b.kind === "float") return { kind: "float", ty: promoteFloat(a.ty, b.ty) };
  if (a.kind === b.kind) return a;
  throw engineError(
    "datatype_mismatch",
    `${setopName(op)} types ${rtName(a)} and ${rtName(b)} cannot be matched`,
  );
}

// coerceSetopRows converts each row's values in place to the unified set-operation column types —
// the only runtime change is integer -> decimal (a NULL stays NULL; integer-width promotion is a
// value no-op since every integer is bigint). Same conversion coerceCaseValue uses for CASE.
function coerceSetopRows(rows: Value[][], from: ResolvedType[], to: ResolvedType[]): void {
  for (let i = 0; i < to.length; i++) {
    if (from[i]!.kind === "int" && to[i]!.kind === "decimal") {
      for (const row of rows) {
        const v = row[i]!;
        if (v.kind === "int") row[i] = decimalValue(Decimal.fromBigInt(v.int));
      }
    }
    // f32 → f64 widening (lossless): the column unified to f64 but this operand is
    // f32, so its values become f64 Values (the number is already an exact binary64).
    const t = to[i]!;
    if (from[i]!.kind === "float" && t.kind === "float" && t.ty === "f64") {
      for (const row of rows) {
        const v = row[i]!;
        if (v.kind === "f32") row[i] = float64Value(v.value);
      }
    }
  }
}

// combineSetop combines the operands' rows per the set operator + ALL flag (spec/design/grammar.md
// §25). Rows match by the NULL-safe, value-canonical distinctRowKey (two NULLs match, 1.5 == 1.50,
// and a converted int matches the decimal). The emitted representative for a matched / deduplicated
// key is its FIRST occurrence scanning the LEFT operand then the right, and emitted rows keep that
// left-then-right scan order — deterministic and identical across cores. (A later ORDER BY
// re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
function combineSetop(op: SetOpKind, all: boolean, left: Value[][], right: Value[][]): Value[][] {
  if (op === "union" && all) return left.concat(right);
  if (op === "union") {
    const seen = new Set<string>();
    const out: Value[][] = [];
    for (const row of left.concat(right)) {
      const k = distinctRowKey(row);
      if (!seen.has(k)) {
        seen.add(k);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "intersect" && all) {
    const counts = new Map<string, number>();
    for (const row of right) {
      const k = distinctRowKey(row);
      counts.set(k, (counts.get(k) ?? 0) + 1);
    }
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      const c = counts.get(k) ?? 0;
      if (c > 0) {
        counts.set(k, c - 1);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "intersect") {
    const rightSet = new Set<string>();
    for (const row of right) rightSet.add(distinctRowKey(row));
    const emitted = new Set<string>();
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      if (rightSet.has(k) && !emitted.has(k)) {
        emitted.add(k);
        out.push(row);
      }
    }
    return out;
  }
  if (op === "except" && all) {
    const counts = new Map<string, number>();
    for (const row of right) {
      const k = distinctRowKey(row);
      counts.set(k, (counts.get(k) ?? 0) + 1);
    }
    const out: Value[][] = [];
    for (const row of left) {
      const k = distinctRowKey(row);
      const c = counts.get(k) ?? 0;
      if (c > 0) counts.set(k, c - 1);
      else out.push(row);
    }
    return out;
  }
  // EXCEPT, distinct
  const rightSet = new Set<string>();
  for (const row of right) rightSet.add(distinctRowKey(row));
  const emitted = new Set<string>();
  const out: Value[][] = [];
  for (const row of left) {
    const k = distinctRowKey(row);
    if (!rightSet.has(k) && !emitted.has(k)) {
      emitted.add(k);
      out.push(row);
    }
  }
  return out;
}

// resolveSetopOrderKey resolves a trailing ORDER BY key for a set operation against the OUTPUT
// column names (the left operand's). A qualified key is 42P01 (no relation scope after a set
// operation); an unknown name is 42703. Returns the output column index.
function resolveSetopOrderKey(key: OrderKey, names: string[]): number {
  if (key.qualifier !== null) {
    throw engineError("undefined_table", "missing FROM-clause entry for table " + key.qualifier);
  }
  const idx = names.findIndex((n) => n.toLowerCase() === key.column.toLowerCase());
  if (idx < 0) throw engineError("undefined_column", "column " + key.column + " does not exist");
  return idx;
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
  // A float column accepts a float value of EQUAL OR NARROWER width (f32 → f64 widening is
  // implicit; f64 → f32 needs an explicit CAST — float.md §6) or NULL. No int/decimal.
  else if (isFloat(colTy)) ok = (t.kind === "float" && promoteFloat(t.ty, colTy) === colTy) || t.kind === "null";
  else if (isBool(colTy)) ok = t.kind === "bool" || t.kind === "null";
  else if (isBytea(colTy)) ok = t.kind === "bytea" || t.kind === "null";
  else if (isUuid(colTy)) ok = t.kind === "uuid" || t.kind === "null";
  else if (isTimestamp(colTy)) ok = t.kind === "timestamp" || t.kind === "null";
  else if (isTimestamptz(colTy)) ok = t.kind === "timestamptz" || t.kind === "null";
  else if (isInterval(colTy)) ok = t.kind === "interval" || t.kind === "null";
  else if (isDate(colTy)) ok = t.kind === "date" || t.kind === "null";
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
      // An integer LITERAL adapts to a float column (float.md §4 literal adaptation — INSERT VALUES /
      // DEFAULT bypass the expression resolver, so the adaptation lands here, like text→bytea). This
      // is literal adaptation, NOT an implicit cross-family cast of a value (storing a f64 into a
      // f32 is still rejected below). Out of binary range → 22003 (the finite-overflow rule).
      if (isFloat(colTy)) return makeFloat(colTy, Number(v.int));
      throw typeError("cannot store an integer value in " + canonicalName(colTy) + " column " + colName);
    case "decimal":
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(v.dec, typmod));
      // A decimal LITERAL adapts to a float column (float.md §4): nearest binary, fround for f32.
      if (isFloat(colTy)) {
        const d = Number(v.dec.render());
        if (!Number.isFinite(d)) throw overflow(colTy);
        return makeFloat(colTy, d);
      }
      throw typeError("cannot store a decimal value in " + canonicalName(colTy) + " column " + colName);
    case "f32":
      // f32 into f32 stores as-is; into f64 widens losslessly (every binary32 is an
      // exact binary64 — float.md §2). Bits (incl -0/NaN) preserved. No cross-family store.
      if (colTy === "f32") return v;
      if (colTy === "f64") return float64Value(v.value);
      throw typeError("cannot store a f32 value in " + canonicalName(colTy) + " column " + colName);
    case "f64":
      // f64 into f64 stores as-is. f64 → f32 is LOSSY (explicit cast required, not a
      // silent store) so it is rejected here (the resolver's assignableTo already gates it 42804).
      if (colTy === "f64") return v;
      throw typeError("cannot store a f64 value in " + canonicalName(colTy) + " column " + colName);
    case "text":
      if (isText(colTy)) return v;
      // A string literal adapts to a bytea column, decoding the hex input (types.md §6/§13);
      // malformed hex traps 22P02.
      if (isBytea(colTy)) return byteaValue(decodeByteaLiteral(v.text));
      // ... and to a uuid column via the PG-flexible uuid input (types.md §6/§14); 22P02 on bad input.
      if (isUuid(colTy)) return uuidValue(decodeUuidLiteral(v.text));
      // ... or to a timestamp column (spec/design/timestamp.md); bad input traps 22007/22008.
      if (isTimestamp(colTy)) return timestampValue(parseTimestamp(v.text));
      if (isTimestamptz(colTy)) return timestamptzValue(parseTimestamptz(v.text));
      if (isDate(colTy)) return dateValue(parseDate(v.text));
      // ... or to an interval column (spec/design/interval.md); bad input traps 22007/22008.
      if (isInterval(colTy)) return intervalValue(parseInterval(v.text));
      throw typeError("cannot store a text value in " + canonicalName(colTy) + " column " + colName);
    case "bytea":
      if (isBytea(colTy)) return v;
      throw typeError("cannot store a bytea value in " + canonicalName(colTy) + " column " + colName);
    case "uuid":
      if (isUuid(colTy)) return v;
      throw typeError("cannot store a uuid value in " + canonicalName(colTy) + " column " + colName);
    case "timestamp":
      if (isTimestamp(colTy)) return v;
      throw typeError("cannot store a timestamp value in " + canonicalName(colTy) + " column " + colName);
    case "timestamptz":
      if (isTimestamptz(colTy)) return v;
      throw typeError("cannot store a timestamptz value in " + canonicalName(colTy) + " column " + colName);
    case "date":
      if (isDate(colTy)) return v;
      throw typeError("cannot store a date value in " + canonicalName(colTy) + " column " + colName);
    case "interval":
      if (isInterval(colTy)) return v;
      throw typeError("cannot store an interval value in " + canonicalName(colTy) + " column " + colName);
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

// coerceForStore coerces a value into a column of resolved ColType (spec/design/composite.md §4):
// a scalar dispatches to storeValue; a composite to storeComposite. The single store-coercion seam
// the INSERT/UPDATE paths use.
function coerceForStore(v: Value, ty: ColType, typmod: DecimalTypmod | null, notNull: boolean, colName: string): Value {
  if (ty.kind === "scalar") return storeValue(v, ty.scalar, typmod, notNull, colName);
  if (ty.kind === "array") return storeArray(v, ty.elem, notNull, colName);
  return storeComposite(v, ty.name, ty.fields, notNull, colName);
}

// storeArray coerces a value into an ARRAY column (spec/design/array.md §4): NULL honours NOT NULL
// (23502); an array value coerces each element to the declared element type via coerceForStore (a
// NULL element is allowed — array elements are nullable). Any other value is a 42804.
function storeArray(v: Value, elem: ColType, notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw engineError("not_null_violation", "null value in column " + colName + " violates not-null constraint");
    }
    return nullValue();
  }
  if (v.kind !== "array") {
    throw typeError("cannot store a non-array value in array column " + colName);
  }
  // Elements are nullable; the element typmod is unconstrained this slice (numeric(p,s)[] deferred).
  // The shape (dims/lbounds) is preserved.
  const out = v.elements.map((el) => coerceForStore(el, elem, null, false, colName));
  return { kind: "array", dims: v.dims, lbounds: v.lbounds, elements: out };
}

// storeComposite coerces a value into a COMPOSITE column (spec/design/composite.md §4): NULL honours
// NOT NULL (23502); a composite value must have exactly the declared field count (42804) and each
// field is coerced to its declared field type via coerceForStore (recursing); any other value is a
// 42804. A NULL field of a NOT NULL composite field traps 23502.
function storeComposite(v: Value, typeName: string, fields: ColField[], notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw engineError("not_null_violation", "null value in column " + colName + " violates not-null constraint");
    }
    return nullValue();
  }
  if (v.kind !== "composite") {
    throw typeError("cannot store a non-record value in composite column " + colName + " (type " + typeName + ")");
  }
  if (v.fields.length !== fields.length) {
    throw typeError("row has " + v.fields.length + " fields but composite type " + typeName + " has " + fields.length);
  }
  const out: Value[] = new Array(fields.length);
  for (let i = 0; i < fields.length; i++) {
    const f = fields[i]!;
    out[i] = coerceForStore(v.fields[i]!, f.type, f.typmod, f.notNull, f.name);
  }
  return compositeValue(out);
}

// materializeInsertValue materializes one INSERT VALUES slot into a Value against the column's
// resolved ColType (spec/design/composite.md §1/§4): a scalar slot is a literal or a bound $N; a
// composite slot is a ROW(…) whose fields recurse against the composite's field types, or a bound
// $N. The result is then fully coerced/range-checked by coerceForStore. DEFAULT is handled by the
// caller at the top level (it is not a valid field inside a ROW(…)).
function materializeInsertValue(iv: InsertValue, ty: ColType, bound: Value[]): Value {
  if (ty.kind === "array") {
    switch (iv.kind) {
      case "array": {
        // ARRAY[e, …]: a nested constructor (an element is itself ARRAY[…]) stacks the sub-arrays
        // into a higher dimension (mirrors the evaluator's buildNestedArray, spec/design/array.md
        // §4); otherwise each element materializes against the element type into a flat 1-D array. A
        // scalar mixed with an array sub-element errors 42804 (materialized against the array type).
        if (iv.elements.some((el) => el.kind === "array")) {
          const subs = iv.elements.map((el) => materializeInsertValue(el, ty, bound));
          return buildNestedArray(subs);
        }
        const vals = iv.elements.map((el) => materializeInsertValue(el, ty.elem, bound));
        return arrayValue(vals);
      }
      case "param":
        return bound[iv.index - 1]!;
      case "row":
        throw typeError("cannot assign a record value to an array column");
      case "lit":
        // A bare string literal adapts to the array context via array_in (the same
        // string-adapts-to-context rule bytea/uuid use — spec/design/array.md §7).
        if (iv.lit.kind === "text") return coerceStringToArray(iv.lit.text, ty.elem);
        if (iv.lit.kind === "null") return nullValue();
        throw typeError("cannot assign a scalar value to an array column");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ARRAY[...]");
    }
  }
  if (ty.kind === "scalar") {
    switch (iv.kind) {
      case "lit":
        return literalToValue(iv.lit);
      case "param":
        return bound[iv.index - 1]!;
      case "row":
        throw typeError("cannot assign a record value to a " + canonicalName(ty.scalar) + " field");
      case "array":
        throw typeError("cannot assign an array value to a " + canonicalName(ty.scalar) + " field");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
    }
  }
  // ty is a composite column type.
  switch (iv.kind) {
    case "row": {
      if (iv.fields.length !== ty.fields.length) {
        throw typeError("ROW has " + iv.fields.length + " fields but composite type " + ty.name + " has " + ty.fields.length);
      }
      const vals: Value[] = new Array(ty.fields.length);
      for (let i = 0; i < ty.fields.length; i++) vals[i] = materializeInsertValue(iv.fields[i]!, ty.fields[i]!.type, bound);
      return compositeValue(vals);
    }
    case "param":
      return bound[iv.index - 1]!;
    case "lit":
      throw typeError("cannot assign a scalar value to composite column (type " + ty.name + ")");
    case "array":
      throw typeError("cannot assign an array value to composite column (type " + ty.name + ")");
    default: // default
      throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
  }
}

// coerceStringToArray parses a text array literal into an array Value against the element ColType
// via array_in (spec/design/array.md §7): each token is coerced to the element type (an unquoted
// NULL token → NULL element). A malformed literal is 22P02.
function coerceStringToArray(s: string, elem: ColType): Value {
  const parsed: ArrayInResult = parseArrayLiteral(s);
  if (!parsed.ok) {
    if (parsed.err === "boundflip") throw arraySubscriptErr("upper bound cannot be less than lower bound");
    throw engineError("invalid_text_representation", "malformed array literal");
  }
  const vals = parsed.value.tokens.map((tok) => {
    if (tok === null) return nullValue();
    // Coerce the token to the element type (a scalar via the string-literal coercion, a composite
    // via record_in — array-of-composite, spec/design/array.md §12 AC1 / §7).
    return coerceArrayElementText(tok, elem);
  });
  return { kind: "array", dims: parsed.value.dims, lbounds: parsed.value.lbounds, elements: vals };
}

// coerceArrayElementText coerces one array-element token to a Value against the element ColType (the
// array_in per-element step, spec/design/array.md §7): a scalar via the string-literal coercion, a
// composite via record_in (recursive — the array-of-composite quoting nests, §12 AC1). Self-contained
// over the resolved ColType (no catalog re-walk). A nested-array element would recurse, but
// array-of-array is not a jed type, so it is unreachable in v1.
function coerceArrayElementText(tok: string, elem: ColType): Value {
  if (elem.kind === "composite") return coerceRecordTextToValue(tok, elem);
  if (elem.kind === "array") return coerceStringToArray(tok, elem.elem);
  const { node } = coerceStringLiteral(tok, elem.scalar, null);
  return rexprConstToValue(node);
}

// coerceRecordTextToValue is record_in over a self-contained composite ColType (the inverse of
// record_out): the token is the composite's own `(f1,f2,…)` text, tokenized by the shared
// parseRecordTokens and recursively coerced per field (a scalar field respects its decimal typmod).
// Mirrors coerceStringToComposite but produces a Value directly and walks ColType (no Database). A
// bad shape / field count is 22P02.
function coerceRecordTextToValue(text: string, ct: { kind: "composite"; name: string; fields: ColField[] }): Value {
  const tokens = parseRecordTokens(text);
  if (tokens === null || tokens.length !== ct.fields.length) {
    throw engineError("invalid_text_representation", `malformed record literal: "${text}" for type ${ct.name}`);
  }
  const vals = tokens.map((tok, i) => {
    if (tok === null) return nullValue();
    const f = ct.fields[i]!;
    if (f.type.kind === "composite") return coerceRecordTextToValue(tok, f.type);
    if (f.type.kind === "array") return coerceStringToArray(tok, f.type.elem);
    const { node } = coerceStringLiteral(tok, f.type.scalar, f.typmod);
    return rexprConstToValue(node);
  });
  return compositeValue(vals);
}

// rexprConstToValue extracts the Value from a constant RExpr (the const nodes coerceStringLiteral
// produces).
function rexprConstToValue(e: RExpr): Value {
  switch (e.kind) {
    case "constNull":
      return nullValue();
    case "constInt":
      return intValue(e.value);
    case "constBool":
      return boolValue(e.value);
    case "constText":
      return textValue(e.value);
    case "constDecimal":
      return decimalValue(e.value);
    case "constBytea":
      return byteaValue(e.value);
    case "constUuid":
      return uuidValue(e.value);
    case "constTimestamp":
      return timestampValue(e.value);
    case "constTimestamptz":
      return timestamptzValue(e.value);
    case "constDate":
      return dateValue(e.value);
    case "constInterval":
      return intervalValue(e.value);
    case "constFloat":
      return e.ty === "f32" ? float32Value(e.value) : float64Value(e.value);
    default:
      throw typeError("non-constant array element literal");
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
// inMembership is three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging
// one operator_eval per element compared. An EMPTY list is `negated` (x IN () = FALSE, x NOT IN ()
// = TRUE) independent of lv. Otherwise: a positive match -> TRUE; else a NULL element (or NULL lv)
// -> NULL; else FALSE. NOT IN is the Kleene negation. Shared by the folded "inValues" node and the
// correlated "subquery"/in eval.
function inMembership(lv: Value, list: Value[], negated: boolean, m: Meter): Value {
  if (list.length === 0) return { kind: "bool", value: negated };
  let anyMatch = false;
  let anyNull = false;
  for (const v of list) {
    m.charge(COSTS.operatorEval);
    // Each element comparison over a decimal pair charges its size-scaled decimal_work
    // (spec/design/cost.md §3 "decimal_work"), like a compare node.
    m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(lv, v) - 1));
    m.guard();
    const t = eq3(lv, v);
    if (t === "true") anyMatch = true;
    else if (t === "unknown") anyNull = true;
  }
  const inVal: Value = anyMatch
    ? { kind: "bool", value: true }
    : anyNull
      ? nullValue()
      : { kind: "bool", value: false };
  return negated ? boolNot(inVal) : inVal;
}

function evalExpr(e: RExpr, row: Row, env: EvalEnv, m: Meter): Value {
  // Enforce the cost ceiling before evaluating this node (CLAUDE.md §13). evalExpr recurses once
  // per expression node, so guarding here bounds a pathological expression to ~O(1) overshoot; it
  // is a no-op when no ceiling is set (spec/design/cost.md §6).
  m.guard();
  switch (e.kind) {
    case "column":
      return row[e.index]!;
    case "outerColumn":
      // A correlated reference: column `index` of the enclosing row `level` hops out (§26).
      return env.outer[env.outer.length - e.level]![e.index]!;
    case "param":
      // The supplied value, already coerced to its inferred type by bindParams before execution
      // (spec/design/api.md §5).
      return env.params[e.index]!;
    case "constInt":
      return intValue(e.value);
    case "constFloat":
      // The value was already width-rounded at resolve (f32 frounded); rebuild the Value.
      return e.ty === "f32" ? float32Value(e.value) : float64Value(e.value);
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
    case "constTimestamp":
      return timestampValue(e.value);
    case "constTimestamptz":
      return timestamptzValue(e.value);
    case "constDate":
      return dateValue(e.value);
    case "constInterval":
      return intervalValue(e.value);
    case "constNull":
      return nullValue();
    case "row": {
      // A ROW(...) constructor — one operator_eval, then build the composite from the evaluated
      // fields (spec/design/composite.md §1, cost.md §9).
      m.charge(COSTS.operatorEval);
      const vals: Value[] = new Array(e.fields.length);
      for (let i = 0; i < e.fields.length; i++) vals[i] = evalExpr(e.fields[i]!, row, env, m);
      return compositeValue(vals);
    }
    case "array": {
      // An ARRAY[...] constructor — one operator_eval. A `nested` constructor stacks its sub-arrays
      // into one higher dimension (spec/design/array.md §4); otherwise a flat 1-D array.
      m.charge(COSTS.operatorEval);
      const elems: Value[] = new Array(e.elements.length);
      for (let i = 0; i < e.elements.length; i++) elems[i] = evalExpr(e.elements[i]!, row, env, m);
      return e.nested ? buildNestedArray(elems) : arrayValue(elems);
    }
    case "constArray":
      // A folded array constant (shape preserved) — return it directly.
      return e.value;
    case "field": {
      // Field selection — one operator_eval, then pull the resolved field ordinal out of the
      // evaluated composite. A whole-value-NULL composite yields NULL (PG); the index is in range
      // by construction (resolve fixed it against the static field list).
      m.charge(COSTS.operatorEval);
      const base = evalExpr(e.base, row, env, m);
      if (base.kind === "null") return nullValue();
      if (base.kind !== "composite") throw typeError("internal: field access on a non-composite value");
      return base.fields[e.index]!;
    }
    case "subscript": {
      // Array subscript `base[..][..]` — one operator_eval (spec/design/array.md §6). A NULL array
      // or any NULL subscript bound yields NULL; element access returns the element (or NULL), slice
      // access a (renumbered) sub-array. The per-element walk is internal (unmetered).
      m.charge(COSTS.operatorEval);
      return evalSubscript(e, row, env, m);
    }
    case "cast": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue();
      return evalCast(v, e.target, e.typmod);
    }
    case "neg": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.operand, row, env, m);
      if (v.kind === "null") return nullValue();
      if (isInterval(e.result)) {
        if (v.kind !== "interval") throw typeError("internal: non-interval unary minus");
        return intervalValue(intervalNeg(v.iv));
      }
      if (isDecimal(e.result)) {
        return decimalValue((v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec).negate());
      }
      if (isFloat(e.result)) {
        // Negation flips the sign (no overflow); -NaN is NaN, -Inf is -Inf per IEEE. f32 stays
        // binary32 (negation never changes the width's representability, but fround keeps the path
        // uniform). float.md §5.
        if (v.kind !== "f32" && v.kind !== "f64") throw typeError("internal: non-float unary minus");
        return e.result === "f32" ? float32Value(-v.value) : float64Value(-v.value);
      }
      if (v.kind !== "int") throw typeError("internal: boolean unary minus");
      const n = -v.int;
      if (!inRange(e.result, n)) throw overflow(e.result);
      return intValue(n);
    }
    case "not": {
      m.charge(COSTS.operatorEval);
      return boolNot(evalExpr(e.operand, row, env, m));
    }
    case "arith": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      if (a.kind === "null" || b.kind === "null") return nullValue();
      if (isInterval(e.result) && (e.op === "mul" || e.op === "div")) {
        // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Mul
        // commutes; Div is interval / number. A zero divisor traps 22012.
        const ivVal = a.kind === "interval" ? a : (b as { iv: Interval });
        const numVal = a.kind === "interval" ? b : a;
        let [fnum, fden] = factorToFraction(numVal);
        if (e.op === "div") {
          if (fnum === 0n) throw engineError("division_by_zero", "division by zero");
          // interval / number = interval * (den/num); keep fden > 0.
          [fnum, fden] = fnum < 0n ? [-fden, -fnum] : [fden, fnum];
        }
        return intervalValue(mulByFraction(ivVal.iv, fnum, fden));
      }
      if (isInterval(e.result)) {
        // interval ± interval → interval; timestamp[tz] − timestamp[tz] → interval (§5).
        if (a.kind === "interval" && b.kind === "interval") {
          return intervalValue(e.op === "add" ? intervalAdd(a.iv, b.iv) : intervalSub(a.iv, b.iv));
        }
        if (
          (a.kind === "timestamp" && b.kind === "timestamp") ||
          (a.kind === "timestamptz" && b.kind === "timestamptz")
        ) {
          return intervalValue(tsDiff(a.micros, b.micros));
        }
        throw typeError("internal: bad temporal-difference operands");
      }
      if (isTimestamp(e.result) || isTimestamptz(e.result)) {
        // timestamp[tz] ± interval → timestamp[tz] (calendar month-add; interval + ts commutes).
        let instant: bigint;
        let iv: Interval;
        if (a.kind === "interval") {
          iv = a.iv;
          instant = (b as { micros: bigint }).micros;
        } else {
          instant = (a as { micros: bigint }).micros;
          iv = (b as { iv: Interval }).iv;
        }
        const r = tsShift(instant, iv, e.op === "sub");
        return isTimestamptz(e.result) ? timestamptzValue(r) : timestampValue(r);
      }
      if (isDecimal(e.result)) {
        // Decimal arithmetic: widen any integer operand to decimal, then apply the op with
        // PG's scale rules (spec/design/decimal.md §4). The size-scaled decimal_work is
        // charged BEFORE the operation runs, so a cost ceiling aborts ahead of the limb
        // work (spec/design/cost.md §3 "decimal_work").
        const da = toDecimal(a);
        const db = toDecimal(b);
        m.charge(COSTS.decimalWork * BigInt(decimalArithWork(e.op, da, db) - 1));
        m.guard();
        return decimalValue(evalDecimalArith(e.op, da, db));
      }
      if (isFloat(e.result)) {
        // Float arithmetic: the resolver promoted both operands to e.result's width (mixed-width
        // pairs were cast to f64), so both are the same float kind here. One IEEE op per node
        // (no FMA fusion — structural in the tree walker, float.md §5).
        if ((a.kind !== "f32" && a.kind !== "f64") || (b.kind !== "f32" && b.kind !== "f64")) {
          throw typeError("internal: non-float arithmetic");
        }
        return evalFloatArith(e.op, a.value, b.value, e.result);
      }
      if (a.kind !== "int" || b.kind !== "int") throw typeError("internal: non-integer arithmetic");
      return evalArith(e.op, a.int, b.int, e.result);
    }
    case "compare": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      // A decimal(-promotable) pair charges size-scaled decimal_work — once per node, even
      // where <=/>= decompose internally (spec/design/cost.md §3 "decimal_work").
      m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(a, b) - 1));
      m.guard();
      switch (e.op) {
        case "eq":
          return from3(eq3(a, b));
        case "ne":
          return from3(not3(eq3(a, b)));
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
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      return boolAnd(a, b);
    }
    case "or": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      return boolOr(a, b);
    }
    case "isNull": {
      m.charge(COSTS.operatorEval);
      // PG's `IS [NOT] NULL` (spec/design/composite.md §5): for a composite the two are NOT
      // negations but the all-fields rule (one level deep, not recursive); a scalar follows the
      // ordinary rule. isNullTest folds both cases. Replaces the old `(v is null) !== negated`.
      const operand = evalExpr(e.operand, row, env, m);
      return { kind: "bool", value: isNullTest(operand, e.negated) };
    }
    case "distinct": {
      m.charge(COSTS.operatorEval);
      const dl = evalExpr(e.lhs, row, env, m);
      const dr = evalExpr(e.rhs, row, env, m);
      // IS [NOT] DISTINCT FROM is a comparison: a decimal pair charges its size-scaled
      // decimal_work like "compare" (spec/design/cost.md §3 "decimal_work").
      m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(dl, dr) - 1));
      m.guard();
      const same = notDistinctFrom(dl, dr);
      // negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
      // the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
      // unknown (the null_safe discipline, functions.md §3).
      return { kind: "bool", value: same === e.negated };
    }
    case "like": {
      m.charge(COSTS.operatorEval);
      const subject = evalExpr(e.lhs, row, env, m);
      const pattern = evalExpr(e.rhs, row, env, m);
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
        const cv = evalExpr(arm.cond, row, env, m);
        if (cv.kind === "bool" && cv.value) {
          return coerceCaseValue(evalExpr(arm.result, row, env, m), e.coerceDecimal);
        }
      }
      return coerceCaseValue(evalExpr(e.els, row, env, m), e.coerceDecimal);
    }
    case "scalarFunc": {
      // One operator_eval per call (the uniform weight); arguments charge their own.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") return nullValue(); // NULL propagates
        vals.push(v);
      }
      if (e.func === "make_interval") {
        // make_interval — six integer components plus the f64 secs. years/months → months
        // field (×12), weeks/days → days field (×7), hours/mins/secs → micros; an i32/i64 field
        // overflow traps 22008 (functions.md §11). The one float step (secs → micros) is
        // correctly-rounded + deterministic, so the interval is in-contract. A f32 secs reads
        // as its exact f64 value (.value holds the binary64 of either width).
        const geti = (k: number): bigint => (vals[k] as { int: bigint }).int;
        const secMicros = f64ToMicros((vals[6] as { value: number }).value);
        return intervalValue(
          makeInterval(geti(0), geti(1), geti(2), geti(3), geti(4), geti(5), secMicros),
        );
      }
      // uuid extractors (spec/design/functions.md §12): pure bit inspection; NULL for a non-RFC
      // variant (and, for the timestamp, any version other than 1/7). The NULL-input case is
      // already handled above.
      if (e.func === "uuid_extract_version") {
        const ver = uuidExtractVersion((vals[0] as { bytes: Uint8Array }).bytes);
        return ver === null ? nullValue() : intValue(ver);
      }
      if (e.func === "uuid_extract_timestamp") {
        const mc = uuidExtractTimestampMicros((vals[0] as { bytes: Uint8Array }).bytes);
        return mc === null ? nullValue() : timestamptzValue(mc);
      }
      // uuid generators (spec/design/entropy.md §3): draw from the per-statement seam (env.rng),
      // advancing the PRNG/counter. The NULL-arg case is handled above.
      if (e.func === "uuidv4") {
        return uuidValue(env.rng.uuidV4(env.seam));
      }
      if (e.func === "uuidv7") {
        const clock = env.rng.statementClockMicros(env.seam);
        // The optional interval arg shifts the embedded instant via the existing calendar-aware
        // timestamptz arithmetic (entropy.md §4).
        const shifted = vals.length === 1 ? tsShift(clock, (vals[0] as { iv: Interval }).iv, false) : clock;
        return uuidValue(env.rng.uuidV7(env.seam, shifted));
      }
      // current-time functions (spec/design/entropy.md §5): now() reads the statement clock ONCE and
      // reuses it (STABLE); clock_timestamp() reads the seam on every call (VOLATILE). Both return
      // the seam's micros directly as timestamptz.
      if (e.func === "now") {
        return timestamptzValue(env.rng.statementClockMicros(env.seam));
      }
      if (e.func === "clock_timestamp") {
        return timestamptzValue(env.rng.clockNowMicros(env.seam));
      }
      // Sequence value functions (spec/design/sequences.md §4/§6). nextval charges an additional
      // sequence_advance unit (the catalog-tuple read+rewrite) and mutates the per-statement pending
      // state; currval is a pure session-state read. The NULL-arg case is handled above (propagates).
      if (e.func === "nextval") {
        m.charge(COSTS.sequenceAdvance);
        return intValue(env.exec.seqNextval((vals[0] as { text: string }).text));
      }
      if (e.func === "currval") {
        return intValue(env.exec.seqCurrval((vals[0] as { text: string }).text));
      }
      // setval charges sequence_advance (it rewrites the catalog tuple, like nextval). Arity 2 →
      // isCalled defaults true; arity 3 → the boolean third argument.
      if (e.func === "setval") {
        m.charge(COSTS.sequenceAdvance);
        const isCalled = vals.length > 2 ? (vals[2] as { value: boolean }).value : true;
        return intValue(
          env.exec.seqSetval((vals[0] as { text: string }).text, (vals[1] as { int: bigint }).int, isCalled),
        );
      }
      if (e.func === "lastval") {
        return intValue(env.exec.seqLastval());
      }
      const v0 = vals[0];
      // Float scalar functions (float.md §8): dispatch on the operand being a float value. Per the
      // catalog, only abs is operand-typed (result "promoted"); every other float func returns
      // f64 (result "f64") — so the result Value's width is e.result, not argWidth. The
      // computation is done in binary64; abs frounds for a f32 result via e.result.
      if (v0.kind === "f32" || v0.kind === "f64") {
        if (e.func === "pow") {
          // pow(x, y): both operands are float (promoted to one width at resolve); result f64.
          const v1 = vals[1] as { value: number };
          return evalFloatPow(v0.value, v1.value, e.result);
        }
        // round(x, n): n is an int operand; the unary funcs ignore it.
        const places = vals.length > 1 ? Number((vals[1] as { int: bigint }).int) : 0;
        return evalFloatFunc(e.func, v0.value, places, e.result);
      }
      if (e.func === "abs") {
        if (v0.kind === "int") {
          // abs over an integer: |x| then range-check at the result type's boundary
          // (abs(i16 -32768) → 22003), exactly like neg.
          let n = v0.int;
          if (n < 0n) n = -n;
          if (!inRange(e.result, n)) throw overflow(e.result);
          return intValue(n);
        }
        return decimalValue((v0 as { dec: Decimal }).dec.abs());
      }
      // round
      const d = v0.kind === "int" ? Decimal.fromBigInt(v0.int) : (v0 as { dec: Decimal }).dec;
      const places = vals.length > 1 ? Number((vals[1] as { int: bigint }).int) : 0;
      return decimalValue(d.roundPlaces(places));
    }
    case "arrayFunc": {
      // A polymorphic array function (spec/design/array-functions.md §3). One operator_eval per call;
      // arguments charge their own. NULL handling is per-kernel (the introspectors propagate, the
      // builders are non-strict), so — unlike "scalarFunc" — there is no blanket NULL short-circuit.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalArrayFunc(e.func, vals);
    }
    case "variadic": {
      // A VARIADIC argument-counting call (spec/design/array-functions.md §12). One operator_eval
      // (the per-element/arg count walk is unmetered, like the array introspectors §3.3); arguments
      // charge their own. Non-strict — no blanket NULL short-circuit. The two forms differ: the
      // spread form counts the args' null-ness (never NULL); the VARIADIC-array form returns NULL on
      // a NULL whole-array, else counts the array's flattened elements' null-ness.
      m.charge(COSTS.operatorEval);
      const wantNulls = e.func === "num_nulls";
      if (e.arrayForm) {
        const v = evalExpr(e.args[0]!, row, env, m);
        if (v.kind === "null") return nullValue();
        if (v.kind !== "array") throw new Error("resolver restricts a VARIADIC operand to an array");
        return intValue(BigInt(countNulls(v.elements, wantNulls)));
      }
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return intValue(BigInt(countNulls(vals, wantNulls)));
    }
    case "subquery": {
      // A correlated subquery (spec/design/grammar.md §26): re-executed once per outer row. Push
      // the current row onto the outer-row stack, run the inner plan, fold its accrued cost into
      // this meter, plus one operator_eval for the node.
      m.charge(COSTS.operatorEval);
      const r = env.runSubquery(e.plan, [...env.outer, row]);
      m.charge(r.cost);
      if (e.subKind === "scalar") {
        if (r.rows.length > 1) {
          throw engineError(
            "cardinality_violation",
            "more than one row returned by a subquery used as an expression",
          );
        }
        // 0 rows -> NULL (the static type was settled at resolve).
        return r.rows.length === 0 ? nullValue() : r.rows[0]![0]!;
      }
      if (e.subKind === "exists") {
        // EXISTS ignores the select list entirely and is never NULL.
        return { kind: "bool", value: r.rows.length > 0 !== e.negated };
      }
      if (e.subKind === "quantified") {
        // A correlated quantified subquery (array-functions.md §11.6): gather the body's single
        // column into an array and run the SAME 3VL fold as the array form.
        const lv = evalExpr(e.lhs!, row, env, m);
        const elements = r.rows.map((rr) => rr[0]!);
        return quantifiedMembership(e.op!, e.all!, lv, arrayValue(elements), m);
      }
      // in
      const lv = evalExpr(e.lhs!, row, env, m);
      const list = r.rows.map((rr) => rr[0]!);
      return inMembership(lv, list, e.negated, m);
    }
    case "inValues": {
      // A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
      m.charge(COSTS.operatorEval);
      const lv = evalExpr(e.lhs, row, env, m);
      return inMembership(lv, e.list, e.negated, m);
    }
    case "quantified": {
      // A quantified array comparison `lhs op ANY/ALL(array)` (array-functions.md §11) — the array
      // spelling of IN, the 3VL fold over the array's flattened elements.
      m.charge(COSTS.operatorEval);
      const lv = evalExpr(e.lhs, row, env, m);
      const av = evalExpr(e.array, row, env, m);
      return quantifiedMembership(e.op, e.all, lv, av, m);
    }
  }
}

// quantifiedMembership is the three-valued membership fold for `lhs op ANY/ALL(array)`
// (array-functions.md §11), the generalization of inMembership to all five comparison operators and
// both quantifiers. A NULL array -> NULL; otherwise, over the flattened elements, ANY/SOME (all=false)
// is the OR-fold (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty ->
// FALSE) and ALL (all=true) the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE;
// empty -> TRUE). Each element comparison charges one operator_eval (+ size-scaled decimal_work),
// exactly like inMembership, so max_cost bounds the walk (54P01).
function quantifiedMembership(op: BinaryOp, all: boolean, lv: Value, av: Value, m: Meter): Value {
  if (av.kind === "null") return nullValue();
  if (av.kind !== "array") throw new Error("BUG: the resolver requires an array right operand");
  let anyNull = false;
  for (const e of av.elements) {
    m.charge(COSTS.operatorEval);
    m.charge(COSTS.decimalWork * BigInt(decimalCmpWork(lv, e) - 1));
    m.guard();
    const t = quantifiedCmp3(op, lv, e);
    if (t === "true") {
      // ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
      if (!all) return { kind: "bool", value: true };
    } else if (t === "false") {
      // ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
      if (all) return { kind: "bool", value: false };
    } else {
      anyNull = true;
    }
  }
  // Drained without a short-circuit: a NULL seen -> UNKNOWN; else the quantifier's identity (ALL ->
  // TRUE, ANY -> FALSE — also the empty-array result).
  return anyNull ? nullValue() : { kind: "bool", value: all };
}

// quantifiedCmp3 is the per-element three-valued comparison `lhs op e` for a quantified node,
// normalizing a mixed-width float pair to f64 first (the resolver admits f32 vs f64,
// matching the compare node's promote — here the array elements are runtime values, so the widen
// happens per element). Bottoms out in the value module's eq3/lt3/gt3 kernels.
//
// A composite operand pair routes through the composite TOTAL ORDER (valueCmp), NOT the bare-ROW 3VL
// eq3/lt3/gt3 (array-functions.md §13): PostgreSQL's = ANY(addr[]) dispatches on the composite =
// operator = record_eq, which is DEFINITE with NULL fields comparable (ROW('a',NULL)::addr =
// ANY(ARRAY[ROW('a',NULL)::addr]) is TRUE), the same total order array_eq / @> already use for
// composite elements (array.md §5). A whole-element NULL is still UNKNOWN — the operator stays strict
// at the value level — so the resolver-guaranteed same-type pair is composite-vs-composite or
// composite-vs-NULL.
function quantifiedCmp3(op: BinaryOp, x: Value, e: Value): ThreeValued {
  if (x.kind === "composite" || e.kind === "composite") {
    if (x.kind === "null" || e.kind === "null") return "unknown";
    const ord = valueCmp(x, e);
    let matched: boolean;
    switch (op) {
      case "eq":
        matched = ord === 0;
        break;
      case "ne":
        matched = ord !== 0;
        break;
      case "lt":
        matched = ord < 0;
        break;
      case "gt":
        matched = ord > 0;
        break;
      case "le":
        matched = ord <= 0;
        break;
      default: // ge
        matched = ord >= 0;
    }
    return matched ? "true" : "false";
  }
  if (x.kind === "f32" && e.kind === "f64") x = float64Value(x.value);
  else if (x.kind === "f64" && e.kind === "f32") e = float64Value(e.value);
  switch (op) {
    case "eq":
      return eq3(x, e);
    case "ne":
      return not3(eq3(x, e));
    case "lt":
      return lt3(x, e);
    case "gt":
      return gt3(x, e);
    case "le":
      return or3(lt3(x, e), eq3(x, e));
    default: // ge
      return or3(gt3(x, e), eq3(x, e));
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
// and 22003 if the result falls outside the declared result type (the i16+i16 →
// i16 boundary — spec/design/functions.md §7). The MinInt64/-1 cases trap to match the
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
      // `x % -1` is mathematically 0 for every x; bigint computes it as 0n exactly (no
      // overflow). Unlike division, modulo by -1 has no out-of-range result, so it does NOT
      // trap — matching PostgreSQL and the i16/i32 widths (spec/design/types.md §3).
      v = x % y; // bigint remainder takes the dividend's sign
      break;
  }
  if (!inRange(result, v)) throw overflow(result);
  return intValue(v);
}

// evalFloatArith computes one IEEE float operation (float.md §5). The trap model (float.md §3):
//   - x / 0 and x % 0 trap 22012 (division_by_zero) for EVERY numerator except NaN — Inf/0 and 0/0
//     trap, only NaN/0 propagates to NaN (matching PG);
//   - a FINITE op whose true result overflows the float range to ±Inf traps 22003 (e.g. 1e308*10);
//   - an Inf/NaN OPERAND otherwise propagates by IEEE (Inf+1=Inf, Inf-Inf=NaN, NaN*0=NaN) — no trap.
// For f32 every result is Math.fround'd (true binary32 rounding — the TS-specific discipline);
// the overflow check is then re-applied because fround can push a finite double past binary32 range.
// `%` is IEEE remainder via JS `%` (which is fmod — truncated, dividend's sign), exact, never
// overflows.
function evalFloatArith(op: BinaryOp, x: number, y: number, result: ScalarType): Value {
  const f32 = result === "f32";
  const finiteInputs = Number.isFinite(x) && Number.isFinite(y);
  let r: number;
  switch (op) {
    case "add":
      r = x + y;
      break;
    case "sub":
      r = x - y;
      break;
    case "mul":
      r = x * y;
      break;
    case "div":
      // x / 0 traps for every numerator except NaN, which propagates (NaN/0 = NaN, matching PG).
      if (y === 0 && !Number.isNaN(x)) throw engineError("division_by_zero", "division by zero");
      r = x / y;
      break;
    default: // "mod"
      if (y === 0 && !Number.isNaN(x)) throw engineError("division_by_zero", "division by zero");
      r = x % y; // JS % is fmod: truncated, takes the dividend's sign; exact, finite for finite x,y
      break;
  }
  if (f32) r = Math.fround(r);
  // A finite-operand op that produced a non-finite result overflowed the (binary32 after fround, or
  // binary64) range → trap 22003. An Inf/NaN that came FROM an operand propagates and is NOT a trap.
  if (finiteInputs && !Number.isFinite(r)) throw overflow(result);
  return f32 ? float32Value(r) : float64Value(r);
}

// evalFloatFunc evaluates a unary float scalar function (float.md §8) over a float value `x`,
// producing a value of width `result` (always f64 here except abs, whose result is the operand
// width). `places` is the second argument of round(x, n) (ignored by the others). An Inf/NaN operand
// propagates through the exact functions; the transcendentals call native Math.* (exempted — the R
// tag absorbs cross-core ULP differences). Domain / overflow errors trap (float.md §8):
//   sqrt(neg) → 22003; ln(0)/ln(neg) → 22003; exp overflow → 22003; sin/cos/tan never trap.
function evalFloatFunc(func: ScalarFuncName, x: number, places: number, result: ScalarType): Value {
  const out = (r: number): Value => {
    // result is f64 for all but abs; abs's result is the operand width, so fround for f32.
    if (result === "f32") {
      const f = Math.fround(r);
      // abs cannot overflow (|finite| stays finite at the same width); a NaN/Inf propagates.
      return float32Value(f);
    }
    return float64Value(r);
  };
  switch (func) {
    case "abs":
      return out(Math.abs(x)); // |NaN| = NaN, |±Inf| = +Inf — propagation, no trap
    case "ceil":
      return out(Math.ceil(x));
    case "floor":
      return out(Math.floor(x));
    case "trunc":
      return out(Math.trunc(x));
    case "round":
      return out(roundFloatHalfAway(x, places));
    case "sqrt":
      // sqrt(neg) is a DOMAIN error → 22003 (NaN stays input-only). sqrt(NaN)=NaN, sqrt(+Inf)=+Inf,
      // sqrt(-0)=-0 all propagate. IEEE mandates sqrt correctly-rounded, so it is in-contract.
      if (x < 0) throw engineError("numeric_value_out_of_range", "cannot take square root of a negative number");
      return out(Math.sqrt(x));
    case "exp": {
      // exp overflow (e.g. exp(710)) → 22003. A NaN/±Inf operand propagates (exp(+Inf)=+Inf,
      // exp(-Inf)=0, exp(NaN)=NaN). Transcendental — exempted (R tag).
      const r = Math.exp(x);
      if (Number.isFinite(x) && !Number.isFinite(r)) throw overflow(result);
      return out(r);
    }
    case "ln":
      // ln(0) → 22003; ln(neg) → 22003 (domain). ln(+Inf)=+Inf, ln(NaN)=NaN propagate.
      if (x === 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of zero");
      if (x < 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of a negative number");
      return out(Math.log(x));
    case "log10":
      if (x === 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of zero");
      if (x < 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of a negative number");
      return out(Math.log10(x));
    case "sin":
      return out(Math.sin(x));
    case "cos":
      return out(Math.cos(x));
    case "tan":
      return out(Math.tan(x));
    default:
      throw typeError("internal: unsupported float scalar function " + func);
  }
}

// evalFloatPow evaluates pow(x, y) → f64 (float.md §8): native Math.pow (transcendental,
// exempted), trapping 22003 on a finite-input overflow to ±Inf (e.g. pow(10, 400)); a NaN/±Inf
// operand propagates per IEEE. result is f64 (the catalog), so no fround.
function evalFloatPow(x: number, y: number, result: ScalarType): Value {
  const r = Math.pow(x, y);
  if (Number.isFinite(x) && Number.isFinite(y) && !Number.isFinite(r)) throw overflow(result);
  return result === "f32" ? float32Value(Math.fround(r)) : float64Value(r);
}

// roundFloatHalfAway rounds a float to `places` decimal places, HALF AWAY FROM ZERO (jed's one
// mode — float.md §8). For an Inf/NaN it returns the value unchanged (propagation). It scales by
// 10^places, rounds half-away (negatives by magnitude — Math.round is half-UP, wrong for ties), then
// unscales. Done in binary64; the caller frounds for a f32 result of round (catalog result is
// f64, so in practice no fround). Note: this is approximate at the binary level (the scale
// factor is not exactly representable) — acceptable since float rounding is in the R-tag surface.
function roundFloatHalfAway(x: number, places: number): number {
  if (!Number.isFinite(x)) return x;
  const f = 10 ** places;
  const scaled = x * f;
  const r = scaled < 0 ? -Math.round(-scaled) : Math.round(scaled);
  return r / f;
}

// evalCast evaluates a (non-NULL) CAST to target. int→int range-checks (22003); int→decimal
// widens then coerces to the typmod; decimal→int rounds half-away to scale 0 then range-checks
// (22003); decimal→decimal re-scales to the typmod (spec/design/decimal.md §6).
function evalCast(v: Value, target: ScalarType, typmod: DecimalTypmod | null): Value {
  if (v.kind === "int") {
    if (isDecimal(target)) return decimalValue(coerceDecimal(Decimal.fromBigInt(v.int), typmod));
    // int → float (explicit, lossy): nearest binary representable, then fround for f32. Exact
    // for |int| ≤ 2^53; a larger i64 may round. Never traps (float.md §6).
    if (isFloat(target)) return makeFloat(target, Number(v.int));
    if (!inRange(target, v.int)) throw overflow(target);
    return intValue(v.int);
  }
  if (v.kind === "decimal") {
    if (isDecimal(target)) return decimalValue(coerceDecimal(v.dec, typmod));
    // decimal → float (explicit, lossy): nearest binary to the exact decimal (Number of the
    // canonical decimal string is the IEEE conversion). A huge decimal → ±Inf traps 22003 rather
    // than yielding Infinity (the finite-overflow rule, float.md §6).
    if (isFloat(target)) {
      const d = Number(v.dec.render());
      if (!Number.isFinite(d)) throw overflow(target);
      return makeFloat(target, d);
    }
    const n = v.dec.toBigIntRound();
    if (n === null || !inRange(target, n)) throw overflow(target);
    return intValue(n);
  }
  if (v.kind === "f32" || v.kind === "f64") {
    // float → float (the tower): f32 → f64 lossless (widen); f64 → f32 frounds
    // (lossy), trapping 22003 if a finite double rounds beyond binary32 range. float→float never
    // converts a NaN/±Inf to an error — those are first-class values that propagate (float.md §6).
    if (isFloat(target)) return makeFloatCast(target, v.value);
    // float → int (explicit): round HALF AWAY FROM ZERO to an integer, range-check (22003). NaN/
    // ±Inf → 22003 (NaN stays input-only — a float never becomes a NaN integer; float.md §6). A
    // documented PG divergence (PG rounds half-to-even; jed keeps one engine-wide mode).
    if (isInteger(target)) {
      if (!Number.isFinite(v.value)) throw overflow(target);
      const n = floatToIntHalfAway(v.value);
      if (!inRange(target, n)) throw overflow(target);
      return intValue(n);
    }
    // float → decimal (explicit): the EXACT decimal of the binary value (float.md §6 — the unique
    // exact value of the IEEE float, NOT Number#toString's shortest round-trip, which would diverge
    // cross-core), then the typmod's scale coercion. NaN/±Inf → 22003 (decimal is finite).
    if (isDecimal(target)) {
      if (!Number.isFinite(v.value)) throw overflow(target);
      const exact =
        v.kind === "f32" ? Decimal.exactFromFloat32(v.value) : Decimal.exactFromFloat64(v.value);
      return decimalValue(coerceDecimal(exact, typmod));
    }
    throw typeError("internal: unsupported float cast target");
  }
  throw typeError("internal: non-numeric cast operand");
}

// makeFloat builds a float Value at `ty`, trapping 22003 if a finite-source value rounds to ±Inf
// (the finite-overflow rule; the source here is already finite — only f32 rounding can push a
// finite double beyond binary32 range). Used by int/decimal → float.
function makeFloat(ty: ScalarType, n: number): Value {
  const r = ty === "f32" ? Math.fround(n) : n;
  if (!Number.isFinite(r)) throw overflow(ty);
  return ty === "f32" ? float32Value(r) : float64Value(r);
}

// makeFloatCast builds a float Value at `ty` from a float SOURCE value, where a NaN/±Inf source is
// preserved (it propagates — float→float is not a finite operation). Only a FINITE double that
// frounds past binary32 range traps 22003. Used by float → float casts.
function makeFloatCast(ty: ScalarType, n: number): Value {
  if (ty === "f64") return float64Value(n);
  const r = Math.fround(n);
  // A finite double beyond binary32 range frounds to ±Inf → trap; a NaN/±Inf source stays as-is.
  if (Number.isFinite(n) && !Number.isFinite(r)) throw overflow(ty);
  return float32Value(r);
}

// floatToIntHalfAway rounds a finite float to a bigint, HALF AWAY FROM ZERO (jed's one rounding
// mode — decimal.md §3; float.md §6). Math.round rounds half UP (toward +Inf), which differs for
// negative ties (Math.round(-2.5) = -2, want -3), so negatives are handled by magnitude. BigInt of
// a non-integer JS number throws, so the rounded (integral) double is converted.
function floatToIntHalfAway(v: number): bigint {
  const r = v < 0 ? -Math.round(-v) : Math.round(v);
  return BigInt(r);
}

// toDecimal widens a numeric value to Decimal (an integer operand of decimal arithmetic).
function toDecimal(v: Value): Decimal {
  if (v.kind === "decimal") return v.dec;
  if (v.kind === "int") return Decimal.fromBigInt(v.int);
  throw typeError("internal: non-numeric decimal operand");
}

// decimalArithWork is the decimal_work W of an arithmetic node — which group-count formula
// applies per op (spec/design/cost.md §3 "decimal_work"). The evaluator charges W − 1 before
// the op runs.
function decimalArithWork(op: BinaryOp, a: Decimal, b: Decimal): number {
  switch (op) {
    case "add":
    case "sub":
      return workLinear(a, b);
    case "mul":
      return workMul(a, b);
    case "div":
      return workDiv(a, b);
    default: // "mod"
      return workMod(a, b);
  }
}

// decimalCmpWork is the decimal_work W of a comparison over a decimal(-promotable) pair — the
// aligned linear formula after int→decimal promotion; 1 (no charge) for any other pair,
// including a NULL side, where no decimal compare runs (spec/design/cost.md §3 "decimal_work").
function decimalCmpWork(a: Value, b: Value): number {
  if (a.kind === "decimal" && b.kind === "decimal") return workLinear(a.dec, b.dec);
  if (a.kind === "decimal" && b.kind === "int") return workLinear(a.dec, Decimal.fromBigInt(b.int));
  if (a.kind === "int" && b.kind === "decimal") return workLinear(Decimal.fromBigInt(a.int), b.dec);
  return 1;
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

// not3 is three-valued NOT (Kleene): true<->false, unknown stays unknown. Used to build `<>`
// as the negation of `=`, so a NULL operand still yields UNKNOWN (`NULL <> NULL`), not a wrong TRUE.
function not3(a: ThreeValued): ThreeValued {
  if (a === "true") return "false";
  if (a === "false") return "true";
  return "unknown";
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
  // Floats by the TOTAL order (-0 == +0, NaN == NaN, NaN largest — float.md §3). ORDER BY / MIN /
  // MAX / DISTINCT over a float column reach here with same-width values (one typed column).
  if (a.kind === "f32" && b.kind === "f32") return floatTotalCmp(a.value, b.value);
  if (a.kind === "f64" && b.kind === "f64") return floatTotalCmp(a.value, b.value);
  if (a.kind === "text" && b.kind === "text") return compareTextC(a.text, b.text);
  if (a.kind === "bytea" && b.kind === "bytea") return compareBytea(a.bytes, b.bytes);
  if (a.kind === "uuid" && b.kind === "uuid") return compareBytea(a.bytes, b.bytes);
  if (a.kind === "bool" && b.kind === "bool") {
    return a.value === b.value ? 0 : a.value ? 1 : -1;
  }
  // Timestamps order by the i64 instant (-infinity < finite < infinity).
  if (a.kind === "timestamp" && b.kind === "timestamp") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
  }
  if (a.kind === "timestamptz" && b.kind === "timestamptz") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
  }
  if (a.kind === "date" && b.kind === "date") {
    return a.days < b.days ? -1 : a.days > b.days ? 1 : 0;
  }
  // Intervals order by the canonical 128-bit span (spec/design/interval.md §2).
  if (a.kind === "interval" && b.kind === "interval") return intervalCmp(a.iv, b.iv);
  // A composite sorts lexicographically, NULLs-last per field (the composite sort key —
  // spec/design/composite.md §5): the first non-equal field decides, recursing through keyCmp so
  // per-field NULL placement and nested composites are handled uniformly. The caller's descending
  // flip in keyCmp reverses the whole tuple. A row-size tie-break keeps it total (same-type rows
  // have equal arity, so it is only reached for safety).
  if (a.kind === "composite" && b.kind === "composite") {
    const n = Math.min(a.fields.length, b.fields.length);
    for (let i = 0; i < n; i++) {
      const c = keyCmp(a.fields[i]!, b.fields[i]!, false, false);
      if (c !== 0) return c;
    }
    return a.fields.length < b.fields.length ? -1 : a.fields.length > b.fields.length ? 1 : 0;
  }
  // An array sorts by the PG array_cmp total order (spec/design/array.md §5): element-wise over the
  // flattened elements (NULLs-last per element, recursing through keyCmp), then fewer elements first,
  // then smaller ndim, then per dimension (length, then lower bound).
  if (a.kind === "array" && b.kind === "array") {
    const n = Math.min(a.elements.length, b.elements.length);
    for (let i = 0; i < n; i++) {
      const c = keyCmp(a.elements[i]!, b.elements[i]!, false, false);
      if (c !== 0) return c;
    }
    if (a.elements.length !== b.elements.length) return a.elements.length < b.elements.length ? -1 : 1;
    if (a.dims.length !== b.dims.length) return a.dims.length < b.dims.length ? -1 : 1;
    for (let d = 0; d < a.dims.length; d++) {
      if (a.dims[d] !== b.dims[d]) return a.dims[d]! < b.dims[d]! ? -1 : 1;
      if (a.lbounds[d] !== b.lbounds[d]) return a.lbounds[d]! < b.lbounds[d]! ? -1 : 1;
    }
    return 0;
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
    case "f32":
      return 4;
    case "f64":
      return 5;
    case "text":
      return 6;
    case "bytea":
      return 7;
    case "uuid":
      return 8;
    case "timestamp":
      return 9;
    case "timestamptz":
      return 10;
    case "interval":
      return 11;
    case "date":
      return 13;
    // A composite sorts only against composites of its own type (ORDER BY is single-typed), so this
    // cross-family rank is only for totality; it sits after the scalar families.
    case "composite":
      return 12;
    default:
      return 13;
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
