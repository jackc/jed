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
  ConflictTarget,
  Cte,
  CteBody,
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
  OnConflict,
  OrderKey,
  QueryExpr,
  RefAction,
  SeqOptions,
  SeqRestart,
  Select,
  SelectItems,
  SetOp,
  SetOpKind,
  Statement,
  SubscriptSpec,
  TableRef,
  TypeMod,
  Update,
  WindowDef,
  WithExpr,
  WithQuery,
} from "./ast.ts";
import { cteBodyAsQuery, cteBodyIsDataModifying, emptySeqOptions } from "./ast.ts";
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
  type IdentityKind,
  type IndexDef,
  type SeqDataType,
  type SeqOwner,
  type SequenceDef,
  type Table,
  columnIndex,
  pkIndices,
  seqDataTypeDefaultBounds,
  seqDataTypeForScalar,
  seqDataTypeFromName,
  seqDataTypePgName,
  seqDataTypeRange,
  primaryKeyIndex,
  resolveColType,
} from "./catalog.ts";
import { LifetimeBudget, Meter } from "./cost.ts";
import {
  type Collation,
  foldCase,
  foldLowerSimple,
  loadedCollation,
  loadedCollationTables,
  loadedProperty,
  loadUnicodeData as loadUnicodeDataGlobal,
  serializeTable,
  sortKey as collationSortKey,
  versionSkew,
} from "./collation.ts";
import { COSTS } from "./costs.ts";
import {
  instantToLocalMicros,
  loadTimeZoneData as loadTimeZoneDataGlobal,
  loadedTimeZones as loadedTimeZonesGlobal,
  localToInstantMicros,
  offsetAtRef,
  resolveZone,
  type TimeZoneInfo,
  type ZoneRef,
} from "./timezone.ts";
import {
  dateTruncInterval,
  dateTruncMicros,
  type ExtractSrc,
  extractField,
} from "./datetime_fn.ts";
import { crc32Ieee } from "./format.ts";
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
import { encodeBool, encodeInt, encodeTerminated } from "./encoding.ts";
import { type EngineError, engineError } from "./errors.ts";
import { type Privilege, type PrivilegeSet, Privileges } from "./privileges.ts";
import { type ScriptSummary, splitStatements } from "./split.ts";
import type { SharedPaging } from "./paging.ts";
import { parseExpression, parseSQL } from "./parser.ts";
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
  rangeT,
  rank,
  roundToWidth,
  scalarT,
  scalarTypeFromName,
  typeCanonicalName,
  typeIsBoolean,
  typeIsBytea,
  typeIsDecimal,
  typeIsInteger,
  typeIsText,
  typeIsTimestamp,
  typeIsTimestamptz,
  typeIsDate,
  typeIsInterval,
  typeIsRange,
  typeIsUuid,
  typeAsScalar,
  typeScalar,
  widthBytes,
  isFixedWidth,
} from "./types.ts";
import { NEG_INFINITY, parseTimestamp, parseTimestamptz, POS_INFINITY } from "./timestamp.ts";
import { DATE_NEG_INFINITY, DATE_POS_INFINITY, parseDate } from "./date.ts";
import { uuidExtractTimestampMicros, uuidExtractVersion } from "./uuid.ts";
import { type ClockFunc, type RandomFill, Seam, StmtRng } from "./seam.ts";
import {
  type Interval,
  intervalAdd,
  intervalCmp,
  intervalEncodeKey,
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
import {
  AGGREGATES,
  type AggregateDesc,
  OPERATORS,
  type OperatorDesc,
  WINDOWS,
} from "./operators.ts";
import {
  type Value,
  type ArrayInResult,
  type ThreeValued,
  boolAnd,
  boolNot,
  arrayValue,
  emptyArray,
  emptyRangeValue,
  rangeValue,
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
import {
  elementScalar,
  finalizeRange,
  encodeRangeKey,
  parseBoundFlags,
  parseRangeText,
  rangeAdjacent,
  rangeAfter,
  rangeBefore,
  rangeByName,
  rangeContains,
  rangeContainsElem,
  rangeForElement,
  rangeIntersect,
  rangeMinus,
  rangeNameForElement,
  rangeOverlaps,
  rangeOverleft,
  rangeOverright,
  rangeTotalCmp,
  rangeUnion,
} from "./range.ts";
import type { RangeDesc } from "./ranges_gen.ts";
import {
  compileRegex,
  type RegexProgram,
  regexIsMatch,
  regexNinst,
  regexpMatch,
  regexpReplace,
} from "./regex.ts";

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
  | {
      kind: "query";
      columnNames: string[];
      columnTypes: string[];
      rows: Value[][];
      cost: bigint;
    };

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

// DEFAULT_TEMP_BUFFERS is the default per-session storage budget for SESSION-LOCAL temporary tables,
// in BYTES (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which neither
// the per-statement cost ceiling (maxCost) nor the cumulative budget (lifetimeMaxCost) bounds, so
// tempBuffers is the §13 gate that does: the instant a session's resident temp storage (byte-identical
// on-disk record bytes) would exceed it, the write aborts 54P03. 0 ⇒ unlimited (a trusted handle).
// Identical across cores (§8); the abort point is part of the cross-core contract.
export const DEFAULT_TEMP_BUFFERS = 32 << 20;

// DEFAULT_SHARED_TEMP_MEM is the default GLOBAL storage budget for DATABASE-WIDE shared temporary
// tables, in BYTES (spec/design/temp-tables.md §7). The shared-temp analogue of DEFAULT_TEMP_BUFFERS:
// shared temp data is global (one set of rows across every session of the open Database), so its
// budget is a Database-level setting (sharedTempMem) rather than per-session. An over-budget shared
// write aborts the same 54P03. 0 ⇒ unlimited; measured identically (deterministic on-disk record
// bytes), so the abort point is part of the cross-core contract.
export const DEFAULT_SHARED_TEMP_MEM = 32 << 20;

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

// CollationVerdict is the slice-2d version-skew verdict for one referenced collation
// (spec/design/collation.md §12, compatibility.md §7). "full" ⇒ a loaded bundle provides the name at
// the file's pinned (unicode, cldr), so the collation's objects are read-write. "skewed" ⇒ a loaded
// bundle provides it at a DIFFERENT version, so its objects are read-only (reads recompute against the
// loaded table — the heap-scan fallback; a write raises XX002). A pure comparison of the file pin (§5)
// vs the loaded set — every core computes the identical verdict (the §10 cross-core contract).
export type CollationVerdict = "full" | "skewed";

// CollationInfo is introspection metadata for one loaded collation (db.collations,
// spec/design/collation.md §1). contentHash is the CRC-32 of the compiled table (the reference-mode
// stamp, §3/§4); description is provenance, excluded from the hash. verdict is the slice-2d
// version-skew verdict (§12) — "full" for the engine-global loaded set (it IS the reference); for a
// database's referenced collations it is "skewed" when the file's pin differs from the loaded bundle's.
export type CollationInfo = {
  name: string;
  unicodeVersion: string;
  cldrVersion: string;
  contentHash: number;
  description: string;
  isDefault: boolean;
  verdict: CollationVerdict;
};

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
  // collations caches collations RESOLVED from the file's reference entries on open, keyed by their
  // exact (CASE-SENSITIVE) name — collation names are quoted identifiers ("en-US",
  // spec/design/collation.md §1). C is never stored (table-free, built in). Under the reference-only
  // model (§4.2) the file holds only a metadata entry per collation the schema references; the table
  // comes from the binary's vendored set (entry_kind = 3, format_version 18). Collation objects are
  // never mutated in place (only added), so the shallow Map copy in clone is safe.
  collations: Map<string, Collation>;
  // defaultCollation is the per-database default collation name, or null for C (collation.md §1/§5).
  // An un-annotated text column inherits this at CREATE TABLE. Persisted as the is_default flag bit
  // on that collation's entry_kind = 3 reference entry, restored on load.
  defaultCollation: string | null;

  constructor(
    txid: bigint = 0n,
    tables: Map<string, Table> = new Map(),
    stores: Map<string, TableStore> = new Map(),
    indexStores: Map<string, TableStore> = new Map(),
    types: Map<string, CompositeType> = new Map(),
    sequences: Map<string, SequenceDef> = new Map(),
    collations: Map<string, Collation> = new Map(),
    defaultCollation: string | null = null,
  ) {
    this.txid = txid;
    this.tables = tables;
    this.stores = stores;
    this.indexStores = indexStores;
    this.types = types;
    this.sequences = sequences;
    this.collations = collations;
    this.defaultCollation = defaultCollation;
  }

  // clone returns an independent copy: the catalog maps are shallow (Table / CompositeType /
  // SequenceDef / Collation objects are never mutated in place — only added/removed) and each store
  // is an O(1) persistent-map clone (pmap.ts).
  clone(): Snapshot {
    return new Snapshot(
      this.txid,
      new Map(this.tables),
      cloneStores(this.stores),
      cloneStores(this.indexStores),
      new Map(this.types),
      new Map(this.sequences),
      new Map(this.collations),
      this.defaultCollation,
    );
  }

  // resolveCollation resolves a collation name for USE — query resolution and key encoding
  // (spec/design/collation.md §2/§9). The collations the database has resolved (a cache populated on
  // open from the file's reference entries, carrying their version pin) first, then the engine-global
  // LOADED set (db.loadUnicodeData, §4). undefined ⇒ neither has it (resolver → 42704). C is handled
  // by the caller (built-in). This is the reference-only read path: a collation is never baked into
  // the file — the file references it by name and the table comes from a loaded bundle.
  resolveCollation(name: string): Collation | undefined {
    return this.collations.get(name) ?? loadedCollation(name);
  }

  // collationSkew is the slice-2d version-skew verdict for a referenced collation (collation.md §12):
  // [fileUnicode, fileCldr, loadedUnicode, loadedCldr] when this database's keys were built under a
  // different (unicode, cldr) than the loaded bundle provides — the object using it is read-only
  // (XX002 on write) — else undefined (Full: same version, or this collation has no catalog-local file
  // pin so it is freshly the loaded version, an in-memory-only database). A pure comparison of the file
  // pin already in the catalog (§5) vs the engine-global loaded set; the Snapshot wiring of versionSkew.
  collationSkew(name: string): [string, string, string, string] | undefined {
    const cat = this.collations.get(name);
    if (cat === undefined) return undefined;
    const loaded = versionSkew(name, cat.unicodeVersion, cat.cldrVersion);
    if (loaded === undefined) return undefined;
    return [cat.unicodeVersion, cat.cldrVersion, loaded[0], loaded[1]];
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

  // referencedCollations is the collations the database SCHEMA references — every column's frozen
  // collation plus the per-database default — resolved (catalog-local set, then the binary's vendored
  // set) and sorted by exact name. Under the reference-only model (spec/design/collation.md §2/§5)
  // these, not an imported set, are what earn a metadata entry on disk: a collation is recorded
  // because the schema uses it, regardless of whether it was ever passed to a (now-removed) import
  // call. C columns (null collation) reference nothing. A referenced name this build does not vendor
  // throws (the precursor to the slice-2d open-time verdict).
  referencedCollations(): Collation[] {
    const names = new Set<string>();
    for (const t of this.tables.values()) {
      for (const col of t.columns) {
        if (col.collation !== null) names.add(col.collation);
      }
    }
    if (this.defaultCollation !== null) names.add(this.defaultCollation);
    return [...names].sort().map((name) => {
      const c = this.resolveCollation(name);
      if (c === undefined) {
        throw engineError(
          "undefined_object",
          `collation "${name}" referenced by the schema is not provided by a loaded bundle`,
        );
      }
      return c;
    });
  }

  // upgradeCollations is the REINDEX / COLLATION UPGRADE migration (spec/design/collation.md §12):
  // rebuild every collated key stored under a version-SKEWED collation against the LOADED table and
  // advance that collation's pin to the loaded version — clearing the skew so the affected tables are
  // read-write again and their collated indexes regain pushdown (a Full index, encoding.md §2.12).
  // Returns the number of collations re-pinned (0 ⇒ nothing skewed, a no-op).
  //
  // Whole-database, per-collation pin: the pin is ONE entry per collation NAME (§5), so a collation's
  // pin may advance only once every key under it is rebuilt — else a not-yet-rebuilt table would
  // falsely read as Full (corruption). The caller swaps the result in atomically. resolveCollation
  // already yields the loaded table data (the file entry carries the file pin but loaded
  // singles/contractions), so re-encoding produces loaded-version sort keys; the re-pin realigns the label.
  upgradeCollations(pageSize: number): number {
    const skewed = new Set<string>();
    for (const c of this.referencedCollations()) {
      if (this.collationSkew(c.name) !== undefined) skewed.add(c.name);
    }
    if (skewed.size === 0) return 0;
    const isSkewed = (coll: string | null): boolean => coll !== null && skewed.has(coll);

    // Sorted table order (no Map-iteration leak, CLAUDE.md §8; the per-table rebuilds are independent
    // and the re-pin is order-free, so the result is order-invariant regardless).
    for (const key of [...this.tables.keys()].sort()) {
      const table = this.tables.get(key)!;
      // A collated PK re-encode moves every storage key ⇒ a full table rewrite, and an index entry
      // carries the storage key as its suffix (indexes.md §3) ⇒ every index of the table is rebuilt.
      // Else only the indexes whose own key columns use a skewed collation are rebuilt.
      const pkSkewed = table.pk.some((i) => isSkewed(table.columns[i]!.collation));
      const indexes = table.indexes.filter(
        (idx) => pkSkewed || idx.columns.some((c) => isSkewed(table.columns[c]!.collation)),
      );
      if (!pkSkewed && indexes.length === 0) continue;
      const colls: (Collation | null)[] = table.columns.map((c) =>
        c.collation !== null ? (this.resolveCollation(c.collation) ?? null) : null,
      );
      // Read every (storage key, row) pair, fully materialized (a spilled non-key value must survive
      // a rewrite; a collated key column never spills — §2.12 narrowing b).
      const store = this.store(table.name);
      const entries: Entry[] = store
        .entriesInKeyOrder()
        .map((e) => ({ key: e.key, row: store.resolveAll(e.row) }));
      // The NEW storage key per row: re-encoded under the loaded collation if the PK moved, else the
      // existing key (unchanged — includes a synthetic-rowid table, which has no PK).
      if (pkSkewed) {
        for (const e of entries) e.key = encodePkKey(table, table.pk, colls, e.row);
        this.putTable(table, pageSize); // fresh empty store (+ re-register the same table)
        const fresh = this.store(table.name);
        for (const e of entries) fresh.insert(e.key, e.row);
      }
      // Rebuild each affected index store from the (re-keyed) rows.
      for (const def of indexes) {
        const ekeys: Uint8Array[] = [];
        for (const e of entries)
          ekeys.push(...indexEntryKeys(table.columns, colls, def, e.key, e.row));
        ekeys.sort(compareBytes);
        const fresh = new TableStore(pageSize - 12, []); // 12 = PAGE_HEADER
        for (const ek of ekeys) fresh.insert(ek, []);
        this.putIndexStore(def.name.toLowerCase(), fresh);
      }
    }
    // Advance each skewed collation's pin to the loaded version.
    for (const name of skewed) {
      const loaded = loadedCollation(name);
      if (loaded !== undefined) this.collations.set(name, loaded);
    }
    return skewed.size;
  }

  // sequencesOwnedBy returns the lowercased keys of every sequence OWNED BY the table `name`
  // (case-insensitive) — the serial-created sequences DROP TABLE must auto-drop
  // (spec/design/sequences.md §12). Sorted so the auto-drop is deterministic (no map-iteration
  // leak, §8).
  sequencesOwnedBy(name: string): string[] {
    const lower = name.toLowerCase();
    const keys: string[] = [];
    for (const [k, s] of this.sequences) {
      if (s.ownedBy !== undefined && s.ownedBy.table.toLowerCase() === lower) keys.push(k);
    }
    return keys.sort();
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
    // Resolve each column's ColType against the (already-registered) composite-type catalog —
    // self-contained codec/coercion trees the store carries, so the value codec never re-walks the
    // type catalog per row (spec/design/composite.md §4). Composite types are registered before any
    // table (the types-first catalog emission order), so resolveColType always resolves.
    const colTypes = t.columns.map((c) => resolveColType(c.type, this.types));
    this.putTableResolved(t, colTypes, pageSize);
  }

  // putTableResolved registers a table whose column ColTypes are ALREADY resolved — used when staging
  // a TEMP table (spec/design/temp-tables.md §8): a temp table's composite columns must resolve against
  // the MAIN snapshot's type catalog (composites are never temp — CREATE TYPE is persistent), not this
  // (temp) snapshot's empty types map. The resolved ColType tree is fully self-contained
  // (spec/design/composite.md §4), so the store needs nothing from the catalog thereafter. The plain
  // putTable resolves against this.types and delegates here.
  putTableResolved(t: Table, colTypes: ColType[], pageSize: number): void {
    const key = t.name.toLowerCase();
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

  // hasIndexStore reports whether this snapshot holds a store for the named index (lowercased key).
  // Used to route index access to the session temp snapshot vs the main snapshot (temp-tables.md §2).
  hasIndexStore(nameKey: string): boolean {
    return this.indexStores.has(nameKey);
  }

  // storageBytes is the total on-disk record bytes of every table store + index store in this snapshot
  // — the temp budget's deterministic footprint measure (spec/design/temp-tables.md §7), summed over
  // the session temp snapshot. Iteration order does not matter (it is a sum).
  storageBytes(): number {
    let total = 0;
    for (const st of this.stores.values()) total += st.storedBytes();
    for (const st of this.indexStores.values()) total += st.storedBytes();
    return total;
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

  // setColumnDefaultExpr replaces a table column's expression default in place — used by ALTER
  // SEQUENCE … RENAME of an owned sequence to rewrite the owning column's nextval default
  // (spec/design/sequences.md §15.3), leaving the table's rows/store untouched. The Table and its
  // columns array are re-allocated (catalog Tables are never mutated in place — snapshots share
  // them). A no-op if the table or column ordinal is absent.
  setColumnDefaultExpr(tableKey: string, column: number, defaultExpr: DefaultExpr): void {
    const old = this.tables.get(tableKey);
    if (old === undefined || column < 0 || column >= old.columns.length) return;
    const columns = old.columns.map((c, i) => (i === column ? { ...c, defaultExpr } : c));
    this.tables.set(tableKey, { ...old, columns });
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
  // tempWorking is the transaction's working copy of the session's temp-table snapshot
  // (spec/design/temp-tables.md §5): cloned from Session.tempCommitted at tx open, mutated by temp
  // DDL/DML, adopted back into tempCommitted on a successful COMMIT and discarded on ROLLBACK. The
  // temp analogue of working, kept SEPARATE so it is never serialized.
  tempWorking: Snapshot;
  // sharedTempWorking is the transaction's working copy of the DATABASE-WIDE shared temp-table
  // snapshot (spec/design/temp-tables.md §5): cloned from Database.sharedTempCommitted at tx open,
  // mutated by shared-temp DDL/DML, adopted back on a successful COMMIT and discarded on ROLLBACK. The
  // shared analogue of tempWorking; for the shared layer the adopted state is then published to the
  // shared root (the two-root commit, §5).
  sharedTempWorking: Snapshot;
  // mainDirty is whether this transaction mutated the MAIN (persistent) snapshot — set by working().
  // Drives the commit's persist decision so a transaction that touched ONLY temp tables (session-local
  // and/or shared) makes zero file writes (temp-tables.md §2). tempDirty / sharedTempDirty mirror it
  // for the two temp snapshots.
  mainDirty: boolean;
  tempDirty: boolean;
  sharedTempDirty: boolean;
};

// SessionOptions are the relocatable session settings (spec/design/session.md §3 — the bucket-A
// envelope subset landed in S1): the cost ceiling, the input-size limit, and the work-memory budget.
// Passed to (db.newSession). An absent field takes its default. The entropy/clock seam is injected
// via Session.setRandomSource / setClockSource, not here.
export type SessionOptions = {
  maxCost?: bigint;
  // The per-session cumulative cost budget (spec/design/session.md §5.4); absent ⇒ unlimited (the
  // default). Bounds the whole session: the instant the session's running total reaches it, the
  // in-flight statement aborts 54P02 (and once spent, every further statement is rejected at
  // admission). Sibling to maxCost, which bounds one statement.
  lifetimeMaxCost?: bigint;
  maxSqlLength?: number;
  workMem?: number;
  // The table-privilege set granted to every table — the GRANT … ON ALL TABLES default
  // (spec/design/session.md §5.3). Absent ⇒ all four (the default), so a fresh session is
  // unrestricted; PrivilegeSet.empty().with("select") is a read-only session.
  defaultPrivileges?: PrivilegeSet;
  // Whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted; a denied schema
  // change is 42501 (§5.3). Absent ⇒ on. Its scope narrows with temporary tables (temp-tables.md §5):
  // allowTempDdl is the temp-scoped sibling gate.
  allowDdl?: boolean;
  // Whether SESSION-LOCAL temporary-table DDL is permitted (spec/design/temp-tables.md §5); a denied
  // temp DDL is 42501. Absent ⇒ INHERIT allowDdl's value (back-compat: one gate governs all DDL). The
  // untrusted-scratch pattern is allowDdl=false + allowTempDdl=true — private scratch tables only.
  allowTempDdl?: boolean;
  // Whether DATABASE-WIDE shared temporary-table DDL is permitted (spec/design/temp-tables.md §5); a
  // denied shared-temp DDL is 42501. Absent ⇒ INHERIT allowDdl's value (back-compat, like
  // allowTempDdl). Shared-temp DDL mutates global state and charges the global budget, so it is the
  // more privileged of the two temp gates.
  allowSharedTempDdl?: boolean;
  // The per-session storage budget for session-local temp tables, in BYTES (spec/design/temp-tables.md
  // §7); 0 ⇒ unlimited; absent ⇒ the engine default (DEFAULT_TEMP_BUFFERS). An over-budget temp write
  // aborts 54P03.
  tempBuffers?: number;
  // The session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a timestamptz is
  // decomposed in by date_trunc / EXTRACT / the cross-family casts. Absent ⇒ UTC. Accepts UTC, a fixed
  // ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; an invalid value falls back to
  // UTC at construction (the validated setter is Session.setTimeZone — 22023).
  timeZone?: string;
};

// TxStatus is the session transaction status (spec/design/session.md §2.2) — PostgreSQL's three
// connection states made explicit on the session, derived from the open transaction: no transaction
// ⇒ "Idle" (autocommit); an open clean block ⇒ "Open"; an open block a statement aborted ⇒ "Failed"
// (only ROLLBACK/COMMIT accepted, everything else 25P02). A string union (the engine is the erasable
// TS subset — no enum, CLAUDE.md §2).
export type TxStatus = "Idle" | "Open" | "Failed";

function txStatusOf(tx: ActiveTx | null): TxStatus {
  if (tx === null) return "Idle";
  return tx.failed ? "Failed" : "Open";
}

// Session is the per-connection SESSION state (spec/design/session.md §2.1): the configured, stateful
// context a host runs statements through, un-fused from the committed storage on Database. It owns the
// open transaction (the Idle/Open/Failed machine), the relocated handle settings, the entropy/clock
// seam, and the currval/lastval session state. A Database holds one as its long-lived default session;
// db.newSession mints additional independent ones that run sequentially on a single-threaded handle
// (by swapping into the default slot for the duration of a call — TS objects swap by reference).
// requireCustomVarName validates + canonicalizes a session-variable name (spec/design/session.md
// §6.1). A variable must be namespaced like a PostgreSQL custom GUC — a dotted name (myapp.tenant); a
// non-dotted name would be a built-in setting, and v1 exposes none through this map (the time_zone
// built-in is a separate slice), so it is 42704. Returns the case-folded (lowercase, PG GUC names are
// case-insensitive) map key.
function requireCustomVarName(name: string): string {
  if (name.includes(".")) {
    return name.toLowerCase();
  }
  throw engineError("undefined_object", "unrecognized configuration parameter: " + name);
}

export class Session {
  // The open transaction, or null under autocommit (transactions.md §4.1); the Idle/Open/Failed
  // status (session.md §2.2) is derived from this.
  tx: ActiveTx | null;
  // The execution-cost ceiling (CLAUDE.md §13; api.md §8), or 0n for unlimited. Bounds every statement
  // run on this session: its Meter aborts 54P01 the instant accrued cost reaches it.
  maxCost: bigint;
  // The per-session cumulative cost budget (spec/design/session.md §5.4) and the session's running
  // CUMULATIVE cost, held together in a LifetimeBudget object shared (by reference) with every
  // statement Meter, which live-charges into it — so partial cost of an aborted statement counts and
  // the cumulative survives the swap (TS swaps the session object by reference). SESSION state, not
  // snapshot state: the cumulative does NOT roll back with a transaction. The budget is 0n ⇒ unlimited
  // (track-only); a statement aborts 54P02 the instant lifetime.total reaches lifetime.limit.
  lifetime: LifetimeBudget;
  // The maximum input SQL length in bytes (CLAUDE.md §13; cost.md §7); 0 = unlimited; default
  // DEFAULT_MAX_SQL_LENGTH. Over-limit input is rejected 54000 at parse, before lexing.
  maxSqlLength: number;
  // The work-memory budget in bytes (spec/design/spill.md §2): the memory a blocking operator holds
  // before it spills. 0 = unlimited; default DEFAULT_WORK_MEM. Never changes what a query observes.
  workMem: number;
  // The entropy + clock seam for the uuid generators / clock functions (entropy.md): two
  // host-injectable functions, each unset ⇒ the platform primitive. Tests inject seededRandomSource +
  // fixedClock (the # seed: / # clock: directives) for byte-identical cross-core output.
  seam: Seam;
  // SESSION currval state (sequences.md §6): the last value nextval/setval(…,true) produced IN THIS
  // SESSION for each sequence (lowercased name). NOT in the snapshot, NOT persisted.
  sessionSeq: Map<string, bigint>;
  // SESSION lastval state (sequences.md §6): the lowercased name of the sequence the most recent
  // nextval ran on — null before the first nextval.
  sessionLastName: string | null;
  // Per-STATEMENT running sequence advances (sequences.md §4); flushed into the working snapshot on
  // success, discarded on error (the transactional rollback of the advance, §5).
  pendingSeq: Map<string, SequenceDef>;
  // Per-STATEMENT running currval updates → flushed into sessionSeq on success.
  pendingCurrval: Map<string, bigint>;
  // Per-STATEMENT running lastval update → flushed into sessionLastName on success.
  pendingLastName: string | null;
  // The authorization envelope (spec/design/session.md §5.3): the GRANT/REVOKE-style per-object
  // privilege model the host configures and the engine enforces (42501) at name resolution. A fresh
  // session is fully permissive (every table privilege, every function EXECUTE).
  privileges: Privileges;
  // Whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted on this session
  // (§5.3); a denied schema change is 42501. Default on. Its scope narrows with temporary tables
  // (temp-tables.md §5): allowTempDdl is the temp-scoped sibling gate.
  allowDdl: boolean;
  // Whether session-local TEMPORARY-table DDL is permitted (spec/design/temp-tables.md §5); a denied
  // temp DDL is 42501. Resolved at construction from opts.allowTempDdl (defaulting to allowDdl's value).
  allowTempDdl: boolean;
  // Whether DATABASE-WIDE shared TEMPORARY-table DDL is permitted (spec/design/temp-tables.md §5); a
  // denied shared-temp DDL is 42501. Resolved at construction from opts.allowSharedTempDdl (defaulting
  // to allowDdl's value). The more privileged of the two temp gates — a global-state mutation.
  allowSharedTempDdl: boolean;
  // The per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒ unlimited. An
  // over-budget temp write aborts 54P03.
  tempBuffers: number;
  // The session-local TEMPORARY-table catalog + stores (spec/design/temp-tables.md §2): a Snapshot
  // holding only this session's temp tables, their stores, and their (UNIQUE) index stores. NEVER
  // serialized — only Database.committed is written to the file, so a temp table makes ZERO file
  // writes. Private to this Session (it carries across the by-reference session swap and is invisible
  // to other sessions), dropped wholesale with the session. Transactional like the main snapshot: an
  // open transaction clones it into ActiveTx.tempWorking, adopted on a successful COMMIT, discarded on
  // ROLLBACK.
  tempCommitted: Snapshot;
  // The session variables (spec/design/session.md §6.1): PostgreSQL's GUC model scoped to the session
  // — a string→string map (PG GUCs are all text) the host sets (setVar/resetVar) and SQL reads with
  // current_setting. Custom (dotted) names only in v1. SESSION state, not snapshot state: it does NOT
  // roll back with a transaction (PG SET SESSION), and each session keeps its own map across the
  // by-reference swap (like the privilege envelope).
  vars: Map<string, string>;
  // The resolved session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
  // timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. Resolved once
  // (from opts.timeZone at construction, or setTimeZone) to a cheap ZoneRef (UTC = fixed 0); the
  // evaluator reads it via the active session. SESSION state (no storage effect).
  timeZone: ZoneRef;
  // The read pin for a data-modifying WITH statement (spec/design/writable-cte.md §2): the single
  // pre-statement snapshot every sub-statement reads, so the data-modifying CTEs and the primary
  // cannot observe each other's table writes (their writes still accumulate into the transaction's
  // working). Set by the writable-CTE orchestrator before the first sub-statement runs and cleared
  // when it finishes (success or error); null for every other statement, where reads fall through to
  // working/committed as usual (readSnap).
  readPin: Snapshot | null;

  constructor(opts: SessionOptions = {}) {
    this.tx = null;
    this.maxCost = opts.maxCost ?? 0n;
    this.lifetime = new LifetimeBudget(opts.lifetimeMaxCost ?? 0n);
    this.maxSqlLength = opts.maxSqlLength ?? DEFAULT_MAX_SQL_LENGTH;
    this.workMem = opts.workMem ?? DEFAULT_WORK_MEM;
    this.seam = new Seam();
    this.sessionSeq = new Map();
    this.sessionLastName = null;
    this.pendingSeq = new Map();
    this.pendingCurrval = new Map();
    this.pendingLastName = null;
    this.privileges = new Privileges();
    if (opts.defaultPrivileges !== undefined) {
      this.privileges.setDefaultTable(opts.defaultPrivileges);
    }
    this.allowDdl = opts.allowDdl ?? true;
    // Back-compat default-inheritance (temp-tables.md §5): an unset allowTempDdl / allowSharedTempDdl
    // takes allowDdl's value, so a session configured before temp tables existed behaves as before.
    this.allowTempDdl = opts.allowTempDdl ?? this.allowDdl;
    this.allowSharedTempDdl = opts.allowSharedTempDdl ?? this.allowDdl;
    this.tempBuffers = opts.tempBuffers ?? DEFAULT_TEMP_BUFFERS;
    this.tempCommitted = new Snapshot();
    this.vars = new Map();
    // Resolve the configured zone once; an invalid value falls back to UTC at construction (the
    // validated path is setTimeZone, which surfaces 22023). timezones.md §9.4.
    this.timeZone = resolveZone(opts.timeZone ?? "UTC") ?? { fixed: true, off: 0 };
    this.readPin = null;
  }

  // setTimeZone sets the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone
  // a timestamptz is decomposed in. Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded
  // JTZ bundle provides; a name no bundle provides (and not a built-in) is 22023, the value unchanged.
  setTimeZone(zone: string): void {
    const zr = resolveZone(zone);
    if (zr === undefined) {
      throw engineError("invalid_parameter_value", `time zone "${zone}" not recognized`);
    }
    this.timeZone = zr;
  }

  // run installs this session as db's active session, runs fn, and restores the default — the swap
  // that lets an additional session run on a single-threaded handle (spec/design/session.md §2.1).
  // TS swaps by reference (no value copy); the default is restored even if fn throws.
  private run<T>(db: Database, fn: () => T): T {
    const saved = db.session;
    db.session = this;
    try {
      return fn();
    } finally {
      db.session = saved;
    }
  }

  // execute runs a (possibly mutating) statement on this session against db, binding $N params. A
  // SELECT returns the query Outcome (with its rows). Transactions are driven via SQL BEGIN/COMMIT
  // through execute; the view/update closure sugar (Rust/Go) is a TS follow-on (it would import the
  // api.ts Transaction, a module cycle the executor avoids).
  execute(db: Database, sql: string, params: Value[] = []): Outcome {
    return this.run(db, () => db.executeStmtParams(db.parse(sql), params));
  }

  // executeScript runs a multi-statement script on this ADDITIONAL session against db, sharing
  // committed storage and running sequentially via the swap (spec/design/session.md §2.1/§4.2).
  executeScript(db: Database, sql: string): ScriptSummary {
    return this.run(db, () => db.executeScript(sql));
  }

  // status is this session's transaction status (Idle/Open/Failed, session.md §2.2).
  status(): TxStatus {
    return txStatusOf(this.tx);
  }
  // inTransaction reports whether an explicit transaction block is open on this session.
  inTransaction(): boolean {
    return this.tx !== null;
  }
  setMaxCost(limit: bigint): void {
    this.maxCost = limit;
  }
  // setLifetimeMaxCost sets the per-session cumulative cost budget (spec/design/session.md §5.4);
  // <= 0n ⇒ unlimited. A statement aborts 54P02 the instant the session's cumulative cost reaches it,
  // and once spent every further statement is rejected 54P02 at admission.
  setLifetimeMaxCost(limit: bigint): void {
    this.lifetime.limit = limit;
  }
  // lifetimeMaxCost is the current per-session cumulative cost budget (0n ⇒ unlimited).
  lifetimeMaxCost(): bigint {
    return this.lifetime.limit;
  }
  // lifetimeCost is the session's running CUMULATIVE execution cost so far (spec/design/session.md
  // §5.4) — the gauge the budget bounds. Tracked even when unlimited; survives a transaction rollback.
  lifetimeCost(): bigint {
    return this.lifetime.total;
  }
  // newMeter builds the Meter for a statement run on this session: the per-statement maxCost ceiling
  // (54P01) plus the shared LifetimeBudget (54P02) the meter live-charges into. Every statement's
  // meter is minted here, so all execution cost accrues into the cumulative.
  newMeter(): Meter {
    return new Meter(this.maxCost, this.lifetime);
  }
  setMaxSqlLength(bytes: number): void {
    this.maxSqlLength = bytes;
  }
  setWorkMem(bytes: number): void {
    this.workMem = bytes;
  }
  // setDefaultPrivileges replaces the default table-privilege set — the GRANT … ON ALL TABLES default
  // (§5.3). A read-only session is PrivilegeSet.empty().with("select").
  setDefaultPrivileges(privs: PrivilegeSet): void {
    this.privileges.setDefaultTable(privs);
  }
  // grant grants privs on a specific object (table or function), beyond the default (§5.3).
  grant(privs: PrivilegeSet, object: string): void {
    this.privileges.grant(privs, object);
  }
  // revoke revokes privs from a specific object (revoke wins over grant and the default, §5.3).
  revoke(privs: PrivilegeSet, object: string): void {
    this.privileges.revoke(privs, object);
  }
  // setAllowDdl sets whether DDL is permitted on this session (§5.3); a denied change is 42501.
  setAllowDdl(allow: boolean): void {
    this.allowDdl = allow;
  }
  // setAllowTempDdl sets whether session-local temporary-table DDL is permitted (temp-tables.md §5).
  setAllowTempDdl(allow: boolean): void {
    this.allowTempDdl = allow;
  }
  // setAllowSharedTempDdl sets whether DATABASE-WIDE shared temporary-table DDL is permitted
  // (temp-tables.md §5).
  setAllowSharedTempDdl(allow: boolean): void {
    this.allowSharedTempDdl = allow;
  }
  // setTempBuffers sets the per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒
  // unlimited. An over-budget temp write aborts 54P03.
  setTempBuffers(bytes: number): void {
    this.tempBuffers = bytes;
  }
  // setVar sets a session variable (spec/design/session.md §6.1) — PostgreSQL's GUC model, scoped to
  // the session. Custom variables must be namespaced (a dotted name like myapp.tenant); a non-dotted
  // name is 42704 (no built-in setting is reachable through this map in v1 — the time_zone built-in is
  // its own slice). The name is case-insensitive (folded to lowercase, PG); the value is text. Session
  // state, not snapshot state — it does NOT roll back with a transaction.
  setVar(name: string, value: string): void {
    this.vars.set(requireCustomVarName(name), value);
  }
  // resetVar clears a session variable (§6.1). A non-dotted name is 42704 (as for setVar); an unset
  // name is a no-op (PG RESET of an unset custom variable).
  resetVar(name: string): void {
    this.vars.delete(requireCustomVarName(name));
  }
  // var reads a session variable's value (§6.1), or undefined if it is not set. The host getter never
  // throws — it is the SQL current_setting read that raises 42704 on an unset name.
  var(name: string): string | undefined {
    return this.vars.get(name.toLowerCase());
  }
  // resetVars clears every session variable (§6.1) — PostgreSQL's RESET ALL for the variable map (also
  // the per-record reset hook the conformance harness's # set: directive uses).
  resetVars(): void {
    this.vars.clear();
  }
  setRandomSource(f: RandomFill): void {
    this.seam.randomFill = f;
  }
  clearRandomSource(): void {
    this.seam.randomFill = undefined;
  }
  setClockSource(f: ClockFunc): void {
    this.seam.clock = f;
  }
  clearClockSource(): void {
    this.seam.clock = undefined;
  }
}

export class Database {
  // The last committed, immutable state — what fresh readers (and autocommit reads) see.
  committed: Snapshot;
  // The DEFAULT SESSION (spec/design/session.md §2.1): the per-connection state this handle runs
  // statements through — the open transaction (the Idle/Open/Failed machine, §2.2), the relocated
  // settings (maxCost/maxSqlLength/workMem, the entropy/clock seam), and the currval/lastval session
  // state. A bare Database IS committed storage + this one long-lived stateful default session; the
  // convenience methods operate on it. newSession mints additional independent sessions (run
  // sequentially on this single-threaded handle by swapping into this slot for a call).
  session: Session;
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
  // readOnly marks a handle opened read-only (spec/design/api.md §2.1, OpenOptions.readOnly). A
  // read-only handle behaves like PostgreSQL hot standby: every transaction defaults to READ ONLY,
  // an explicit READ WRITE request and any write statement are 25006, and the file is opened
  // without write access, so it is never written. Always false for an in-memory or
  // normally-opened database.
  readOnly: boolean;
  // spillSink is the host backing for the ORDER BY external merge sort's spilled runs (spec/design/
  // spill.md §4): set by a durable host that can spill to disk (file.ts → FileSpillSink), null for an
  // in-memory or OPFS database (which never spills — sorts stay resident, spill.md §2). A type-only
  // import keeps the executor free of any node:* dependency (the Node `fs` impl lives in spillfile.ts),
  // so the engine runs in a browser bundle (the OPFS host).
  spillSink: SpillSink | null;
  // sharedTempCommitted is the DATABASE-WIDE shared temporary-table snapshot (temp-tables.md §4): the
  // committed rows of every SHARED temp table, held in memory and NEVER serialized — the shared
  // analogue of session.tempCommitted, but on the handle (visible to every session minted from this
  // Database) rather than per-session. On a single handle this is just a field; for the shared layer
  // (shared.ts) it is pinned from / published to the shared roots alongside committed (the two-root
  // commit, §5). Born empty, gone at close — never recovered (divergence D5).
  sharedTempCommitted: Snapshot;
  // sharedTempMem is the GLOBAL byte budget for shared temp storage (shared_temp_mem, §7); 0 ⇒
  // unlimited. The shared analogue of session.tempBuffers, but Database-level (shared temp is global).
  // An over-budget shared write aborts 54P03.
  sharedTempMem: number;

  constructor() {
    this.committed = new Snapshot();
    this.session = new Session();
    this.path = null;
    this.pageSize = DEFAULT_PAGE_SIZE;
    this.pageCount = 0;
    this.freePages = [];
    this.persistHook = null;
    this.paging = null;
    this.readOnly = false;
    this.spillSink = null;
    this.sharedTempCommitted = new Snapshot();
    this.sharedTempMem = DEFAULT_SHARED_TEMP_MEM;
  }

  // setMaxCost sets the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
  // spec/design/api.md §8). A positive limit bounds every subsequent statement: it aborts with 54P01
  // the instant accrued cost reaches limit (spec/design/cost.md §6). limit <= 0n (the default) is
  // unlimited. The primary guard for safely evaluating untrusted, user-supplied queries; a handle
  // setting, not stored in the file.
  setMaxCost(limit: bigint): void {
    this.session.maxCost = limit;
  }

  // setLifetimeMaxCost sets the PER-SESSION cumulative cost budget on the default session
  // (spec/design/session.md §5.4); limit <= 0n (the default) is unlimited. Where maxCost bounds one
  // statement (54P01), this bounds the whole session: the instant the session's running cumulative
  // cost reaches limit the in-flight statement aborts 54P02, and once spent every further statement is
  // rejected 54P02 at admission. The multi-tenant / untrusted-host gate atop maxCost; a handle
  // setting, not stored in the file.
  setLifetimeMaxCost(limit: bigint): void {
    this.session.lifetime.limit = limit;
  }

  // lifetimeMaxCost is the default session's per-session cumulative cost budget (0n ⇒ unlimited).
  lifetimeMaxCost(): bigint {
    return this.session.lifetime.limit;
  }

  // lifetimeCost is the default session's running CUMULATIVE execution cost so far
  // (spec/design/session.md §5.4) — the gauge the lifetime_max_cost budget bounds. Tracked even when
  // unlimited; survives a transaction rollback (session state, not snapshot state).
  lifetimeCost(): bigint {
    return this.session.lifetime.total;
  }

  // setDefaultPrivileges replaces the default session's default table-privilege set — the
  // GRANT … ON ALL TABLES default (spec/design/session.md §5.3). PrivilegeSet.empty().with("select")
  // makes the session read-only (a write resolves to 42501). A handle setting, not stored in the file.
  setDefaultPrivileges(privs: PrivilegeSet): void {
    this.session.privileges.setDefaultTable(privs);
  }

  // grant grants privs on a specific object (table or function) on the default session (§5.3).
  grant(privs: PrivilegeSet, object: string): void {
    this.session.privileges.grant(privs, object);
  }

  // revoke revokes privs from a specific object on the default session (revoke wins, §5.3).
  revoke(privs: PrivilegeSet, object: string): void {
    this.session.privileges.revoke(privs, object);
  }

  // resetPrivileges resets the default session's authorization envelope to fully permissive — every
  // table privilege, no per-object delta, DDL allowed (§5.3). The conformance harness calls this
  // before each record so a # default_privileges: / # grant: / # revoke: / # allow_ddl: directive
  // never leaks past the record it decorates.
  resetPrivileges(): void {
    this.session.privileges = new Privileges();
    this.session.allowDdl = true;
    // The temp-DDL gates are part of the authorization envelope (temp-tables.md §5); reset them with
    // the rest so a # allow_temp_ddl: / # allow_shared_temp_ddl: directive never leaks past its record.
    // Default-inherit allowDdl=true.
    this.session.allowTempDdl = true;
    this.session.allowSharedTempDdl = true;
  }

  // privileges is read-only access to the default session's authorization envelope (§5.3).
  privileges(): Privileges {
    return this.session.privileges;
  }

  // setAllowDdl sets whether DDL is permitted on the default session (§5.3); a denied change is 42501.
  setAllowDdl(allow: boolean): void {
    this.session.allowDdl = allow;
  }

  // allowDdl reports whether DDL is permitted on the default session.
  allowDdl(): boolean {
    return this.session.allowDdl;
  }

  // setAllowTempDdl sets whether session-local temporary-table DDL is permitted on the default session
  // (spec/design/temp-tables.md §5) — the temp-scoped split of allowDdl; a denied temp DDL is 42501.
  setAllowTempDdl(allow: boolean): void {
    this.session.allowTempDdl = allow;
  }

  // allowTempDdl reports whether session-local temporary-table DDL is permitted on the default session.
  allowTempDdl(): boolean {
    return this.session.allowTempDdl;
  }

  // setAllowSharedTempDdl sets whether DATABASE-WIDE shared temporary-table DDL is permitted on the
  // default session (spec/design/temp-tables.md §5) — the shared-temp split of allowDdl, the more
  // privileged of the two temp gates; a denied shared-temp DDL is 42501.
  setAllowSharedTempDdl(allow: boolean): void {
    this.session.allowSharedTempDdl = allow;
  }

  // allowSharedTempDdl reports whether shared temporary-table DDL is permitted on the default session.
  allowSharedTempDdl(): boolean {
    return this.session.allowSharedTempDdl;
  }

  // setTempBuffers sets the default session's per-session temp-table storage budget in BYTES
  // (spec/design/temp-tables.md §7); 0 ⇒ unlimited. An over-budget temp write aborts 54P03.
  setTempBuffers(bytes: number): void {
    this.session.tempBuffers = bytes;
  }

  // tempBuffers reports the default session's per-session temp-table storage budget (0 ⇒ unlimited).
  tempBuffers(): number {
    return this.session.tempBuffers;
  }

  // setSharedTempMem sets the GLOBAL shared-temp storage budget in BYTES (shared_temp_mem,
  // spec/design/temp-tables.md §7); 0 ⇒ unlimited. A Database-level setting (shared temp data is
  // global); an over-budget shared write aborts 54P03. Read the budget back via the public
  // `sharedTempMem` field (a field, not a method — TS forbids a same-named getter).
  setSharedTempMem(bytes: number): void {
    this.sharedTempMem = bytes;
  }

  // setVar sets a session variable on the default session (spec/design/session.md §6.1). Custom
  // variables must be namespaced (a dotted name); a non-dotted name throws 42704. Read it back in SQL
  // with current_setting('name'[, missing_ok]).
  setVar(name: string, value: string): void {
    this.session.setVar(name, value);
  }

  // resetVar clears a session variable on the default session (§6.1); a non-dotted name throws 42704.
  resetVar(name: string): void {
    this.session.resetVar(name);
  }

  // var reads a session variable's value on the default session (§6.1), or undefined if it is not set.
  var(name: string): string | undefined {
    return this.session.var(name);
  }

  // resetVars clears every session variable on the default session (§6.1) — PostgreSQL's RESET ALL for
  // the variable map (also the conformance harness # set: reset hook).
  resetVars(): void {
    this.session.resetVars();
  }

  // setTimeZone sets the time zone on the default session (spec/design/session.md §6.2, timezones.md
  // §9.4): the zone a timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts.
  // Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; else 22023.
  setTimeZone(zone: string): void {
    this.session.setTimeZone(zone);
  }

  // setMaxSqlLength sets the maximum input SQL length, in bytes, accepted on this handle (CLAUDE.md
  // §13; spec/design/api.md §8). A statement whose text exceeds bytes is rejected with 54000 at
  // parse entry, before lexing — the §13 input-size gate (cost.md §7). 0 is unlimited (a trusted
  // caller's opt-out); the default is DEFAULT_MAX_SQL_LENGTH (1 MiB). A handle setting, not stored
  // in the file (mirrors setMaxCost).
  setMaxSqlLength(bytes: number): void {
    this.session.maxSqlLength = bytes;
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
    const max = this.session.maxSqlLength;
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
    this.session.seam.randomFill = f;
  }

  clearRandomSource(): void {
    this.session.seam.randomFill = undefined;
  }

  // setClockSource injects a clock source for uuidv7 (entropy.md §6) — e.g. fixedClock (the # clock:
  // directive). clearClockSource returns to the wall clock.
  setClockSource(f: ClockFunc): void {
    this.session.seam.clock = f;
  }

  clearClockSource(): void {
    this.session.seam.clock = undefined;
  }

  // setWorkMem sets the work-memory budget (in bytes) for blocking operators run on this handle
  // (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly
  // this many bytes of rows resident before it spills sorted runs to disk. 0 is unlimited (never
  // spill). It never changes what a query observes (results + cost are invariant — spill.md §6),
  // only when an operator spills; an in-memory database ignores it. A handle setting, not stored in
  // the file (mirrors setMaxCost).
  setWorkMem(bytes: number): void {
    this.session.workMem = bytes;
  }

  // readSnap is the snapshot a read sees: the read pin if one is set (a data-modifying WITH statement
  // pins the pre-statement snapshot so every sub-statement reads it — writable-cte.md §2), else the
  // open transaction's working (read-your-writes for a writable tx; the pinned snapshot for a
  // read-only tx), else the committed snapshot.
  private readSnap(): Snapshot {
    if (this.session.readPin !== null) {
      return this.session.readPin;
    }
    return this.session.tx !== null ? this.session.tx.working : this.committed;
  }

  // working is the snapshot a write mutates — the open transaction's working. A write only ever
  // runs with a transaction open (autocommit opens one implicitly), so tx is non-null here.
  private working(): Snapshot {
    // Mark the main image dirty so the commit knows to persist it; a temp-only transaction never
    // reaches here (it writes via the temp funnels) and so makes zero file writes (temp-tables.md §2).
    this.session.tx!.mainDirty = true;
    return this.session.tx!.working;
  }

  // tempSnap is the session's temp-table snapshot for READS (spec/design/temp-tables.md §2): the open
  // transaction's tempWorking, else the session's committed temp state. The temp analogue of readSnap
  // (it does not consult readPin — a writable-CTE pins only the main snapshot).
  private tempSnap(): Snapshot {
    return this.session.tx !== null ? this.session.tx.tempWorking : this.session.tempCommitted;
  }

  // sharedTempSnap is the DATABASE-WIDE shared temp-table snapshot for READS (temp-tables.md §4/§5):
  // the open transaction's sharedTempWorking, else the handle's sharedTempCommitted. The shared
  // analogue of tempSnap.
  private sharedTempSnap(): Snapshot {
    return this.session.tx !== null ? this.session.tx.sharedTempWorking : this.sharedTempCommitted;
  }

  // isTempTable reports whether name resolves to a SESSION-LOCAL temporary table in the visible temp
  // snapshot (spec/design/temp-tables.md §3). Preclude-overlaps guarantees a name is temp XOR
  // persistent, so this is the routing predicate the table/store funnels use.
  private isTempTable(name: string): boolean {
    return this.tempSnap().table(name) !== undefined;
  }

  // isSharedTempTable reports whether name resolves to a DATABASE-WIDE shared temporary table in the
  // visible shared-temp snapshot (temp-tables.md §3). Checked AFTER session-local in the resolution
  // walk (session-local → shared → persistent); preclude-overlaps keeps a name in at most one scope.
  private isSharedTempTable(name: string): boolean {
    return this.sharedTempSnap().table(name) !== undefined;
  }

  // compositeDependentAny is the DROP TYPE … RESTRICT dependency check across EVERY visible scope
  // (spec/design/temp-tables.md §8): the main image (tables + composite fields), then the visible
  // session-local and shared temp snapshots (their tables). A composite type is always persistent, but
  // a TEMP table column may reference it, so dropping the type while such a temp table exists is 2BP01
  // — matching the persistent case (PostgreSQL blocks the drop). A session sees only its own
  // session-local temp tables plus the shared ones, so the check is scoped to what is visible (another
  // session's private temp table is invisible by design — and its resolved ColType is self-contained,
  // so it keeps working regardless).
  private compositeDependentAny(name: string): string | null {
    return (
      this.readSnap().compositeDependent(name) ??
      this.tempSnap().compositeDependent(name) ??
      this.sharedTempSnap().compositeDependent(name)
    );
  }

  // isTempIndex reports whether name is a secondary index on a SESSION-LOCAL temp table
  // (spec/design/temp-tables.md §8) — the index analogue of isTempTable, used to gate (allowTempDdl)
  // and route a DROP INDEX of a temp index. Preclude-overlaps keeps an index name in one scope.
  private isTempIndex(name: string): boolean {
    return this.tempSnap().findIndex(name) !== null;
  }

  // isSharedTempIndex reports whether name is a secondary index on a DATABASE-WIDE shared temp table
  // (temp-tables.md §8) — the index analogue of isSharedTempTable; checked AFTER the session-local
  // index (the resolution walk).
  private isSharedTempIndex(name: string): boolean {
    return this.sharedTempSnap().findIndex(name) !== null;
  }

  // sequence resolves a sequence by name along the resolution walk session-local → shared → persistent
  // (spec/design/sequences.md + temp-tables.md §8). Preclude-overlaps keeps a name in at most one scope
  // (the shared relation namespace), so this is just "where the sequence lives". Every sequence READ
  // (nextval/currval/setval resolution, DROP/ALTER SEQUENCE) goes through here, so a serial/IDENTITY
  // column's OWNED temp sequence resolves exactly like a persistent one.
  private sequence(name: string): SequenceDef | undefined {
    return (
      this.tempSnap().sequence(name) ??
      this.sharedTempSnap().sequence(name) ??
      this.readSnap().sequence(name)
    );
  }

  // isTempSequence reports whether name is a sequence in the SESSION-LOCAL temp snapshot
  // (temp-tables.md §8) — the sequence analogue of isTempTable. A temp sequence only ever arises from a
  // serial/IDENTITY temp column (standalone CREATE SEQUENCE is always persistent), so it is always owned.
  private isTempSequence(name: string): boolean {
    return this.tempSnap().sequence(name) !== undefined;
  }

  // isSharedTempSequence reports whether name is a sequence in the DATABASE-WIDE shared temp snapshot
  // (temp-tables.md §8) — checked AFTER session-local (the resolution walk).
  private isSharedTempSequence(name: string): boolean {
    return this.sharedTempSnap().sequence(name) !== undefined;
  }

  // putSequenceRouted stages a sequence def into whichever scope currently owns its name (flagging the
  // matching dirty bit): session-local temp, shared temp, else the main working set. A serial/IDENTITY
  // temp column's owned sequence advances (nextval flush) into its temp snapshot — like the table's
  // rows, zero file writes (temp-tables.md §2); a brand-new persistent sequence is absent from both temp
  // scopes and lands in the main image.
  private putSequenceRouted(def: SequenceDef): void {
    if (this.isTempSequence(def.name)) {
      this.session.tx!.tempDirty = true;
      this.session.tx!.tempWorking.putSequence(def);
    } else if (this.isSharedTempSequence(def.name)) {
      this.session.tx!.sharedTempDirty = true;
      this.session.tx!.sharedTempWorking.putSequence(def);
    } else {
      this.working().putSequence(def);
    }
  }

  // removeSequenceRouted removes a sequence from whichever scope owns its name (the routed analogue of
  // putSequenceRouted). Used by DROP SEQUENCE and DROP TABLE's owned-sequence auto-drop.
  private removeSequenceRouted(name: string): void {
    const key = name.toLowerCase();
    if (this.isTempSequence(name)) {
      this.session.tx!.tempDirty = true;
      this.session.tx!.tempWorking.removeSequence(key);
    } else if (this.isSharedTempSequence(name)) {
      this.session.tx!.sharedTempDirty = true;
      this.session.tx!.sharedTempWorking.removeSequence(key);
    } else {
      this.working().removeSequence(key);
    }
  }

  // setColumnDefaultExprRouted rewrites a column's stored DEFAULT expression in whichever scope owns the
  // table — the routed analogue used by ALTER SEQUENCE … RENAME of an owned sequence (temp-tables.md §8),
  // so a renamed owned TEMP sequence's nextval default is rewritten in the temp snapshot.
  private setColumnDefaultExprRouted(tableKey: string, column: number, de: DefaultExpr): void {
    if (this.isTempTable(tableKey)) {
      this.session.tx!.tempDirty = true;
      this.session.tx!.tempWorking.setColumnDefaultExpr(tableKey, column, de);
    } else if (this.isSharedTempTable(tableKey)) {
      this.session.tx!.sharedTempDirty = true;
      this.session.tx!.sharedTempWorking.setColumnDefaultExpr(tableKey, column, de);
    } else {
      this.working().setColumnDefaultExpr(tableKey, column, de);
    }
  }

  // lkpTable resolves a table by name along the resolution walk session-local → shared → persistent
  // (temp-tables.md §3). Preclude-overlaps keeps a name in at most one scope, so this is just "where it lives".
  private lkpTable(name: string): Table | undefined {
    return (
      this.tempSnap().table(name) ??
      this.sharedTempSnap().table(name) ??
      this.readSnap().table(name)
    );
  }

  // lkpStore returns a table's store for READS, routing by the resolution walk (session-local temp →
  // shared temp → visible main snapshot — temp-tables.md §2/§4). No dirty flag — reads never persist.
  private lkpStore(name: string): TableStore {
    if (this.isTempTable(name)) return this.tempSnap().store(name);
    if (this.isSharedTempTable(name)) return this.sharedTempSnap().store(name);
    return this.readSnap().store(name);
  }

  // writeStore returns a table's store for MUTATION, routing a session-local temp write to tempWorking
  // (flagging tempDirty), a shared temp write to sharedTempWorking (flagging sharedTempDirty), and a
  // persistent write to working (which flags mainDirty) — so a pure-temp transaction leaves the main
  // image untouched (temp-tables.md §2).
  private writeStore(name: string): TableStore {
    if (this.isTempTable(name)) {
      this.session.tx!.tempDirty = true;
      return this.session.tx!.tempWorking.store(name);
    }
    if (this.isSharedTempTable(name)) {
      this.session.tx!.sharedTempDirty = true;
      return this.session.tx!.sharedTempWorking.store(name);
    }
    return this.working().store(name);
  }

  // lkpIndexStore returns a secondary index's store for READS, walking session-local → shared → main
  // (temp-tables.md §8).
  private lkpIndexStore(nameKey: string): TableStore {
    if (this.tempSnap().hasIndexStore(nameKey)) {
      return this.tempSnap().indexStore(nameKey);
    }
    if (this.sharedTempSnap().hasIndexStore(nameKey)) {
      return this.sharedTempSnap().indexStore(nameKey);
    }
    return this.readSnap().indexStore(nameKey);
  }

  // writeIndexStore returns a secondary index's store for MUTATION, walking session-local → shared →
  // main (flagging the matching dirty bit).
  private writeIndexStore(nameKey: string): TableStore {
    if (this.tempSnap().hasIndexStore(nameKey)) {
      this.session.tx!.tempDirty = true;
      return this.session.tx!.tempWorking.indexStore(nameKey);
    }
    if (this.sharedTempSnap().hasIndexStore(nameKey)) {
      this.session.tx!.sharedTempDirty = true;
      return this.session.tx!.sharedTempWorking.indexStore(nameKey);
    }
    return this.working().indexStore(nameKey);
  }

  // columnCollations resolves each column's frozen collation (Column.collation, the name) to its
  // baked table, indexed by column ordinal — null for a C / non-text column (the fast path). The key
  // encoders (§2.12) consult colls[ci] to pick a text column's key form.
  private columnCollations(columns: Column[]): (Collation | null)[] {
    const snap = this.readSnap();
    return columns.map((c) =>
      c.collation !== null ? (snap.resolveCollation(c.collation) ?? null) : null,
    );
  }

  // ensureCollationsWritable refuses a WRITE that would maintain a collated B-tree under a
  // version-skewed collation (the slice-2d verdict, spec/design/collation.md §12/§14): if any of
  // columns carries a collation the file pinned to a different (unicode, cldr) than the loaded bundle
  // provides, an insert/update/delete/index-build would mix two orderings in one tree and corrupt it,
  // so the whole table is read-only until a REINDEX migration (deferred) rebuilds + re-pins it. XX002,
  // naming the collation + both versions. Reads never call this — they recompute against the loaded
  // table (the heap-scan fallback, compatibility.md §8). Per-table granularity: one skewed column
  // collation makes the table read-only (finer per-index gating is a follow-on).
  private ensureCollationsWritable(columns: Column[]): void {
    const snap = this.readSnap();
    for (const c of columns) {
      if (c.collation === null) continue;
      const skew = snap.collationSkew(c.collation);
      if (skew !== undefined) {
        const [fu, fc, lu, lc] = skew;
        throw engineError(
          "collation_version_mismatch",
          `collation "${c.collation}" version mismatch: this database's keys were built under ${fu}/${fc} ` +
            `but the loaded bundle is ${lu}/${lc}; tables using it are read-only until a REINDEX migration rebuilds them`,
        );
      }
    }
  }

  // seqNextval is nextval('name') (spec/design/sequences.md §4): advance the named sequence and
  // return the new value. The running state lives in pendingSeq, seeded from the working snapshot on
  // first touch this statement, and is flushed into the working snapshot + sessionSeq on statement
  // success (flushPendingSequences). A missing sequence is 42P01; advancing past a bound without
  // CYCLE is 2200H.
  seqNextval(name: string): bigint {
    const key = name.toLowerCase();
    let def = this.session.pendingSeq.get(key);
    if (def === undefined) {
      const committed = this.sequence(name);
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
    this.session.pendingSeq.set(key, def);
    // nextval defines this session's currval for the sequence AND makes it the lastval target (the
    // most-recent-nextval sequence; lastval then reads its current session value — §6).
    this.session.pendingCurrval.set(key, result);
    this.session.pendingLastName = key;
    return result;
  }

  // seqSetval is setval('name', n) / setval('name', n, isCalled) (spec/design/sequences.md §4): set
  // the sequence's counter directly and return n. A missing sequence is 42P01; n outside
  // [minValue, maxValue] is 22003. lastValue = n, isCalled = the flag (default true); when isCalled
  // is true the value also defines this session's currval (PG: isCalled=false leaves currval
  // untouched). setval never updates lastval (PG — §6).
  seqSetval(name: string, n: bigint, isCalled: boolean): bigint {
    const key = name.toLowerCase();
    let def = this.session.pendingSeq.get(key);
    if (def === undefined) {
      const committed = this.sequence(name);
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
    this.session.pendingSeq.set(key, def);
    // currval is defined only when isCalled (PG do_setval: elm->last_valid set iff iscalled).
    if (isCalled) this.session.pendingCurrval.set(key, n);
    return n;
  }

  // seqLastval is lastval() (spec/design/sequences.md §6): the CURRENT session value of the sequence
  // the most recent nextval (of any sequence) ran on IN THIS SESSION — PG reads the last-used
  // sequence's cached value, so a setval on that same sequence is reflected, while a setval on a
  // different sequence is not. Takes no name argument (no 42P01); 55000 before the first nextval. The
  // effective name and its value both honor the statement's running updates over the session state.
  seqLastval(): bigint {
    const key = this.session.pendingLastName ?? this.session.sessionLastName;
    if (key === null) {
      throw engineError(
        "object_not_in_prerequisite_state",
        "lastval is not yet defined in this session",
      );
    }
    const pending = this.session.pendingCurrval.get(key);
    if (pending !== undefined) return pending;
    const v = this.session.sessionSeq.get(key);
    if (v !== undefined) return v;
    // A nextval always defines the sequence's session value, so a recorded last-name with no value is
    // unreachable; fall back to 55000 defensively rather than returning a wrong value.
    throw engineError(
      "object_not_in_prerequisite_state",
      "lastval is not yet defined in this session",
    );
  }

  // seqCurrval is currval('name') (spec/design/sequences.md §6): the value nextval/setval(…,true)
  // last produced for this sequence IN THIS SESSION. Resolves the name against the catalog first
  // (42P01 if absent), then reads the running update this statement (pendingCurrval) else the session
  // value (sessionSeq); 55000 if it has not been defined this session.
  seqCurrval(name: string): bigint {
    if (this.sequence(name) === undefined) {
      throw engineError("undefined_table", `relation does not exist: ${name}`);
    }
    const key = name.toLowerCase();
    const pending = this.session.pendingCurrval.get(key);
    if (pending !== undefined) return pending;
    const v = this.session.sessionSeq.get(key);
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
    for (const def of this.session.pendingSeq.values()) {
      // Route each advance to its owning scope (temp-tables.md §8): a serial/IDENTITY temp column's
      // owned sequence flushes into its temp snapshot (zero file writes), a persistent one into main.
      this.putSequenceRouted(def);
    }
    for (const [key, v] of this.session.pendingCurrval) {
      this.session.sessionSeq.set(key, v);
    }
    if (this.session.pendingLastName !== null) {
      this.session.sessionLastName = this.session.pendingLastName;
    }
    this.session.pendingSeq.clear();
    this.session.pendingCurrval.clear();
    this.session.pendingLastName = null;
  }

  // restoreSessionState restores the handle's currval/lastval session state from a discarded
  // transaction's captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block
  // nextval/setval session updates. Called wherever a transaction is dropped without publishing.
  private restoreSessionState(tx: ActiveTx): void {
    this.session.sessionSeq = tx.savedSessionSeq;
    this.session.sessionLastName = tx.savedSessionLastName;
  }

  // newTx opens a transaction over a clone of the committed snapshot, capturing the handle's
  // currval/lastval session state so it can be restored if the transaction is discarded (the
  // rollback of any in-block nextval/setval session updates — spec/design/sequences.md §5/§6).
  private newTx(writable: boolean): ActiveTx {
    return {
      writable,
      failed: false,
      working: this.committed.clone(),
      tempWorking: this.session.tempCommitted.clone(),
      sharedTempWorking: this.sharedTempCommitted.clone(),
      mainDirty: false,
      tempDirty: false,
      sharedTempDirty: false,
      savedSessionSeq: new Map(this.session.sessionSeq),
      savedSessionLastName: this.session.sessionLastName,
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

  // inTransaction reports whether an explicit transaction block is currently open on the DEFAULT
  // session (spec/design/transactions.md §4.2). False under autocommit. Used by the host Transaction
  // surface.
  inTransaction(): boolean {
    return this.session.tx !== null;
  }

  // status reports the DEFAULT session's transaction status (Idle/Open/Failed, spec/design/session.md
  // §2.2) — the explicit three-state machine the convenience methods drive.
  status(): TxStatus {
    return txStatusOf(this.session.tx);
  }

  // newSession mints an ADDITIONAL independent session over this database (spec/design/session.md
  // §2.1), configured from opts. The new session has its own settings, transaction status, and
  // sequence state; the committed storage is shared. On a single-threaded handle, additional sessions
  // run sequentially — a statement is issued through Session.execute, which swaps the session into the
  // active slot for the call. The bare Database keeps its long-lived default session.
  newSession(opts: SessionOptions = {}): Session {
    return new Session(opts);
  }

  // executeScript runs a multi-statement sql SCRIPT on the default session (spec/design/session.md
  // §4.2): split it, run each statement in order, DISCARD the result rows (keeping only counts), and
  // return the O(1) ScriptSummary. The dominant migration/import path — "run this script; I only
  // care that it succeeded."
  //
  //   - Idle at entry  ⇒ the whole run is one implicit transaction, all-or-nothing: a statement
  //     error rolls the wrapper back (nothing is committed) and rethrows that error.
  //   - Open at entry  ⇒ the run joins that transaction (no wrapper, no auto-commit); a mid-run error
  //     leaves the block Failed for the caller to roll back.
  //   - In-script transaction control (BEGIN/COMMIT/ROLLBACK) is 0A000 — the implicit wrapper owns
  //     the boundary (partitioning is deferred, session.md §11); a host that needs self-managed
  //     transactions writes its own splitStatements loop instead.
  executeScript(sql: string): ScriptSummary {
    // We own an implicit wrapper iff the session is Idle at entry. beginTx(null) honors the handle's
    // read-only mode (READ ONLY wrapper on a read-only handle — a write inside is 25006).
    const ownsWrapper = !this.inTransaction();
    if (ownsWrapper) this.beginTx(null);
    try {
      const summary = this.runScriptBody(sql);
      if (ownsWrapper) this.commitTx(); // publish the all-or-nothing run
      return summary;
    } catch (e) {
      if (ownsWrapper) this.rollbackTx(); // discard everything; rethrow the original error
      throw e;
    }
  }

  // runScriptBody splits sql and runs each statement on the current transaction, accumulating the
  // ScriptSummary. Separated so executeScript's wrapper commit/rollback runs once on either path.
  private runScriptBody(sql: string): ScriptSummary {
    let statementsRun = 0;
    let rowsAffectedTotal = 0;
    let cost = 0n;
    for (const span of splitStatements(sql)) {
      const ast = this.parse(span.text);
      // Transaction control inside a script is the v1 narrowing (session.md §4.2): the implicit
      // wrapper owns the boundary, so BEGIN/COMMIT/ROLLBACK is 0A000 (partitioning deferred).
      if (ast.kind === "begin" || ast.kind === "commit" || ast.kind === "rollback") {
        throw engineError(
          "feature_not_supported",
          "transaction control (BEGIN/COMMIT/ROLLBACK) is not supported inside execute_script; " +
            "use splitStatements to run a self-managed multi-statement transaction",
        );
      }
      const out = this.executeStmtParams(ast, []);
      statementsRun++;
      if (out.kind === "statement" && out.rowsAffected !== null)
        rowsAffectedTotal += out.rowsAffected;
      cost += out.cost;
    }
    return { statementsRun, rowsAffectedTotal, cost };
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

  // resolveCollationByName resolves a collation for USE — the database's resolved set then the
  // engine-global loaded set (spec/design/collation.md §2/§9). The reference-only read path; undefined
  // ⇒ neither has it (resolver → 42704).
  resolveCollationByName(name: string): Collation | undefined {
    return this.readSnap().resolveCollation(name);
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

  // importCollation / exportCollation are GONE (the reference-only pivot, spec/design/collation.md
  // §4.2): a collation is provided by a host-loaded bundle and used by name, never loaded into a
  // database. There is no runtime path that constructs or bakes a collation table — the only load is
  // loadUnicodeData of jed's own pinned bundle bytes.

  // loadUnicodeData loads a JUCD Unicode-data bundle (db.loadUnicodeData, spec/design/collation.md
  // §4.2): its collations become resolvable by name for COLLATE, per-column collation, and ORDER BY …
  // COLLATE. The loaded set is ENGINE-GLOBAL (§9), so a bundle loaded through any handle is visible
  // everywhere — including to a later Database.open of a file that REFERENCES one of its collations.
  // Privileged host op (not SQL-reachable, no path, no engine I/O — §11); ADDITIVE and idempotent for
  // an already-loaded bundle. Browser-safe (Uint8Array, no node:fs). A malformed bundle is XX001.
  // (Mirrors the free loadUnicodeData, which the host may call before opening any file.)
  loadUnicodeData(data: Uint8Array): void {
    loadUnicodeDataGlobal(data);
  }

  // loadTimeZoneData loads a JTZ time-zone bundle into the engine-global loaded set
  // (db.loadTimeZoneData, spec/design/timezones.md §3.3). The bytes are jed's own pinned TZif (RFC
  // 8536) wrapped in a manifest; the loaded zones become usable by AT TIME ZONE. Like the collation
  // seam, this is a privileged host op (not SQL-reachable, no path, no engine I/O — §10), additive and
  // idempotent, engine-global so it may be called before open. Browser-safe. A malformed bundle is
  // XX001. (UTC and fixed offsets are built in and need no load.)
  loadTimeZoneData(data: Uint8Array): void {
    loadTimeZoneDataGlobal(data);
  }

  // loadedTimeZones introspects the engine-global loaded zone set (db.loadedTimeZones, timezones.md
  // §3.3) — every named zone (and alias) a loaded bundle provides, ascending by name. A property of
  // the running engine, not of this database. UTC and fixed offsets are built in and not listed.
  loadedTimeZones(): TimeZoneInfo[] {
    return loadedTimeZonesGlobal();
  }

  // loadedCollations introspects the engine-global LOADED collation set (db.loadedCollations,
  // spec/design/collation.md §4.2) — every collation a loaded bundle provides, available to any
  // database on this handle, ascending by name. A property of the running ENGINE, not of this
  // database; for the collations this database references, use Database.collations. isDefault is
  // always false here (that is a per-database property). C is built in and not listed.
  loadedCollations(): CollationInfo[] {
    return loadedCollationTables().map((c) => ({
      name: c.name,
      unicodeVersion: c.unicodeVersion,
      cldrVersion: c.cldrVersion,
      contentHash: crc32Ieee(serializeTable(c)),
      description: c.description,
      isDefault: false,
      // The loaded set IS the version reference — it can never be skewed against itself.
      verdict: "full" as const,
    }));
  }

  // setDefaultCollation sets the per-database default collation (db.setDefaultCollation,
  // spec/design/collation.md §1). "C" resets to byte order; any other name must be a LOADED
  // collation (else 42704). Persisted as the is_default flag on that collation's reference entry at
  // the next commit (the entry is emitted because the default references it — §5).
  setDefaultCollation(name: string): void {
    if (name === "C") {
      this.committed.defaultCollation = null;
      return;
    }
    if (this.committed.resolveCollation(name) === undefined) {
      throw engineError("undefined_object", `collation "${name}" does not exist`);
    }
    this.committed.defaultCollation = name;
  }

  // defaultCollation returns the per-database default collation name — "C" unless setDefaultCollation
  // moved it (db.defaultCollation, spec/design/collation.md §1).
  defaultCollation(): string {
    return this.committed.defaultCollation ?? "C";
  }

  // upgradeCollations adopts a newly-loaded Unicode version for this database's skewed collations
  // (the REINDEX / COLLATION UPGRADE migration, spec/design/collation.md §12). A privileged host op
  // like setDefaultCollation — NOT SQL-reachable, so an untrusted query can never trigger it
  // (CLAUDE.md §13). For every collation whose file pin differs from the loaded bundle (Skewed) it
  // rebuilds the collated keys (PK + indexes) under the loaded table and re-pins the stamp, clearing
  // the skew so the affected tables are read-write again and regain collated-index pushdown.
  // Whole-database + atomic (the rebuild stages in a snapshot clone swapped in only on success);
  // idempotent (no skew ⇒ a no-op returning 0). Persisted by the next explicit commit. Returns the
  // number of collations re-pinned.
  upgradeCollations(): number {
    const work = this.committed.clone();
    const n = work.upgradeCollations(this.pageSize);
    if (n > 0) this.committed = work;
    return n;
  }

  // collations introspects the collations THIS DATABASE references (db.collations,
  // spec/design/collation.md §4.2) — every collation its schema uses (a column's COLLATE, or the
  // per-database default), in ascending name order. This is the per-file view; for the engine-global
  // LOADED set, use Database.loadedCollations. C is built in and not listed.
  collations(): CollationInfo[] {
    const dflt = this.committed.defaultCollation;
    // referencedCollations resolves each referenced name (from a loaded bundle).
    let refs: Collation[];
    try {
      refs = this.committed.referencedCollations();
    } catch {
      return [];
    }
    return refs.map((c) => ({
      name: c.name,
      unicodeVersion: c.unicodeVersion,
      cldrVersion: c.cldrVersion,
      contentHash: crc32Ieee(serializeTable(c)),
      description: c.description,
      isDefault: dflt === c.name,
      // The slice-2d verdict: "skewed" when the file's pin differs from the loaded bundle's version
      // (the object is read-only), else "full" (collation.md §12).
      verdict: (this.committed.collationSkew(c.name) !== undefined
        ? "skewed"
        : "full") as CollationVerdict,
    }));
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
    this.session.pendingSeq.clear();
    this.session.pendingCurrval.clear();
    this.session.pendingLastName = null;

    // Inside an explicit block?
    const tx = this.session.tx;
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
        // Enforce the temp-storage budgets after a successful temp write (temp-tables.md §7): an
        // over-budget statement (session-local tempBuffers OR global sharedTempMem) throws 54P03, which
        // aborts the block (the staged temp rows roll back at ROLLBACK). A no-op for non-temp statements.
        this.checkTempBudget();
        this.checkSharedTempBudget();
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
    this.session.tx = this.newTx(true);
    let outcome: Outcome;
    try {
      outcome = this.dispatchStmt(stmt, params);
      // Enforce the temp-storage budgets before committing (temp-tables.md §7): an over-budget temp
      // write in this implicit transaction (session-local tempBuffers OR global sharedTempMem) is
      // discarded (rolling back temp + main) and surfaces 54P03.
      this.checkTempBudget();
      this.checkSharedTempBudget();
    } catch (e) {
      // The statement failed before any flush, so session state is untouched; restore from the
      // captured copy anyway to keep the discard path uniform (sequences.md §6).
      this.restoreSessionState(this.session.tx);
      this.session.tx = null;
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
    if (this.session.tx !== null) {
      throw engineError("active_sql_transaction", "there is already a transaction in progress");
    }
    if (writable === true && this.readOnly) {
      throw engineError(
        "read_only_sql_transaction",
        "cannot set transaction read-write mode on a read-only database",
      );
    }
    this.session.tx = this.newTx(writable ?? !this.readOnly);
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
    const tx = this.session.tx;
    if (tx === null) return { kind: "statement", cost: 0n, rowsAffected: null };
    this.session.tx = null;
    if (tx.failed || !tx.writable) {
      // A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG), so any
      // in-block session updates revert with the discarded working set (§5/§6). The discarded
      // tempWorking rolls back temp changes too.
      this.restoreSessionState(tx);
      return { kind: "statement", cost: 0n, rowsAffected: null };
    }
    const working = tx.working;
    // Persist the main image when it changed; a transaction that touched ONLY temp tables (session-
    // local and/or shared) skips it entirely so a temp table makes ZERO file writes (spec/design/
    // temp-tables.md §2). An empty block (no kind dirty) still persists, preserving prior behavior.
    // Temp state is adopted regardless — never serialized, only swapped into the in-memory committed
    // temp snapshots (the shared root is then published by the shared layer — the two-root commit, §5).
    const pureTemp = !tx.mainDirty && (tx.tempDirty || tx.sharedTempDirty);
    if (!pureTemp) {
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
    }
    // Adopt the transaction's temp changes into the committed temp snapshots (temp-tables.md §5) — the
    // temp analogue of publishing committed, but purely in memory. Session-local temp lives on the
    // session; shared temp lives on the handle (and is published to the shared root by the shared
    // layer's WriteHandle.commit, the two-root commit).
    this.session.tempCommitted = tx.tempWorking;
    this.sharedTempCommitted = tx.sharedTempWorking;
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // rollbackTx rolls back the current transaction (spec/design/transactions.md §4.2). With no open
  // block it is a no-op success. Otherwise the working snapshot is dropped — every staged
  // INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
  // committed was never mutated, so there is nothing to restore there. The handle's currval/lastval
  // session state, however, was updated in place by in-block nextval/setval, so it is restored from
  // the block's captured copy (sequences.md §5/§6).
  rollbackTx(): Outcome {
    if (this.session.tx !== null) this.restoreSessionState(this.session.tx);
    this.session.tx = null;
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // dispatchStmt routes one parsed statement to its executor. The autocommit transaction
  // handling (capture / durable commit / rollback-on-error) lives in executeStmtParams.
  // checkPrivileges enforces the session's authorization envelope for stmt (spec/design/session.md
  // §5.3). A fully-permissive session (the default) needs no check. Otherwise DDL is gated by
  // allowDdl, and DML requires a per-table privilege for each table it reads (SELECT) or writes
  // (INSERT/UPDATE/DELETE) and EXECUTE for each named function it calls. Enforcement is at name
  // resolution: a table privilege is required only for a name that resolves to an existing catalog
  // table (a missing table stays 42P01; a CTE / derived-table label is statement-local). Missing
  // privilege → 42501.
  // checkLifetimeAdmission rejects a statement at admission when the session's lifetime cost budget is
  // already spent (spec/design/session.md §5.4): if a budget is set and the session's cumulative cost
  // has reached it, no further statement may run (it "cannot accrue") — 54P02. A no-op when the budget
  // is unlimited (the default), so the common path pays one comparison.
  private checkLifetimeAdmission(): void {
    const limit = this.session.lifetime.limit;
    const total = this.session.lifetime.total;
    if (limit > 0n && total >= limit) {
      throw engineError(
        "session_cost_limit_exceeded",
        `session exceeded the lifetime cost limit of ${limit} (accrued ${total})`,
      );
    }
  }

  // checkTempBudget enforces the per-session temp-table storage budget (tempBuffers, spec/design/
  // temp-tables.md §7) — the §13 gate on RETAINED temp bytes. Checked after each temp-writing
  // statement: if the session's temp footprint (byte-identical on-disk record bytes, summed over every
  // temp table + index) EXCEEDS the budget, throw 54P03. The over-budget write is in tempWorking, so
  // the abort discards it (autocommit) or fails the block (rolled back at ROLLBACK) — nothing commits.
  // tempBuffers 0 ⇒ unlimited; a transaction that did not touch temp cannot have grown it, so the check
  // self-gates on tempDirty and is a no-op for ordinary (persistent) statements. Within-statement bound
  // is maxCost.
  private checkTempBudget(): void {
    const limit = this.session.tempBuffers;
    if (limit === 0) return;
    if (this.session.tx === null || !this.session.tx.tempDirty) return;
    const used = this.tempSnap().storageBytes();
    if (used > limit) {
      throw engineError(
        "temp_storage_limit_exceeded",
        `temporary table storage exceeded the limit of ${limit} bytes`,
      );
    }
  }

  // checkSharedTempBudget enforces the GLOBAL shared-temp storage budget (sharedTempMem, spec/design/
  // temp-tables.md §7) — the shared analogue of checkTempBudget, charged against the Database-level
  // budget over the shared-temp footprint. Self-gates on sharedTempDirty (a no-op for any statement
  // that did not write shared temp). The over-budget write is staged, so the abort rolls it back.
  private checkSharedTempBudget(): void {
    const limit = this.sharedTempMem;
    if (limit === 0) return;
    if (this.session.tx === null || !this.session.tx.sharedTempDirty) return;
    const used = this.sharedTempSnap().storageBytes();
    if (used > limit) {
      throw engineError(
        "temp_storage_limit_exceeded",
        `shared temporary table storage exceeded the limit of ${limit} bytes`,
      );
    }
  }

  private checkPrivileges(stmt: Statement): void {
    // Fast path: a session that allows ALL DDL (persistent + both temp kinds) and grants every
    // privilege pays nothing. All three gates must be on, since temp DDL now has its own gates (§5).
    if (
      this.session.allowDdl &&
      this.session.allowTempDdl &&
      this.session.allowSharedTempDdl &&
      this.session.privileges.isPermissive()
    ) {
      return;
    }
    const req: PrivReq = {
      tables: [],
      functions: [],
      isDdl: false,
      isTempDdl: false,
      isSharedTempDdl: false,
    };
    collectStmtPrivs(stmt, req);
    if (req.isDdl) {
      // DDL is gated by the kind of relation it targets (temp-tables.md §5): a shared temp table by
      // allowSharedTempDdl, a session-local temp table by allowTempDdl, everything else (persistent) by
      // allowDdl. A CREATE TABLE is classified statically; the rest by resolving the name — a DROP
      // TABLE / CREATE INDEX by its target table, a DROP INDEX by the index — shared before
      // session-local (the resolution-walk order; preclude-overlaps keeps a name in one scope).
      let allowed: boolean;
      if (
        req.isSharedTempDdl ||
        (stmt.kind === "dropTable" && this.isSharedTempTable(stmt.name)) ||
        (stmt.kind === "createIndex" && this.isSharedTempTable(stmt.table)) ||
        (stmt.kind === "dropIndex" && this.isSharedTempIndex(stmt.name)) ||
        (stmt.kind === "dropSequence" && stmt.names.some((n) => this.isSharedTempSequence(n))) ||
        (stmt.kind === "alterSequence" && this.isSharedTempSequence(stmt.name))
      ) {
        allowed = this.session.allowSharedTempDdl;
      } else if (
        req.isTempDdl ||
        (stmt.kind === "dropTable" && this.isTempTable(stmt.name)) ||
        (stmt.kind === "createIndex" && this.isTempTable(stmt.table)) ||
        (stmt.kind === "dropIndex" && this.isTempIndex(stmt.name)) ||
        (stmt.kind === "dropSequence" && stmt.names.some((n) => this.isTempSequence(n))) ||
        (stmt.kind === "alterSequence" && this.isTempSequence(stmt.name))
      ) {
        allowed = this.session.allowTempDdl;
      } else {
        allowed = this.session.allowDdl;
      }
      if (!allowed) {
        throw engineError(
          "insufficient_privilege",
          "permission denied: DDL is not permitted in this session",
        );
      }
    }
    const snap = this.readSnap();
    for (const t of req.tables) {
      const key = t.name.toLowerCase();
      // Only a name that resolves to an existing catalog table is privilege-checked; a missing one is
      // left to raise 42P01 in execution (existence before authorization).
      if (snap.table(key) !== undefined && !this.session.privileges.allowsTable(key, t.priv)) {
        throw engineError("insufficient_privilege", "permission denied for table " + key);
      }
    }
    for (const fn of req.functions) {
      const key = fn.toLowerCase();
      if (!this.session.privileges.allowsFunction(key)) {
        throw engineError("insufficient_privilege", "permission denied for function " + key);
      }
    }
  }

  private dispatchStmt(stmt: Statement, params: Value[]): Outcome {
    // Lifetime budget admission (spec/design/session.md §5.4): once the session's cumulative cost has
    // reached lifetime_max_cost, every further statement is rejected 54P02 BEFORE it can accrue —
    // checked ahead of privileges/existence, so an exhausted session runs nothing. A no-op when the
    // budget is unlimited (the default). Transaction control (BEGIN/COMMIT/ROLLBACK) never reaches
    // dispatch (handled earlier), so an exhausted session can still close out an open block.
    this.checkLifetimeAdmission();
    // Authorization (spec/design/session.md §5.3): enforce the session's privilege envelope before the
    // statement runs — DDL gated by allowDdl, DML by per-table/per-function privileges, all 42501.
    // Skipped on a fully-permissive session (the default), so the common path pays nothing. The
    // physical access-mode gate (25006) is checked earlier in executeStmtParams, so it wins when both
    // apply.
    this.checkPrivileges(stmt);
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
        return this.executeInsert(stmt, params, EMPTY_CTE_CTX);
      case "select":
        return this.executeSelect(stmt, params);
      case "setOp":
        return this.executeSetOp(stmt, params);
      case "with":
        return this.executeWith(stmt, params);
      case "update":
        return this.executeUpdate(stmt, params, EMPTY_CTE_CTX);
      case "delete":
        return this.executeDelete(stmt, params, EMPTY_CTE_CTX);
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
    // A session-local temporary table (spec/design/temp-tables.md) is built exactly like a persistent
    // one but registered into the session temp snapshot at the end (§2), so it makes zero file writes.
    // FOREIGN KEY on a temp table is deferred this slice (§8) — rejected HERE, before any persistent
    // parent resolves, so the error is a clean 0A000. The other temp narrowings (composite/collated
    // columns, serial/IDENTITY) are checked just before registration, once the columns are built.
    if (ct.temp && ct.fks.length > 0) {
      throw engineError(
        "feature_not_supported",
        "FOREIGN KEY on a temporary table is not yet supported",
      );
    }
    // The relation namespace is shared between tables and indexes (indexes.md §2), so a
    // CREATE TABLE colliding with either kind is the same 42P07 — PG's "relation" word. relationExists
    // is temp-aware, so a temp name collides with temp + persistent alike (preclude-overlaps, §3).
    if (this.relationExists(ct.name)) {
      throw engineError("duplicate_table", "relation already exists: " + ct.name);
    }

    const columns: Column[] = [];
    // pk is the primary-key member ordinals in KEY order (constraints.md §3): the
    // column-level form is the one-member case; the table-level list below records its
    // own order.
    let pk: number[] = [];
    let pkSeen = false;
    // The OWNED sequences a serial column desugars to (spec/design/sequences.md §12), collected
    // during the column walk and staged into the working snapshot only after the whole CREATE TABLE
    // validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
    const pendingSerials: SequenceDef[] = [];
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
      // A serial / bigserial / smallserial pseudo-type (spec/design/sequences.md §12): CREATE TABLE
      // sugar for an integer column that is NOT NULL with a DEFAULT nextval(...) backed by a
      // newly-created OWNED sequence. Here we only resolve the underlying integer type; the
      // desugaring (the owned sequence + default + NOT NULL force) happens below. serial[] is NOT a
      // serial column (it falls to the array branch as an unknown element type — §12.1).
      const serialKind = serialPseudoType(def.typeName);
      let colType: Type;
      let decimal: DecimalTypmod | null;
      const ctype = this.compositeType(def.typeName);
      if (serialKind !== undefined) {
        // A serial column takes no typmod (serial(5) is 42601) and no [] (the array branch).
        if (def.typeMod !== null) {
          throw engineError(
            "syntax_error",
            "type modifier is not allowed for type " + def.typeName,
          );
        }
        colType = scalarT(serialKind);
        decimal = null;
      } else if (def.typeName.endsWith("[]")) {
        // An array column (spec/design/array.md §3). The element type is a scalar or a
        // previously-defined composite (array-of-composite, §12 AC1 — element_type_code 14 + name);
        // a nested-array element and an array typmod (numeric(p,s)[]) stay deferred (0A000).
        const base = def.typeName.slice(0, -2);
        if (def.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier on an array type is not supported yet",
          );
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
      } else if (rangeByName(def.typeName) !== undefined) {
        // A range column (spec/design/ranges.md §3): structural like array, the element carried
        // inline. A range takes no typmod (numrange(10,2) is not a thing — the element is the
        // unconstrained subtype), so a type modifier is rejected.
        if (def.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier on a range type is not supported",
          );
        }
        const rdesc = rangeByName(def.typeName)!;
        colType = rangeT(scalarT(elementScalar(rdesc)));
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
        // §2.9). text/bytea (…-terminated-escape §2.4/§2.6) and decimal (decimal-order-preserving
        // §2.5) are also allowed — their variable-width key encodings are self-delimiting, so they
        // compose in composite keys / index suffixes; an oversized one trips the RECORD_MAX
        // oversized-item 0A000 (PG's btree key limit). interval is the fixed 16-byte span key
        // (interval-span-i128, §2.10), and range the first container key (range-bounds, §2.11 —
        // recursing into the element key with empty/±∞/inclusivity framing). Still rejected 0A000:
        // float (the determinism carve-out, determinism.md §4) and the recursive composite/array
        // containers. timestamp / timestamptz / date share the i64/i32 int-be-signflip key (timestamp.md §6).
        if (
          !typeIsInteger(colType) &&
          !typeIsBoolean(colType) &&
          !typeIsText(colType) &&
          !typeIsBytea(colType) &&
          !typeIsDecimal(colType) &&
          !typeIsUuid(colType) &&
          !typeIsTimestamp(colType) &&
          !typeIsTimestamptz(colType) &&
          !typeIsDate(colType) &&
          !typeIsInterval(colType) &&
          !typeIsRange(colType)
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
      let identityKind: IdentityKind | null = null;
      // A serial pseudo-type OR a GENERATED … AS IDENTITY constraint both desugar to an
      // auto-numbered column: an OWNED sequence + a synthesized DEFAULT nextval(...) + NOT NULL
      // (sequences.md §12/§13). Identity additionally records ALWAYS/BY DEFAULT and gates the
      // column type to i16/i32/i64.
      if (serialKind !== undefined || def.identity !== null) {
        // IDENTITY type gate: the declared column type must be smallint/integer/bigint
        // (sequences.md §13.1). serial's type is the pseudo-type (always integer), so this only
        // bites an identity column written on a non-integer type.
        if (def.identity !== null && !typeIsInteger(colType)) {
          throw engineError(
            "invalid_parameter_value",
            "identity column type must be smallint, integer, or bigint",
          );
        }
        // Conflicts (42601, sequences.md §13.2). An explicit DEFAULT — or a serial type, itself a
        // synthesized default — alongside IDENTITY is "both default and identity"; a serial column
        // with its own explicit DEFAULT is "multiple default values" (the S3 message, unchanged).
        if (def.identity !== null && (def.default !== null || serialKind !== undefined)) {
          throw engineError(
            "syntax_error",
            `both default and identity specified for column ${def.name} of table ${ct.name}`,
          );
        }
        if (serialKind !== undefined && def.default !== null) {
          throw engineError(
            "syntax_error",
            `multiple default values specified for column ${def.name} of table ${ct.name}`,
          );
        }
        // Create the OWNED sequence — a default ascending i64 for serial, or the IDENTITY column's
        // `( seq_options )` (defaulting the same way) — and synthesize the DEFAULT nextval(...)
        // expression default (format_version 8 mechanism).
        const seqName = this.chooseSerialSeqName(ct.name, def.name, pendingSerials);
        const owner: SeqOwner = { table: ct.name, column: columns.length }; // this column's ordinal
        const opts = def.identity !== null ? def.identity.options : emptySeqOptions();
        // The owned sequence's data type follows the column (§14): serial → the pseudo-type,
        // identity → the column type. An explicit `AS` inside the identity `( … )` options conflicts
        // with that — 42601 (PG: "conflicting or redundant options"). serial carries no parsed
        // options, so this only fires for identity.
        if (opts.dataType !== null) {
          throw engineError("syntax_error", "conflicting or redundant options");
        }
        // serial fixes the scalar to its pseudo-type; identity's column type is a gated integer
        // scalar (typeIsInteger above), so colType is always a scalar in the identity branch.
        const seqScalar = serialKind ?? (colType.kind === "scalar" ? colType.scalar : undefined);
        const seqDtype = seqScalar === undefined ? undefined : seqDataTypeForScalar(seqScalar);
        if (seqDtype === undefined) {
          // Unreachable: a serial / identity column is i16/i32/i64 (gated above).
          throw engineError("invalid_parameter_value", "serial / identity column is i16/i32/i64");
        }
        opts.dataType = seqDtype;
        pendingSerials.push(buildSequenceDef(seqName, opts, owner));
        // Render the synthetic default exactly as the parser would the equivalent
        // DEFAULT nextval('<seqName>') (space-joined tokens — the canonical expression-text form),
        // so the in-memory expr matches what reload re-parses. The seqName is a lowercased
        // identifier-derived name, so the quoting is always safe.
        const exprText = `nextval ( '${seqName.replace(/'/g, "''")}' )`;
        def_defaultExpr = { exprText, expr: parseExpression(exprText) };
        if (def.identity !== null) identityKind = def.identity.always ? "always" : "byDefault";
      } else if (
        colType.kind === "composite" ||
        colType.kind === "array" ||
        colType.kind === "range"
      ) {
        // A DEFAULT on a composite-/array-typed column is not supported this slice (composite.md §12
        // / array.md §12); a range column is not storable at all yet (ranges.md §8 — unreachable,
        // CREATE TABLE rejects it).
        if (def.default !== null) {
          throw engineError(
            "feature_not_supported",
            "a DEFAULT on a composite- or array-typed column is not supported yet",
          );
        }
      } else if (def.default !== null) {
        const sty = colType.scalar;
        if (def.default.expr.kind === "literal") {
          def_default = storeValue(
            literalToValue(def.default.expr.literal),
            sty,
            decimal,
            false,
            def.name,
          );
        } else {
          rejectDefaultStructure(def.default.expr);
          const { type: rt } = resolve(
            Scope.empty(this),
            def.default.expr,
            sty,
            { collecting: false, groupKeys: [], specs: [] },
            new ParamTypes(),
          );
          if (!assignableTo(rt, sty)) {
            throw typeError(
              `column ${def.name} is of type ${canonicalName(sty)} but default expression is of type ${rtName(rt)}`,
            );
          }
          def_defaultExpr = {
            exprText: def.default.text,
            expr: def.default.expr,
          };
        }
      }
      // The column's effective collation, frozen now (spec/design/collation.md §1). An explicit
      // COLLATE "name" is text-only (42804) and must name a loaded collation or C (42704); a text
      // column without a clause inherits the per-database default. A C effective collation stores as
      // null (the fast path).
      let collation: string | null = null;
      if (def.collation !== null) {
        if (!typeIsText(colType)) {
          throw typeError(`collations are not supported by type ${typeCanonicalName(colType)}`);
        }
        resolveCollationName(this, def.collation); // validates loaded; 42704 if not
        if (def.collation !== "C") collation = def.collation;
      } else if (typeIsText(colType)) {
        collation = this.readSnap().defaultCollation;
      }
      columns.push({
        name: def.name,
        type: colType,
        decimal,
        primaryKey: def.primaryKey,
        // PRIMARY KEY ⇒ NOT NULL; a serial or IDENTITY column is NOT NULL too (sequences.md §12/§13).
        notNull: def.primaryKey || def.notNull || serialKind !== undefined || def.identity !== null,
        default: def_default,
        defaultExpr: def_defaultExpr,
        identity: identityKind,
        collation,
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
          !typeIsText(ty) &&
          !typeIsBytea(ty) &&
          !typeIsDecimal(ty) &&
          !typeIsUuid(ty) &&
          !typeIsTimestamp(ty) &&
          !typeIsTimestamptz(ty) &&
          !typeIsDate(ty) &&
          !typeIsInterval(ty) &&
          !typeIsRange(ty)
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
          !typeIsText(ty) &&
          !typeIsBytea(ty) &&
          !typeIsDecimal(ty) &&
          !typeIsUuid(ty) &&
          !typeIsTimestamp(ty) &&
          !typeIsTimestamptz(ty) &&
          !typeIsDate(ty) &&
          !typeIsInterval(ty) &&
          !typeIsRange(ty)
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
    const table: Table = {
      name: ct.name,
      columns,
      pk,
      checks: [],
      indexes: [],
      fks: [],
    };
    for (const def of ct.checks) {
      // Structural rejections first (a single pre-walk — a documented micro-order
      // divergence from PG, which interleaves them with name/type resolution): subquery
      // 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
      rejectCheckStructure(def.expr);
      const scope = Scope.single(this, table);
      const { type } = resolve(
        scope,
        def.expr,
        null,
        { collecting: false, groupKeys: [], specs: [] },
        new ParamTypes(),
      );
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
      table.indexes.splice(pos, 0, {
        name,
        columns: ru.cols,
        unique: true,
        kind: "btree",
      });
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
          throw engineError(
            "duplicate_column",
            "column " + cname + " appears twice in foreign key constraint",
          );
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
          throw engineError(
            "undefined_object",
            "there is no primary key for referenced table " + parent.name,
          );
        }
        refs = [...parent.pk];
      } else {
        refs = [];
        for (const cname of fk.refColumns) {
          const idx = columnIndex(parent, cname);
          if (idx < 0) {
            throw engineError(
              "undefined_column",
              "column " + cname + " named in key does not exist",
            );
          }
          if (refs.includes(idx)) {
            throw engineError(
              "duplicate_column",
              "column " + cname + " appears twice in foreign key constraint",
            );
          }
          refs.push(idx);
        }
      }
      // 4. Referencing/referenced count must agree.
      if (local.length !== refs.length) {
        throw engineError(
          "invalid_foreign_key",
          "number of referencing and referenced columns for foreign key disagree",
        );
      }
      // 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
      const nameTakenFk = (n: string): boolean =>
        table.checks.some((c) => c.name.toLowerCase() === n.toLowerCase()) ||
        resolvedFks.some((f) => f.name.toLowerCase() === n.toLowerCase());
      let fkName: string;
      if (fk.name !== null) {
        if (nameTakenFk(fk.name)) {
          throw engineError(
            "duplicate_object",
            "constraint " + fk.name + " for relation " + table.name + " already exists",
          );
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
        throw engineError(
          "invalid_foreign_key",
          "there is no unique constraint matching given keys for referenced table " + parent.name,
        );
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
            "foreign key constraint " +
              fkName +
              " cannot be implemented: key columns " +
              table.columns[li]!.name +
              " and " +
              parent.columns[ri]!.name +
              " are of incompatible types: " +
              typeCanonicalName(table.columns[li]!.type) +
              " and " +
              typeCanonicalName(parent.columns[ri]!.type),
          );
        }
      }
      resolvedFks.push({
        name: fkName,
        columns: local,
        refTable: parent.name,
        refColumns: refs,
        onDelete,
        onUpdate,
      });
    }
    // Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
    resolvedFks.sort((a, b) => {
      const an = a.name.toLowerCase();
      const bn = b.name.toLowerCase();
      return an < bn ? -1 : an > bn ? 1 : 0;
    });
    table.fks = resolvedFks;

    if (ct.temp) {
      // Deferred narrowing on a temp table this slice (spec/design/temp-tables.md §8), a clean 0A000:
      // a collated column (needs the temp snapshot to carry the collation catalog). Plain
      // scalar/array/range/decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE,
      // serial/IDENTITY columns (the OWNED sequence is staged into the same temp snapshot below), and
      // COMPOSITE-typed columns (resolved against the MAIN type catalog just below) are fully supported.
      for (const c of table.columns) {
        if (c.collation !== null) {
          throw engineError(
            "feature_not_supported",
            `COLLATE on temporary-table column ${c.name} is not yet supported`,
          );
        }
      }
      // Resolve each column's ColType against the MAIN snapshot's composite-type catalog
      // (spec/design/temp-tables.md §8): composites are always persistent (CREATE TYPE is persistent
      // DDL), so the temp snapshot's own types map is empty — resolving there would miss a composite
      // reference. The resulting ColType tree is self-contained, so the temp store needs nothing from
      // the catalog after this (composite.md §4).
      const mainTypes = this.readSnap().types;
      const colTypes = table.columns.map((c) => resolveColType(c.type, mainTypes));
      // Register into the matching temp snapshot — never the main image, so the table makes zero file
      // writes (§2). A SHARED table goes in the database-wide shared snapshot (visible to every
      // session, §4); a plain temp table in the session-local one. Flag the matching dirty bit so the
      // commit can skip persisting the main image.
      let ts: Snapshot;
      if (ct.shared) {
        this.session.tx!.sharedTempDirty = true;
        ts = this.session.tx!.sharedTempWorking;
      } else {
        this.session.tx!.tempDirty = true;
        ts = this.session.tx!.tempWorking;
      }
      ts.putTableResolved(table, colTypes, this.pageSize);
      for (const ix of table.indexes) {
        ts.putIndexStore(
          ix.name.toLowerCase(),
          new TableStore(this.pageSize - 12, []), // 12 = PAGE_HEADER
        );
      }
      // Stage each serial/IDENTITY column's OWNED sequence into the SAME temp snapshot
      // (spec/design/sequences.md §12, temp-tables.md §8) — never the main image, so the sequence
      // (like the table) makes zero file writes and is dropped with the table. The names were resolved
      // collision-free during the column walk (relationExists is temp-aware); nextval resolves and
      // advances them via the scope-aware sequence funnel.
      for (const s of pendingSerials) ts.putSequence(s);
      return { kind: "statement", cost: 0n, rowsAffected: null };
    }

    this.putTable(table);
    // The table is brand new (no rows), so each backing index store starts empty.
    for (const ix of table.indexes) {
      this.working().putIndexStore(
        ix.name.toLowerCase(),
        new TableStore(this.pageSize - 12, []), // 12 = PAGE_HEADER
      );
    }
    // Stage each serial column's OWNED sequence now that the table validated
    // (spec/design/sequences.md §12). The names were resolved (collision-free) during the column
    // walk; the table is in the catalog, so a DROP TABLE will auto-drop these.
    for (const s of pendingSerials) this.working().putSequence(s);
    // DDL touches no rows and evaluates no expressions: zero cost.
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // chooseSerialSeqName chooses the auto-generated name for a serial column's OWNED sequence
  // (spec/design/sequences.md §12), matching PostgreSQL: lower(table)_lower(column)_seq, with the
  // smallest integer suffix 1, 2, … appended until the name is free in the relation namespace — not
  // taken by an existing relation, not equal to the table being created, and not already chosen by
  // an earlier serial column of the same statement (pending). All-lowercase identifier-derived.
  private chooseSerialSeqName(table: string, column: string, pending: SequenceDef[]): string {
    const base = `${table.toLowerCase()}_${column.toLowerCase()}_seq`;
    const taken = (c: string): boolean =>
      this.relationExists(c) ||
      c.toLowerCase() === table.toLowerCase() ||
      pending.some((s) => s.name.toLowerCase() === c.toLowerCase());
    if (!taken(base)) return base;
    for (let n = 1; ; n++) {
      const cand = base + n.toString();
      if (!taken(cand)) return cand;
    }
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
      node: resolve(
        scope,
        c.expr,
        null,
        { collecting: false, groupKeys: [], specs: [] },
        new ParamTypes(),
      ).node,
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
      return resolve(
        Scope.empty(this),
        col.defaultExpr.expr,
        typeScalar(col.type),
        { collecting: false, groupKeys: [], specs: [] },
        new ParamTypes(),
      ).node;
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
      seam: this.session.seam,
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
    // A temp table (spec/design/temp-tables.md): remove it from the matching temp snapshot — never the
    // main image, so DROP makes zero file writes. No FK-dependent check (a temp table is never an FK
    // parent, §8), but it DOES auto-drop every sequence OWNED BY the table — a serial/IDENTITY temp
    // column's owned temp sequence (spec/design/sequences.md §12, temp-tables.md §8) — from the SAME
    // temp snapshot. The temp-DDL capability gate already ran at dispatch (§5). Session-local first,
    // then shared.
    if (this.isTempTable(dt.name)) {
      this.session.tx!.tempDirty = true;
      const ts = this.tempSnap();
      for (const sk of ts.sequencesOwnedBy(dt.name)) ts.removeSequence(sk);
      ts.removeTable(dt.name.toLowerCase());
      return { kind: "statement", cost: 0n, rowsAffected: null };
    }
    if (this.isSharedTempTable(dt.name)) {
      this.session.tx!.sharedTempDirty = true;
      const ts = this.sharedTempSnap();
      for (const sk of ts.sequencesOwnedBy(dt.name)) ts.removeSequence(sk);
      ts.removeTable(dt.name.toLowerCase());
      return { kind: "statement", cost: 0n, rowsAffected: null };
    }
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
    // Auto-drop every sequence OWNED BY this table — the serial columns' sequences
    // (spec/design/sequences.md §12). An owned sequence is never an FK dependent, so the check
    // above never blocked on it; the sequences are removed alongside the table.
    const ownedSeqs = this.readSnap().sequencesOwnedBy(dt.name);
    const w = this.working();
    for (const sk of ownedSeqs) w.removeSequence(sk);
    w.removeTable(dt.name.toLowerCase());
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
    // Temp tables (session-local AND shared) + their (UNIQUE) index names join the namespace too, so a
    // name colliding with any temp relation is also 42P07 (preclude-overlaps — spec/design/temp-tables.md
    // §3). this.table is persistent-only, so both temp snapshots are checked explicitly.
    return (
      this.table(name) !== undefined ||
      this.tempSnap().table(name) !== undefined ||
      this.sharedTempSnap().table(name) !== undefined ||
      this.findIndex(name) !== null ||
      this.tempSnap().findIndex(name) !== null ||
      this.sharedTempSnap().findIndex(name) !== null ||
      // The sequence funnel walks session-local → shared → persistent, so an owned TEMP sequence's
      // name joins the namespace (temp-tables.md §8) — a collision with it is 42P07 too.
      this.sequence(name) !== undefined
    );
  }

  // fkProbeHits reports whether the parent currently holds the key/prefix `probe` (committed +
  // working state) — the child-side foreign-key existence test (spec/design/constraints.md §6.4).
  // `parentTable` is the referenced table's name. Unmetered, like the PK/UNIQUE probes (cost.md §3).
  private fkProbeHits(probe: FkProbe, parentTable: string): boolean {
    if (probe.kind === "pk") {
      return this.lkpStore(parentTable).get(probe.bytes) !== undefined;
    }
    return (
      this.readSnap().indexStore(probe.index).rangeEntries(uniqueProbeBound(probe.prefix)).length >
      0
    );
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
    // target is in the parent's stored-key byte space, so the child probe encodes a collated parent
    // key column with the PARENT's collation (§2.12).
    const parentColls = this.columnCollations(parent.columns);
    for (const e of this.lkpStore(childTable).entriesInKeyOrder()) {
      if (exclude.has(e.key.join(","))) continue;
      const probe = fkProbe(fk, parent, parentColls, e.row, fk.columns);
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
    // A standalone CREATE INDEX targets whichever scope owns the table — session-local temp, shared
    // temp, or persistent (spec/design/temp-tables.md §8). The build below is scope-agnostic (the
    // lkpTable/lkpStore/writeIndexStore funnels route by the resolution walk; the cost meter, UNIQUE
    // validation, naming/namespace collision, and the storage budget are all generic); only the catalog
    // putIndex write must target the owning snapshot, so the routing happens there.
    const table = this.lkpTable(ci.table);
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ci.table);
    }
    const tableKey = table.name.toLowerCase();
    const columns = table.columns;
    // Refuse building a collated index on a version-skewed table (slice 2d, collation.md §12, XX002):
    // the new B-tree would be pinned inconsistently with the file's other structures.
    this.ensureCollationsWritable(columns);
    // Per-column frozen collations for the collated text key form (§2.12); null everywhere for a
    // C-only / non-text table (the fast path).
    const colls = this.columnCollations(columns);
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
        // and only a FIXED-WIDTH KEY-ENCODABLE element type (else 0A000) — the GIN term IS that
        // element's key encoding (gin.md §3/§4), so the admitted set is the integers, boolean, uuid,
        // date, timestamp, timestamptz (interval's GIN-element support is a separate follow-on — its
        // key landed but the GIN slice has not; gin.md §3/§10).
        if (ty.kind !== "array") {
          throw engineError(
            "undefined_object",
            "data type " +
              typeCanonicalName(ty) +
              " has no default operator class for access method gin",
          );
        }
        if (!isGinElementType(ty.elem)) {
          throw engineError(
            "feature_not_supported",
            "a gin index on " + typeCanonicalName(ty) + " is not supported yet",
          );
        }
      } else if (
        !typeIsInteger(ty) &&
        !typeIsBoolean(ty) &&
        !typeIsText(ty) &&
        !typeIsBytea(ty) &&
        !typeIsDecimal(ty) &&
        !typeIsUuid(ty) &&
        !typeIsTimestamp(ty) &&
        !typeIsTimestamptz(ty) &&
        !typeIsDate(ty) &&
        !typeIsInterval(ty) &&
        !typeIsRange(ty)
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
        throw engineError(
          "feature_not_supported",
          "access method gin does not support unique indexes",
        );
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
    const meter = this.session.newMeter();
    const mask = columns.map(() => false);
    for (const c of cols) mask[c] = true;
    const def: IndexDef = { name, columns: cols, unique: ci.unique, kind };
    const store = this.lkpStore(ci.table);
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
        const prefix = indexPrefixKey(columns, colls, def, e.row);
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
      entries.push(...indexEntryKeys(columns, colls, def, e.key, e.row));
    }
    meter.guard();

    const nameKey = def.name.toLowerCase();
    // Register the index catalog entry + its (empty) store in the snapshot that owns the table (the
    // resolution walk — temp-tables.md §2/§4/§8): a session-local temp table's index lives in the
    // session temp snapshot, a shared temp table's in the database-wide shared one, so the index makes
    // ZERO file writes for either (the dirty bit lets the commit skip the main image). The entry writes
    // below then route through writeIndexStore, which finds the new store in that same temp snapshot.
    if (this.isTempTable(ci.table)) {
      this.session.tx!.tempDirty = true;
      this.session.tx!.tempWorking.putIndex(tableKey, def, this.pageSize);
    } else if (this.isSharedTempTable(ci.table)) {
      this.session.tx!.sharedTempDirty = true;
      this.session.tx!.sharedTempWorking.putIndex(tableKey, def, this.pageSize);
    } else {
      this.working().putIndex(tableKey, def, this.pageSize);
    }
    const istore = this.writeIndexStore(nameKey);
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
  // 42809, a missing one 42704. A pure catalog edit — zero cost, like DROP TABLE. The index is
  // resolved along the resolution walk (session-local → shared → persistent — temp-tables.md §8) and
  // removed from the snapshot that owns it, so dropping a temp table's index makes zero file writes.
  private executeDropIndex(di: DropIndex): Outcome {
    // lkpTable covers all three scopes, so DROP INDEX naming a table is 42809 regardless of kind.
    if (this.lkpTable(di.name)) {
      throw engineError("wrong_object_type", di.name + " is not an index");
    }
    const nameKey = di.name.toLowerCase();
    if (this.isTempIndex(di.name)) {
      const tableKey = this.tempSnap().findIndex(di.name)![0];
      this.session.tx!.tempDirty = true;
      this.session.tx!.tempWorking.removeIndex(tableKey, nameKey);
    } else if (this.isSharedTempIndex(di.name)) {
      const tableKey = this.sharedTempSnap().findIndex(di.name)![0];
      this.session.tx!.sharedTempDirty = true;
      this.session.tx!.sharedTempWorking.removeIndex(tableKey, nameKey);
    } else {
      const found = this.findIndex(di.name);
      if (!found) {
        throw engineError("undefined_object", "index does not exist: " + di.name);
      }
      this.working().removeIndex(found[0], nameKey);
    }
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
          throw engineError(
            "duplicate_column",
            "attribute " + f.name + " specified more than once",
          );
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
          throw engineError(
            "feature_not_supported",
            "a type modifier on an array type is not supported yet",
          );
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
      } else if (rangeByName(f.typeName) !== undefined) {
        // A range-typed composite field (a range inside CREATE TYPE) is deferred this slice (only
        // range *columns* are storable — spec/design/ranges.md §3); the type name IS known, so this
        // is 0A000, not the 42704 below.
        throw engineError(
          "feature_not_supported",
          "a range-typed composite field (" + f.typeName + ") is not supported yet",
        );
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
      fields.push({
        name: f.name,
        type: fty,
        decimal: fdecimal,
        notNull: f.notNull,
      });
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
    for (const f of fields)
      maxField = Math.max(maxField, this.readSnap().compositeTypeDepth(f.type, cache));
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
    const dep = this.compositeDependentAny(dt.name);
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
    this.working().putSequence(buildSequenceDef(cs.name, cs.options, undefined));
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeDropSequence analyzes and runs a DROP SEQUENCE (spec/design/sequences.md §1).
  // RESTRICT-only: a missing sequence is 42P01 unless IF EXISTS. No dependency tracking this slice
  // (a plain `DEFAULT nextval('s')` creates none — PG). Multiple names are dropped left to right.
  private executeDropSequence(ds: DropSequence): Outcome {
    for (const name of ds.names) {
      // Missing → 42P01 (unless IF EXISTS). An OWNED (serial) sequence has a dependent — its
      // column's default — so RESTRICT (the only mode this slice; CASCADE 0A000) is 2BP01
      // (spec/design/sequences.md §12).
      const seq = this.sequence(name);
      if (seq === undefined) {
        if (ds.ifExists) continue;
        throw engineError("undefined_table", `sequence does not exist: ${name}`);
      }
      if (seq.ownedBy !== undefined) {
        // The owning table is always present (its own DROP TABLE would auto-drop this sequence
        // first), so the column name for the detail resolves. The scope-aware lkpTable finds an
        // owned TEMP sequence's temp owner (temp-tables.md §8).
        const owner = seq.ownedBy;
        const t = this.lkpTable(owner.table);
        const tableName = t?.name ?? owner.table;
        const colName = t?.columns[owner.column]?.name ?? "";
        throw engineError(
          "dependent_objects_still_exist",
          `cannot drop sequence ${seq.name} because other objects depend on it: default value for column ${colName} of table ${tableName} depends on sequence ${seq.name}`,
        );
      }
      // Not owned: remove from whichever scope owns it (a temp sequence is always owned, so this
      // routed path is reached only for a plain persistent sequence — temp-tables.md §8).
      this.removeSequenceRouted(name);
    }
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // executeAlterSequence analyzes and runs an ALTER SEQUENCE [IF EXISTS] s <action>
  // (spec/design/sequences.md §4/§15). A missing sequence is 42P01 unless IF EXISTS (then a no-op).
  // The option form re-edits the definition (PG init_params, isInit=false — only written options
  // change, the counter preserved unless RESTART); RENAME TO moves the catalog key. Touches no
  // session state (currval/lastval unchanged). A catalog write (the write path, transactional, §5).
  private executeAlterSequence(as: AlterSequence): Outcome {
    const committed = this.sequence(as.name);
    if (committed === undefined) {
      if (as.ifExists) return { kind: "statement", cost: 0n, rowsAffected: null };
      throw engineError("undefined_table", `relation does not exist: ${as.name}`);
    }
    if (as.action.kind === "rename") {
      this.alterSequenceRename(committed, as.action.newName);
    } else {
      // AS type on ALTER is 0A000 — the value type is not persisted (sequences.md §14.4), so the
      // original type for re-deriving a default bound is gone.
      if (as.action.options.dataType !== null) {
        throw engineError("feature_not_supported", "ALTER SEQUENCE ... AS type is not supported");
      }
      const newDef = applySeqAlter(committed, as.action.options, as.action.restart);
      this.putSequenceRouted(newDef);
    }
    return { kind: "statement", cost: 0n, rowsAffected: null };
  }

  // alterSequenceRename implements ALTER SEQUENCE s RENAME TO s2 (spec/design/sequences.md §15.3): a
  // collision with any relation — including s itself — is 42P07; otherwise move the entry to the new
  // key. For an OWNED sequence, the owning column's DEFAULT nextval('s') text is rewritten in place
  // to nextval('s2') (the rows survive — not via putTable) so a later INSERT still advances the
  // renamed sequence (jed resolves the sequence by name, unlike PG's OID reference).
  private alterSequenceRename(existing: SequenceDef, newName: string): void {
    if (this.relationExists(newName)) {
      throw engineError("duplicate_table", `relation already exists: ${newName}`);
    }
    if (existing.ownedBy !== undefined) {
      const exprText = `nextval ( '${newName.toLowerCase().replace(/'/g, "''")}' )`;
      // Route to the owner's scope so a renamed owned TEMP sequence rewrites its column default in the
      // temp snapshot (temp-tables.md §8).
      this.setColumnDefaultExprRouted(
        existing.ownedBy.table.toLowerCase(),
        existing.ownedBy.column,
        {
          exprText,
          expr: parseExpression(exprText),
        },
      );
    }
    // Capture the owning scope BEFORE the remove: after dropping the old key the new name is in no
    // scope, so a post-remove route would wrongly default to the main image (temp-tables.md §8).
    let w: Snapshot;
    if (this.isTempSequence(existing.name)) {
      this.session.tx!.tempDirty = true;
      w = this.session.tx!.tempWorking;
    } else if (this.isSharedTempSequence(existing.name)) {
      this.session.tx!.sharedTempDirty = true;
      w = this.session.tx!.sharedTempWorking;
    } else {
      w = this.working();
    }
    w.removeSequence(existing.name.toLowerCase());
    w.putSequence({ ...existing, name: newName });
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
      const bk = encodeBoundKey(ib.colType, src, params, outer, ib.coll);
      if (bk.kind !== "key") return { rows: [], pages: 0, slabs: 0 };
      if (agreed === null) agreed = bk.key;
      else if (!bytesEq(agreed, bk.key)) return { rows: [], pages: 0, slabs: 0 };
    }
    // The entry-key prefix: the §2.2 present tag + the value's bare key bytes. The range
    // is every entry extending the prefix: [prefix, byte-successor(prefix)).
    const prefix = new Uint8Array(1 + agreed!.length);
    prefix.set(agreed!, 1);
    const b: KeyBound = {
      lo: prefix,
      loInc: true,
      hi: prefixSuccessor(prefix),
      hiInc: false,
    };
    const istore = this.lkpIndexStore(ib.nameKey);
    // The index store has no payload columns, so its mask is empty and its fused scan
    // contributes only the index-tree page_read count (no spill/compress units).
    const iscan = istore.rangeScanWithUnits(b, []);
    let pages = iscan.pages;
    const store = this.lkpStore(tableName);
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

  // ginBoundRows executes a GIN-bounded scan (spec/design/gin.md §6, cost.md §3). Evaluates the
  // constant query operand, extracts its terms + mode via the array_ops opclass (an array for @>/&&/=;
  // a single scalar term for = ANY — "member"; the array's distinct non-NULL terms for = — "equal"),
  // gathers each term's posting list (a prefix range scan of the GIN entry tree), combines them by
  // mode (@>, = ANY, and = → intersection, && → union) into the candidate storage-key set, and
  // point-looks-up each candidate in storage-key order. The original predicate stays the residual
  // WHERE filter (re-applied downstream), so the result is always correct. Returns the candidate rows
  // + the scan's up-front (pages, slabs); gin_entry (per posting entry visited) is charged on meter
  // directly. Degenerate constant queries (gin.md §6): a NULL Q, an @> whose Q holds a NULL element,
  // an && with no non-NULL term, and a NULL = ANY scalar are provably empty; @> '{}' and array = with
  // no non-NULL term fall back to the full scan.
  ginBoundRows(
    tableName: string,
    gb: GinBound,
    query: RExpr | null,
    env: EvalEnv,
    meter: Meter,
    mask: boolean[],
  ): { entries: Entry[]; pages: number; slabs: number } {
    const store = this.lkpStore(tableName);
    if (query === null) return { entries: [], pages: 0, slabs: 0 };
    // Extract the query's terms (extract_query_terms) — a pure planning step, NOT metered (cost.md
    // §3): evaluate Q on a scratch meter. Q is a constant, so the empty row suffices.
    const qv = evalExpr(query, [], env, new Meter());
    // Each term is the element's order-preserving key encoding (gin.md §4) — the SAME bytes the
    // entries carry, so a term doubles as its posting-list prefix below. Encoding now lets us dedup
    // distinct terms by bytes (a bijection: byte-dedup == value-dedup, byte-sort == value-sort)
    // generically over every admitted element type.
    const terms: Uint8Array[] = [];
    let hasNull = false;
    let isEmpty = false;
    if (gb.strategy === "member") {
      // `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL c can equal no element,
      // so the bound is provably empty (gin.md §6). c is in the element type's domain by resolution
      // (jed coerces c to the element type, rejecting an out-of-range integer constant 22003 before
      // exec); inRange is a defensive guard against silently truncating an out-of-range integer into
      // a wrong term.
      if (qv.kind === "null") return { entries: [], pages: 0, slabs: 0 };
      if (qv.kind === "int" && !inRange(gb.elemType, qv.int))
        return { entries: [], pages: 0, slabs: 0 };
      // a GIN element is fixed-width (no text), so the term never collates.
      terms.push(encodeKeyValue(gb.elemType, qv, null));
    } else {
      if (qv.kind !== "array") return { entries: [], pages: 0, slabs: 0 }; // NULL/non-array → provably empty
      const seen = new Set<string>();
      for (const el of qv.elements) {
        if (el.kind === "null") {
          hasNull = true; // a NULL element carries no term
          continue;
        }
        const t = encodeKeyValue(gb.elemType, el, null);
        const k = byteKey(t);
        if (!seen.has(k)) {
          seen.add(k);
          terms.push(t);
        }
      }
      isEmpty = qv.elements.length === 0;
      terms.sort(cmpBytes);
    }

    if (gb.strategy === "contains") {
      if (isEmpty) {
        // @> '{}': every non-NULL array contains the empty array — not derivable from the index;
        // fall back to the full scan (the residual filter keeps the right rows — gin.md §6).
        const u = store.scanWithUnits(mask);
        return { entries: u.entries, pages: u.pages, slabs: u.slabs };
      }
      if (hasNull) return { entries: [], pages: 0, slabs: 0 }; // @> a query with a NULL element is never TRUE
    } else if (gb.strategy === "equal") {
      if (terms.length === 0) {
        // col = Q with NO non-NULL term — '{}' (isEmpty) or an all-NULL Q (hasNull, no non-NULL
        // element). The rows it matches ({}, {NULL}, …) carry NO index terms, so the index cannot
        // enumerate them: fall back to the full scan and let the residual = keep them (gin.md §6).
        // NOT a provably-empty bound — and a Q with ≥1 non-NULL element is NOT caught here (it
        // gathers, even when it also has a NULL element).
        const u = store.scanWithUnits(mask);
        return { entries: u.entries, pages: u.pages, slabs: u.slabs };
      }
    } else if (gb.strategy === "overlaps") {
      if (terms.length === 0) return { entries: [], pages: 0, slabs: 0 }; // && with no non-NULL term
    }

    // Gather each term's posting list: the entry range [encode(term), successor) of the GIN tree
    // (gin.md §4). The entry is encode(term) ‖ storage_key; the fixed-width term self-delimits, so
    // the storage key is the suffix after termWidth bytes.
    const istore = this.lkpIndexStore(gb.nameKey);
    const termWidth = widthBytes(gb.elemType);
    let pages = 0;
    let entriesVisited = 0;
    const postings: Uint8Array[][] = [];
    for (const prefix of terms) {
      const b: KeyBound = {
        lo: prefix,
        loInc: true,
        hi: prefixSuccessor(prefix),
        hiInc: false,
      };
      const scan = istore.rangeScanWithUnits(b, []);
      pages += scan.pages;
      entriesVisited += scan.entries.length;
      postings.push(scan.entries.map((e) => e.key.slice(termWidth)));
    }
    meter.charge(COSTS.ginEntry * BigInt(entriesVisited));

    // Combine into the candidate storage keys, ascending byte (= storage-key) order, so the point
    // lookups and emitted rows follow storage order exactly as a full scan (gin.md §6/§8). @> ALL →
    // intersection; = ANY (member) is a single term, so its intersection is that lone posting list;
    // array = (equal) gathers the same superset as @> over Q's distinct non-NULL terms (the residual
    // = makes it exact downstream); && ANY → union.
    const cand = gb.strategy === "overlaps" ? unionPostings(postings) : intersectPostings(postings);

    let slabs = 0;
    const entries: Entry[] = [];
    for (const key of cand) {
      const u = store.getWithUnits(key, mask);
      pages += u.pages;
      slabs += u.slabs;
      if (u.row === undefined) throw new Error("a GIN entry references a stored row");
      entries.push({ key, row: u.row });
    }
    return { entries, pages, slabs };
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
  private executeInsert(ins: Insert, params: Value[], ctx: CteCtx): Outcome {
    const table = this.lkpTable(ins.table); // temp-first (temp-tables.md §3)
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + ins.table);
    }
    // Refuse the write if any of this table's collated keys are version-skewed (slice 2d): a
    // maintained B-tree would mix two orderings (collation.md §12, XX002).
    this.ensureCollationsWritable(table.columns);
    const store = this.writeStore(ins.table);
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

    // IDENTITY column handling (spec/design/sequences.md §13). OVERRIDING USER VALUE discards any
    // supplied value for every identity column and uses its sequence instead — modeled by treating
    // the column as omitted (provided[i] = -1, so its nextval default applies). Apply it before the
    // GENERATED ALWAYS gate below so a User-overridden ALWAYS column needs no further check.
    if (ins.overriding === "user") {
      for (let i = 0; i < n; i++) {
        if (table.columns[i]!.identity !== null) provided[i] = -1;
      }
    }
    // The GENERATED ALWAYS columns still explicitly targeted (and not OVERRIDING SYSTEM VALUE):
    // supplying a non-DEFAULT value to one is 428C9. Collected as (column ordinal, value position)
    // so the source branches can enforce it (VALUES per-row, SELECT up-front).
    const alwaysTargeted: { col: number; pos: number }[] = [];
    if (ins.overriding !== "system") {
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        if (col.identity === "always" && provided[i]! >= 0) {
          alwaysTargeted.push({ col: i, pos: provided[i]! });
        }
      }
    }

    if (ins.source.kind === "select") {
      // GENERATED ALWAYS gate (sequences.md §13.3): a SELECT projection always supplies an explicit
      // value, so targeting an ALWAYS identity column without OVERRIDING SYSTEM VALUE is 428C9 —
      // raised up front (PG raises it at rewrite), firing even over a zero-row source.
      if (alwaysTargeted.length > 0) {
        throw engineError(
          "generated_always",
          `cannot insert a non-DEFAULT value into column ${table.columns[alwaysTargeted[0]!.col]!.name}`,
        );
      }
      // SELECT source (§24). Plan the source query, then resolve the RETURNING projection
      // (PostgreSQL's analysis order — both precede any execution), threading ONE ParamTypes
      // so a $N shared by the source and the RETURNING list unifies statement-wide (api.md
      // §5). The source returns OWNED rows, so a self-insert (INSERT INTO t SELECT ... FROM
      // t) reads the pre-insert snapshot, then writes.
      const ptypes = new ParamTypes();
      // The source query (and the RETURNING sublinks) see the statement's CTE bindings
      // (writable-cte.md) — the move-rows idiom INSERTs a SELECT over a CTE buffer.
      const plan = this.planQuery(ins.source.select, null, ctx.bindings, ptypes);
      const ret =
        ins.returning !== null
          ? this.resolveReturning(table, ins.returning, false, ctx.bindings, ptypes)
          : null;
      const cplan =
        ins.onConflict !== null ? this.resolveOnConflict(table, ins.onConflict, ptypes) : null;
      const bound = bindParams(params, ptypes.finalize());
      const meter = this.session.newMeter();
      const foldCost = { value: 0n };
      this.foldUncorrelatedInPlan(plan, bound, ctx, foldCost);
      // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
      // pre-statement snapshot (grammar.md §32). They see the statement's CTE bindings
      // (writable-cte.md) via `ctx`.
      if (ret !== null) {
        ret.nodes = ret.nodes.map((node) =>
          this.foldUncorrelatedInRExpr(node, bound, ctx, foldCost),
        );
      }
      this.foldConflictPlan(cplan, bound, foldCost);
      meter.charge(foldCost.value);
      const q = this.execQueryPlan(plan, [], bound, ctx);
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
        if (col.type.kind === "range") {
          // INSERT ... SELECT into a range column is deferred (the VALUES + range literal/cast path
          // is the supported input — spec/design/ranges.md §1), like array.
          throw engineError(
            "feature_not_supported",
            "INSERT ... SELECT into range column " + col.name + " is not supported yet",
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
      const { affected, returned } = this.runInsertRows(
        table,
        store,
        pk,
        checks,
        defaultExprs,
        stmtRng,
        provided,
        q.rows,
        cplan,
        ret?.nodes ?? null,
        bound,
        ctx,
        meter,
      );
      return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, affected, meter.accrued);
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
    // GENERATED ALWAYS gate (sequences.md §13.3): an explicit (non-DEFAULT) value targeting an
    // ALWAYS identity column without OVERRIDING SYSTEM VALUE is 428C9. Statement-level — fires
    // before any row is materialized; an all-DEFAULT column is fine. Arity is validated above, so
    // values[pos] is in range.
    for (const at of alwaysTargeted) {
      if (rowsIn.some((values) => values[at.pos]!.kind !== "default")) {
        throw engineError(
          "generated_always",
          `cannot insert a non-DEFAULT value into column ${table.columns[at.col]!.name}`,
        );
      }
    }
    // Resolve the RETURNING projection after the source (PostgreSQL's analysis order) and
    // before binding/execution — a 42703 here beats a would-be 23505 (grammar.md §32).
    const ret =
      ins.returning !== null
        ? this.resolveReturning(table, ins.returning, false, ctx.bindings, ptypes)
        : null;
    const cplan =
      ins.onConflict !== null ? this.resolveOnConflict(table, ins.onConflict, ptypes) : null;
    const bound = bindParams(params, ptypes.finalize());

    // INSERT ... VALUES reads no rows; with only literal values and constant defaults it
    // evaluates no expression tree (leaves), so a plain fully-inline insert still costs zero. An
    // EXPRESSION default (DEFAULT uuidv7()) evaluates a tree per application — operator_eval per
    // node — the documented exception (constraints.md §2, like CHECK). Other metered work: the
    // disposition plan's compression attempts for over-RECORD_MAX rows (value_compress) and the
    // RETURNING projection. The meter is created here (before materialization) so a
    // DEFAULT-keyword expression default charges it too.
    const meter = this.session.newMeter();

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
          if (iv.kind === "default")
            rv[p] = this.evalDefault(col, defaultExprs[i]!, stmtRng, meter);
          else rv[p] = materializeInsertValue(iv, store.columnTypes()[i]!, bound);
        }
      }
      rows.push(rv);
    }
    // Uncorrelated subqueries in the RETURNING list and the DO UPDATE SET/WHERE fold once
    // (cost.md §3), reading the pre-statement snapshot (grammar.md §32).
    const foldCost = { value: 0n };
    if (ret !== null) {
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, ctx, foldCost));
    }
    this.foldConflictPlan(cplan, bound, foldCost);
    meter.charge(foldCost.value);
    const { affected, returned } = this.runInsertRows(
      table,
      store,
      pk,
      checks,
      defaultExprs,
      stmtRng,
      provided,
      rows,
      cplan,
      ret?.nodes ?? null,
      bound,
      ctx,
      meter,
    );
    return dmlOutcome(ret?.names ?? null, ret?.types ?? null, returned, affected, meter.accrued);
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
    ctes: CteCtx,
    meter: Meter,
  ): Value[][] | null {
    const n = table.columns.length;
    // The columns' resolved ColTypes (a scalar, or a composite resolved to its field tree), for
    // composite-aware store coercion (spec/design/composite.md §4).
    const colTypes = store.columnTypes();
    // Per-column frozen collations for the collated text key form (§2.12); null everywhere for a
    // C-only / non-text table (the fast path).
    const colls = this.columnCollations(table.columns);
    // Phase-1 existence probes read the VISIBLE snapshot (the read pin under a data-modifying WITH —
    // writable-cte.md §2; else working == read-your-writes), so a self-insert sees the pre-insert
    // state and an earlier sub-statement's staged key is invisible here (its collision is caught at
    // phase 2 — §7). The passed `store` is the working set the phase-2 inserts land in.
    const readStore = this.lkpStore(table.name);
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
        const candidate: Value =
          p >= 0 ? values[p]! : this.evalDefault(col, defaultExprs[i]!, rng, meter);
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
          seam: this.session.seam,
          rng,
          ctes: EMPTY_CTE_CTX,
          exec: this,
        };
        evalChecks(checks, table.name, row, env, meter);
      }

      let key: Uint8Array | null = null;
      if (pk.length > 0) {
        // The composite key is the concatenation of the members' bare encodings in key order
        // (encoding.md §2.3 — encodePkKey); a single-column key is the one-member case.
        key = encodePkKey(table, pk, colls, row);
        // The PK's 23505 reports PostgreSQL's derived auto-name for the PK index,
        // `<table>_pkey` — jed persists/reserves no such relation (constraints.md §5.4).
        const seen = key.join(",");
        if (seenKeys.has(seen) || readStore.get(key) !== undefined) {
          throw engineError(
            "unique_violation",
            "duplicate key value violates unique constraint: " + table.name.toLowerCase() + "_pkey",
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
        const prefix = indexPrefixKey(table.columns, colls, def, row);
        if (prefix === null) continue;
        const istore = this.lkpIndexStore(def.name.toLowerCase());
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
      // The probe matches the parent's stored key, so a collated parent key column uses the
      // PARENT's collation (§2.12).
      const parentColls = this.columnCollations(parent.columns);
      // Only a self-reference can satisfy against this statement's batch (a different parent table
      // is unchanged by this INSERT). Collect the parent keys the batch supplies.
      const batch = new Set<string>();
      if (fk.refTable.toLowerCase() === relation.toLowerCase()) {
        for (const pr of prepared) {
          const p = fkProbe(fk, parent, parentColls, pr.row, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
      }
      for (const pr of prepared) {
        const probe = fkProbe(fk, parent, parentColls, pr.row, fk.columns);
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
        ? this.projectReturning(
            returning,
            prepared.map((pr) => pr.row),
            null,
            params,
            ctes,
            meter,
          )
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
        indexInserts[k]!.push(
          ...indexEntryKeys(table.columns, colls, table.indexes[k]!, key, pr.row),
        );
      }
      if (!store.insert(key, pr.row)) {
        // A collision here can only happen under the writable-CTE read pin (writable-cte.md §7): an
        // EARLIER data-modifying sub-statement of the same WITH staged this key, which phase 1
        // (reading the pin) did not see. Matches PostgreSQL's unique violation; the whole statement
        // aborts all-or-nothing. For a single statement, phase 1 already caught every duplicate, so
        // this is never reached.
        throw engineError(
          "unique_violation",
          `duplicate key value violates unique constraint: ${relation.toLowerCase()}_pkey`,
        );
      }
    }
    for (let k = 0; k < table.indexes.length; k++) {
      const def = table.indexes[k]!;
      const istore = this.writeIndexStore(def.name.toLowerCase());
      for (const ek of indexInserts[k]!) {
        if (!istore.insert(ek, [])) {
          // A cross-sub-statement unique-index collision under the read pin (as above).
          throw engineError(
            "unique_violation",
            `duplicate key value violates unique constraint: ${def.name}`,
          );
        }
      }
    }
    return returned;
  }

  // resolveOnConflict resolves an ON CONFLICT clause (spec/design/upsert.md §2/§5) into a
  // ConflictPlan: the arbiter, plus — for DO UPDATE — the resolved SET assignment plans and the
  // optional WHERE filter, both resolved against the [existing | excluded] scope. Threads the
  // statement ptypes so a $N in a SET/WHERE unifies with the rest of the INSERT.
  private resolveOnConflict(table: Table, oc: OnConflict, ptypes: ParamTypes): ConflictPlan {
    const arb = resolveArbiter(table, oc.target);
    if (!oc.doUpdate) {
      return { arb, doUpdate: false, assignments: [], filter: null };
    }
    // DO UPDATE requires a target (spec/design/upsert.md §2) — PostgreSQL's message.
    if (arb === null) {
      throw engineError(
        "syntax_error",
        "ON CONFLICT DO UPDATE requires inference specification or constraint name",
      );
    }
    const scope = Scope.onConflictExcluded(this, table);
    const pkMembers = pkIndices(table);
    const plans: AssignPlan[] = [];
    for (const a of oc.assignments) {
      const idx = columnIndex(table, a.column);
      if (idx < 0) throw engineError("undefined_column", "column does not exist: " + a.column);
      if (table.columns[idx]!.identity === "always") {
        throw engineError("generated_always", `column ${a.column} can only be updated to DEFAULT`);
      }
      if (pkMembers.includes(idx)) {
        throw engineError(
          "feature_not_supported",
          "updating a primary key column is not supported",
        );
      }
      for (const p of plans) {
        if (p.idx === idx) {
          throw engineError("duplicate_column", "column " + a.column + " assigned more than once");
        }
      }
      const col = table.columns[idx]!;
      if (col.type.kind === "composite") {
        throw engineError(
          "feature_not_supported",
          "updating composite column " + a.column + " is not supported yet",
        );
      }
      if (col.type.kind === "array") {
        throw engineError(
          "feature_not_supported",
          "updating array column " + a.column + " is not supported yet",
        );
      }
      if (col.type.kind === "range") {
        throw engineError(
          "feature_not_supported",
          "updating range column " + a.column + " is not supported yet",
        );
      }
      const targetScalar = col.type.scalar;
      const { node, type } = resolve(
        scope,
        a.value,
        targetScalar,
        { collecting: false, groupKeys: [], specs: [] },
        ptypes,
      );
      requireAssignable(type, targetScalar, a.column);
      plans.push({
        idx,
        name: col.name,
        target: targetScalar,
        decimal: col.decimal,
        notNull: col.notNull,
        source: node,
      });
    }
    const filter = oc.filter !== null ? resolveBooleanFilter(scope, oc.filter, ptypes) : null;
    return { arb, doUpdate: true, assignments: plans, filter };
  }

  // arbiterExisting looks up the EXISTING (committed) conflicting row for an arbiter key
  // (spec/design/upsert.md §3): always a committed row (an in-batch row sharing the arbiter key was
  // caught earlier by the proposed-arbiter set). Returns { key, row } or null.
  private arbiterExisting(
    arb: Arbiter,
    store: TableStore,
    table: Table,
    ak: Uint8Array,
  ): { key: Uint8Array; row: Row } | null {
    if (arb.isPK) {
      const row = store.get(ak);
      if (row === undefined) return null;
      return { key: ak, row: store.resolveAll(row) };
    }
    const def = table.indexes[arb.indexPos]!;
    const entries = this.readSnap()
      .indexStore(def.name.toLowerCase())
      .rangeEntries(uniqueProbeBound(ak));
    if (entries.length === 0) return null;
    const suffix = entries[0]!.key.slice(ak.length);
    const row = store.get(suffix);
    if (row === undefined) throw new Error("a unique-index entry points at a live row");
    return { key: suffix, row: store.resolveAll(row) };
  }

  // rowConflictsCommitted reports whether a candidate row conflicts with a COMMITTED row on the
  // primary key or any unique index (the no-target DO NOTHING skip test — spec/design/upsert.md §2).
  // NULLS DISTINCT: a unique tuple with any NULL component never conflicts.
  private rowConflictsCommitted(
    store: TableStore,
    table: Table,
    pk: number[],
    colls: (Collation | null)[],
    row: Row,
  ): boolean {
    if (pk.length > 0 && store.get(encodePkKey(table, pk, colls, row)) !== undefined) return true;
    for (const def of table.indexes) {
      if (!def.unique) continue;
      const prefix = indexPrefixKey(table.columns, colls, def, row);
      if (prefix === null) continue;
      if (
        this.readSnap().indexStore(def.name.toLowerCase()).rangeEntries(uniqueProbeBound(prefix))
          .length > 0
      ) {
        return true;
      }
    }
    return false;
  }

  // foldConflictPlan folds globally-uncorrelated subqueries in a DO UPDATE's SET/WHERE once (their
  // cost is added a single time — cost.md §3), exactly as UPDATE folds its assignment/filter.
  private foldConflictPlan(
    plan: ConflictPlan | null,
    bound: Value[],
    foldCost: { value: bigint },
  ): void {
    if (plan === null || !plan.doUpdate) return;
    plan.assignments = plan.assignments.map((ap) => ({
      ...ap,
      source: this.foldUncorrelatedInRExpr(ap.source, bound, EMPTY_CTE_CTX, foldCost),
    }));
    if (plan.filter !== null) {
      plan.filter = this.foldUncorrelatedInRExpr(plan.filter, bound, EMPTY_CTE_CTX, foldCost);
    }
  }

  // runInsertRows dispatches the validated candidate rows to the plain or the ON CONFLICT insert
  // path, shared by both INSERT sources. Returns { affected, returned }: a plain insert affects
  // every candidate row; an ON CONFLICT may insert, update, or skip (spec/design/upsert.md §3).
  private runInsertRows(
    table: Table,
    store: TableStore,
    pk: number[],
    checks: NamedCheck[],
    defaultExprs: (RExpr | null)[],
    rng: StmtRng,
    provided: number[],
    rows: Value[][],
    conflict: ConflictPlan | null,
    returning: RExpr[] | null,
    params: Value[],
    ctes: CteCtx,
    meter: Meter,
  ): { affected: number; returned: Value[][] | null } {
    if (conflict !== null) {
      return this.insertRowsOnConflict(
        table,
        store,
        pk,
        checks,
        defaultExprs,
        rng,
        provided,
        rows,
        conflict,
        returning,
        params,
        ctes,
        meter,
      );
    }
    const returned = this.insertRows(
      table,
      store,
      pk,
      checks,
      defaultExprs,
      rng,
      provided,
      rows,
      returning,
      params,
      ctes,
      meter,
    );
    return { affected: rows.length, returned };
  }

  // insertRowsOnConflict runs phase 1 + phase 2 of an INSERT ... ON CONFLICT (spec/design/upsert.md
  // §3), the UPSERT analogue of insertRows. Phase 1 walks the candidate rows in source order,
  // classifying each as a planned INSERT, a planned UPDATE of an existing row, or a SKIP; the
  // planned inserts + updates are then validated against the statement END STATE (PK / unique /
  // CHECK / FK) before phase 2 writes anything (all-or-nothing). returning projects the AFFECTED
  // rows (inserts with an all-NULL old side, updates with their pre-update existing row).
  private insertRowsOnConflict(
    table: Table,
    store: TableStore,
    pk: number[],
    checks: NamedCheck[],
    defaultExprs: (RExpr | null)[],
    rng: StmtRng,
    provided: number[],
    rows: Value[][],
    plan: ConflictPlan,
    returning: RExpr[] | null,
    params: Value[],
    ctes: CteCtx,
    meter: Meter,
  ): { affected: number; returned: Value[][] | null } {
    const n = table.columns.length;
    const relation = table.name;
    const colTypes = store.columnTypes();
    // Phase-1 existence/arbiter probes read the VISIBLE snapshot (the read pin under a data-modifying
    // WITH — writable-cte.md §2; else working == read-your-writes); the passed `store` is the working
    // set the phase-2 inserts/replaces land in.
    const readStore = this.lkpStore(table.name);
    // Per-column frozen collations for the collated text key form (§2.12); null everywhere for a
    // C-only / non-text table (the fast path).
    const colls = this.columnCollations(table.columns);
    // The unique-index positions in table.indexes (no-target skip test + end-state pass).
    const uniqIdx: number[] = [];
    for (let i = 0; i < table.indexes.length; i++) if (table.indexes[i]!.unique) uniqIdx.push(i);

    const inserts: Row[] = [];
    const updates: { key: Uint8Array; newRow: Row; oldRow: Row }[] = [];
    // Arbiter keys this statement has already proposed (the §4 second-affect rule).
    const proposedArb = new Set<string>();
    // For the no-target DO NOTHING path: the planned inserts' keys/prefixes (the arbiter path uses
    // proposedArb instead).
    const insPk = new Set<string>();
    const insPrefixes = uniqIdx.map(() => new Set<string>());

    const checkEnv = (): EvalEnv => ({
      params: [],
      outer: [],
      runSubquery: (p, o) => this.execQueryPlan(p, o, [], EMPTY_CTE_CTX),
      seam: this.session.seam,
      rng,
      ctes: EMPTY_CTE_CTX,
      exec: this,
    });

    for (const values of rows) {
      // Build + coerce the candidate row, then CHECK — the INSERT per-row order (NOT NULL before
      // CHECK before conflict; constraints.md §4.4).
      const row: Row = new Array(n);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        const candidate: Value =
          p >= 0 ? values[p]! : this.evalDefault(col, defaultExprs[i]!, rng, meter);
        row[i] = coerceForStore(candidate, colTypes[i]!, col.decimal, col.notNull, col.name);
      }
      if (checks.length > 0) {
        meter.guard();
        evalChecks(checks, relation, row, checkEnv(), meter);
      }

      if (plan.arb === null) {
        // No-target DO NOTHING: skip on ANY uniqueness conflict (committed OR an earlier planned
        // insert); else insert (upsert.md §2/§3).
        const pkk = pk.length > 0 ? encodePkKey(table, pk, colls, row) : null;
        let conflictHit = this.rowConflictsCommitted(store, table, pk, colls, row);
        if (!conflictHit && pkk !== null && insPk.has(pkk.join(","))) conflictHit = true;
        if (!conflictHit) {
          for (let u = 0; u < uniqIdx.length; u++) {
            const prefix = indexPrefixKey(table.columns, colls, table.indexes[uniqIdx[u]!]!, row);
            if (prefix !== null && insPrefixes[u]!.has(prefix.join(","))) {
              conflictHit = true;
              break;
            }
          }
        }
        if (conflictHit) continue; // skip
        if (pkk !== null) insPk.add(pkk.join(","));
        for (let u = 0; u < uniqIdx.length; u++) {
          const prefix = indexPrefixKey(table.columns, colls, table.indexes[uniqIdx[u]!]!, row);
          if (prefix !== null) insPrefixes[u]!.add(prefix.join(","));
        }
        inserts.push(row);
        continue;
      }

      // Arbiter present (DO UPDATE always; DO NOTHING with a target).
      const ak = arbiterKey(plan.arb, table, pk, colls, row);
      if (ak === null) {
        // A NULL-bearing arbiter key never conflicts (NULLS DISTINCT) — plain insert.
        inserts.push(row);
        continue;
      }
      const akKey = ak.join(",");
      if (proposedArb.has(akKey)) {
        // A second proposed row with the same arbiter key (§4).
        if (plan.doUpdate) {
          throw engineError(
            "cardinality_violation",
            "ON CONFLICT DO UPDATE command cannot affect row a second time",
          );
        }
        continue; // DO NOTHING → skip
      }
      proposedArb.add(akKey);
      const existing = this.arbiterExisting(plan.arb, readStore, table, ak);
      if (existing === null) {
        // No committed conflict on the arbiter → insert (a non-arbiter conflict is caught by the
        // end-state validation below).
        inserts.push(row);
        continue;
      }
      if (!plan.doUpdate) continue; // DO NOTHING → skip
      // DO UPDATE: the combined eval row [existing | proposed] the §5 scope resolves against.
      const combined = existing.row.concat(row);
      const env: EvalEnv = {
        params,
        outer: [],
        runSubquery: (p, o) => this.execQueryPlan(p, o, params, EMPTY_CTE_CTX),
        seam: this.session.seam,
        rng,
        ctes: EMPTY_CTE_CTX,
        exec: this,
      };
      // An optional WHERE that is not TRUE skips the update (existing row unchanged, not returned)
      // — but the arbiter key was already proposed, so a second row still trips §4.
      if (plan.filter !== null && !isTrue(evalExpr(plan.filter, combined, env, meter))) continue;
      const newRow = existing.row.slice();
      for (const ap of plan.assignments) {
        newRow[ap.idx] = checkAssign(ap, evalExpr(ap.source, combined, env, meter));
      }
      if (checks.length > 0) evalChecks(checks, relation, newRow, checkEnv(), meter);
      updates.push({ key: existing.key, newRow, oldRow: existing.row });
    }

    // End-state validation (upsert.md §3), before any write. PRIMARY KEY: each insert's key must be
    // free in the committed store and distinct from the other inserts — a collision is 23505 on
    // <table>_pkey (a non-arbiter PK conflict).
    if (pk.length > 0 && inserts.length > 0) {
      const seen = new Set<string>();
      for (const row of inserts) {
        const k = encodePkKey(table, pk, colls, row);
        const ks = k.join(",");
        if (readStore.get(k) !== undefined || seen.has(ks)) {
          throw engineError(
            "unique_violation",
            "duplicate key value violates unique constraint: " + relation.toLowerCase() + "_pkey",
          );
        }
        seen.add(ks);
      }
    }

    // UNIQUE indexes: validate the END STATE over the updated NEW rows + the inserted rows
    // (indexes.md §8 — the same end-state model as UPDATE).
    if (uniqIdx.length > 0 && (inserts.length > 0 || updates.length > 0)) {
      const rewritten = new Set<string>(updates.map((u) => u.key.join(",")));
      const newRows = updates.map((u) => u.newRow).concat(inserts);
      for (const ix of uniqIdx) {
        const def = table.indexes[ix]!;
        const istore = this.lkpIndexStore(def.name.toLowerCase());
        const batch = new Set<string>();
        for (const newRow of newRows) {
          const prefix = indexPrefixKey(table.columns, colls, def, newRow);
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

    // FOREIGN KEY child-side (constraints.md §6.4): each inserted row, and each updated row that
    // assigned an FK local column, must reference an existing parent key — the committed parent
    // state plus (for a self-reference) the statement's end state.
    const assigned = new Set<number>(plan.doUpdate ? plan.assignments.map((a) => a.idx) : []);
    const fks = this.table(relation)?.fks ?? [];
    for (const fk of fks) {
      const parent = this.table(fk.refTable);
      if (parent === undefined) continue;
      // The probe matches the parent's stored key, so a collated parent key column uses the
      // PARENT's collation (§2.12).
      const parentColls = this.columnCollations(parent.columns);
      const checkUpdates = fk.columns.some((c) => assigned.has(c));
      const batch = new Set<string>();
      if (fk.refTable.toLowerCase() === relation.toLowerCase()) {
        for (const row of inserts) {
          const p = fkProbe(fk, parent, parentColls, row, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
        for (const u of updates) {
          const p = fkProbe(fk, parent, parentColls, u.newRow, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
      }
      const toCheck = inserts.concat(checkUpdates ? updates.map((u) => u.newRow) : []);
      for (const row of toCheck) {
        const probe = fkProbe(fk, parent, parentColls, row, fk.columns);
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

    // FOREIGN KEY parent-side (constraints.md §6.5): an updated referenced row must not strand a
    // child (only a referenced UNIQUE column is at risk; inserts add rows, never strand a child).
    const referencers = this.fkReferencers(relation);
    if (referencers.length > 0 && updates.length > 0) {
      const parent = this.table(relation)!;
      const updatedKeys = new Set<string>(updates.map((u) => u.key.join(",")));
      for (const { childTable, fk } of referencers) {
        // parent is the insert target itself, so its key columns use colls (§2.12).
        const newPresent = new Set<string>();
        for (const u of updates) {
          const p = fkProbe(fk, parent, colls, u.newRow, fk.refColumns);
          if (p !== null) newPresent.add(fkProbeBytes(p).join(","));
        }
        for (const u of updates) {
          const oldProbe = fkProbe(fk, parent, colls, u.oldRow, fk.refColumns);
          if (oldProbe === null) continue;
          const newProbe = fkProbe(fk, parent, colls, u.newRow, fk.refColumns);
          if (newProbe !== null && bytesEq(fkProbeBytes(newProbe), fkProbeBytes(oldProbe)))
            continue;
          if (newPresent.has(fkProbeBytes(oldProbe).join(","))) continue;
          if (this.fkChildReferences(childTable, fk, parent, fkProbeBytes(oldProbe), updatedKeys)) {
            throw engineError(
              "foreign_key_violation",
              "update or delete on table " +
                parent.name +
                " violates foreign key constraint " +
                fk.name +
                " on table " +
                childTable,
            );
          }
        }
      }
    }

    // Meter the disposition-plan compression attempts (value_compress, cost.md §3) for the inserted
    // + updated rows; enforce the ceiling BEFORE phase 2 writes (all-or-nothing).
    let cunits = 0n;
    const placeholder = new Uint8Array(8);
    for (const row of inserts) {
      const kb = pk.length > 0 ? encodePkKey(table, pk, colls, row) : placeholder;
      cunits += BigInt(store.writeCompressUnits(kb, row));
    }
    for (const u of updates) cunits += BigInt(store.writeCompressUnits(u.key, u.newRow));
    meter.charge(COSTS.valueCompress * cunits);
    meter.guard();

    // RETURNING (grammar.md §32): project the affected rows — inserts (old side all-NULL) then
    // updates (old side the pre-update existing row) — after all validation, before any write.
    let returned: Value[][] | null = null;
    if (returning !== null) {
      const nullRow: Row = table.columns.map(() => nullValue());
      const prows: Row[] = [];
      const olds: Row[] = [];
      for (const row of inserts) {
        prows.push(row);
        olds.push(nullRow);
      }
      for (const u of updates) {
        prows.push(u.newRow);
        olds.push(u.oldRow);
      }
      returned = this.projectReturning(returning, prows, olds, params, ctes, meter);
    }

    const affected = inserts.length + updates.length;

    // Phase 2 — every row validated. Insert the new rows (rowid alloc for a no-PK table, index
    // entries added), then replace the updated rows (index entries moved).
    const indexAdds: Uint8Array[][] = table.indexes.map(() => []);
    for (const row of inserts) {
      const key =
        pk.length > 0 ? encodePkKey(table, pk, colls, row) : encodeInt("i64", store.allocRowid());
      for (let k = 0; k < table.indexes.length; k++) {
        indexAdds[k]!.push(...indexEntryKeys(table.columns, colls, table.indexes[k]!, key, row));
      }
      if (!store.insert(key, row)) throw new Error("pre-validated INSERT key must be unique");
    }
    const indexMoves: { removals: Uint8Array[]; insertions: Uint8Array[] }[][] = table.indexes.map(
      () => [],
    );
    for (const u of updates) {
      for (let k = 0; k < table.indexes.length; k++) {
        const def = table.indexes[k]!;
        const oldEks = indexEntryKeys(table.columns, colls, def, u.key, u.oldRow);
        const newEks = indexEntryKeys(table.columns, colls, def, u.key, u.newRow);
        const removals = bytesDiff(oldEks, newEks);
        const insertions = bytesDiff(newEks, oldEks);
        if (removals.length > 0 || insertions.length > 0)
          indexMoves[k]!.push({ removals, insertions });
      }
    }
    for (const u of updates) store.replace(u.key, u.newRow);
    for (let k = 0; k < table.indexes.length; k++) {
      const istore = this.writeIndexStore(table.indexes[k]!.name.toLowerCase());
      for (const ek of indexAdds[k]!) {
        if (!istore.insert(ek, []))
          throw new Error("index entry keys are unique (storage-key suffix)");
      }
      for (const mv of indexMoves[k]!) {
        for (const oldEk of mv.removals) istore.remove(oldEk);
        for (const newEk of mv.insertions) {
          if (!istore.insert(newEk, []))
            throw new Error("index entry keys are unique (storage-key suffix)");
        }
      }
    }
    return { affected, returned };
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
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): { nodes: RExpr[]; names: string[]; types: string[] } {
    const scope = Scope.returning(this, table, baseIsOld);
    scope.ctes = ctes;
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
    ctes: CteCtx,
    meter: Meter,
  ): Value[][] {
    const env: EvalEnv = {
      params,
      outer: [],
      runSubquery: (p, o) => this.execQueryPlan(p, o, params, ctes),
      seam: this.session.seam,
      rng: new StmtRng(),
      ctes,
      exec: this,
    };
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
  private executeDelete(del: Delete, params: Value[], ctx: CteCtx): Outcome {
    const table = this.lkpTable(del.table); // temp-first (temp-tables.md §3)
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + del.table);
    }
    // Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002): a
    // DELETE must locate + remove a stored key, which a skewed encoding cannot match.
    this.ensureCollationsWritable(table.columns);
    // Per-column frozen collations for the collated text key form (§2.12) — indexes both the FK
    // parent-side probe (parent is this table) and the index-entry path.
    const colls = this.columnCollations(table.columns);
    // DELETE is single-table; resolve its WHERE against a one-relation scope. The RETURNING
    // projection resolves after it (PostgreSQL's analysis order), against the same scope
    // (grammar.md §32). The statement's CTE bindings (writable-cte.md) are visible so a WHERE /
    // RETURNING sublink may reference an earlier CTE.
    const scope = Scope.single(this, table);
    scope.ctes = ctx.bindings;
    const ptypes = new ParamTypes();
    let filter = del.filter ? resolveBooleanFilter(scope, del.filter, ptypes) : null;
    const ret =
      del.returning !== null
        ? this.resolveReturning(table, del.returning, true, ctx.bindings, ptypes)
        : null;
    const bound = bindParams(params, ptypes.finalize());

    // Fold globally-uncorrelated WHERE subqueries once (their cost is added a single time —
    // spec/design/grammar.md §26, cost.md §3); a correlated one stays and re-runs per row via the
    // per-row outer environment below (it pushes the current row, so `target.col` reads it). The
    // uncorrelated execution reads the pre-DELETE snapshot (keys are collected before mutating).
    // Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13; cost.md §3).
    const meter = this.session.newMeter();
    if (filter !== null) {
      const cost = { value: 0n };
      filter = this.foldUncorrelatedInRExpr(filter, bound, ctx, cost);
      meter.charge(cost.value);
    }
    // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
    // pre-statement snapshot (grammar.md §32).
    if (ret !== null) {
      const cost = { value: 0n };
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, ctx, cost));
      meter.charge(cost.value);
    }
    const env: EvalEnv = {
      params: bound,
      outer: [],
      runSubquery: (p, o) => this.execQueryPlan(p, o, bound, ctx),
      seam: this.session.seam,
      rng: new StmtRng(),
      ctes: ctx,
      exec: this,
    };
    // The SCAN reads the visible snapshot (the read pin under a data-modifying WITH — writable-cte.md
    // §2; else working == read-your-writes); the phase-2 REMOVAL writes the transaction's working set.
    const store = this.lkpStore(del.table);
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
    const mb = mutationPkBound(table, filter, this.readSnap());
    let entries: Entry[] | null;
    let overlap: number;
    let slabs: number;
    if (mb !== null) {
      ({ entries, overlap, slabs } = scanEntries(store, mb, bound, mask));
    } else {
      // GIN bound (gin.md §6): when no PK bound applies, a GIN-accelerable WHERE conjunct bounds the
      // delete's target-row scan through the index instead of a full scan (PK-then-GIN-then-full; the
      // ordered-index bound stays SELECT-only). readSnap()==working() during a mutation (tx open), so
      // this reads the read-your-writes state. ginEntry charged inside; the block below.
      const gb = detectGinBound(filter, table.indexes, table.columns, 0);
      if (gb !== null) {
        const m = filter !== null ? ginMatch(filter, gb.colGlobal) : null;
        const r = this.ginBoundRows(del.table, gb, m?.query ?? null, env, meter, mask);
        entries = r.entries;
        overlap = r.pages;
        slabs = r.slabs;
      } else {
        const u = store.scanWithUnits(mask);
        entries = u.entries;
        overlap = u.pages;
        slabs = u.slabs;
      }
    }
    if (entries === null)
      return dmlOutcome(ret?.names ?? null, ret?.types ?? null, null, 0, meter.accrued); // empty bound
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
          // parent is the delete target itself, so its key columns use colls (§2.12).
          const probe = fkProbe(fk, parent, colls, m.row, fk.refColumns);
          if (probe === null) continue; // a NULL referenced value cannot be referenced (MATCH SIMPLE)
          if (this.fkChildReferences(childTable, fk, parent, fkProbeBytes(probe), exclude)) {
            throw engineError(
              "foreign_key_violation",
              "update or delete on table " +
                parent.name +
                " violates foreign key constraint " +
                fk.name +
                " on table " +
                childTable,
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
        ? this.projectReturning(
            ret.nodes,
            matched.map((m) => m.row),
            null,
            bound,
            ctx,
            meter,
          )
        : null;
    // Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
    // unmetered write work; an index removal cannot fail).
    const writeStore = this.writeStore(del.table);
    for (const m of matched) writeStore.remove(m.key);
    for (const def of table.indexes) {
      const istore = this.writeIndexStore(def.name.toLowerCase());
      for (const m of matched) {
        for (const ek of indexEntryKeys(table.columns, colls, def, m.key, m.row)) istore.remove(ek);
      }
    }
    return dmlOutcome(
      ret?.names ?? null,
      ret?.types ?? null,
      returned,
      matched.length,
      meter.accrued,
    );
  }

  // executeUpdate is two-phase / all-or-nothing: phase 1 builds and type-checks every
  // matching row's new values (assignments evaluate against the OLD row, so
  // `SET a = b, b = a` swaps); a 22003/23502 aborts with no writes. Phase 2 applies.
  // Assigning a PRIMARY KEY column traps 0A000 (the storage key must not change this
  // slice); a duplicate target column traps 42701. No WHERE updates every row.
  private executeUpdate(upd: Update, params: Value[], ctx: CteCtx): Outcome {
    const table = this.lkpTable(upd.table); // temp-first (temp-tables.md §3)
    if (!table) {
      throw engineError("undefined_table", "table does not exist: " + upd.table);
    }
    // Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002): an
    // UPDATE re-encodes + re-places keys, which a skewed encoding would corrupt.
    this.ensureCollationsWritable(table.columns);
    // Per-column frozen collations for the collated text key form (§2.12) — indexes both the FK
    // probe and the index-entry move path.
    const colls = this.columnCollations(table.columns);

    // UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
    // shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
    // The statement's CTE bindings (writable-cte.md) are visible so a SET / WHERE / RETURNING
    // sublink may reference an earlier CTE.
    const scope = Scope.single(this, table);
    scope.ctes = ctx.bindings;
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
      // A GENERATED ALWAYS identity column can only be set to DEFAULT (sequences.md §13.4); jed's
      // UPDATE has no `= DEFAULT` form, so any assignment is 428C9. Ordered before the PK-narrowing
      // 0A000 so an ALWAYS identity PRIMARY KEY reports 428C9 (PG's code).
      if (table.columns[idx]!.identity === "always") {
        throw engineError("generated_always", `column ${a.column} can only be updated to DEFAULT`);
      }
      if (pkMembers.includes(idx)) {
        throw engineError(
          "feature_not_supported",
          "updating a primary key column is not supported",
        );
      }
      for (const p of plans) {
        if (p.idx === idx) {
          throw engineError("duplicate_column", "column " + a.column + " assigned more than once");
        }
      }
      const col = table.columns[idx]!;
      // Updating a composite-typed column lands in a later slice (the storable + INSERT/SELECT
      // round-trip is S3 — spec/design/composite.md §12); reject it for now (0A000).
      if (col.type.kind === "composite") {
        throw engineError(
          "feature_not_supported",
          "updating composite column " + a.column + " is not supported yet",
        );
      }
      if (col.type.kind === "array") {
        throw engineError(
          "feature_not_supported",
          "updating array column " + a.column + " is not supported yet",
        );
      }
      if (col.type.kind === "range") {
        // Updating a range column is deferred this slice (the storable column round-trips via INSERT
        // VALUES; assignment lands with the range operator surface) — reject it 0A000, like array.
        throw engineError(
          "feature_not_supported",
          "updating range column " + a.column + " is not supported yet",
        );
      }
      const targetScalar = col.type.scalar;
      // The RHS is a general expression evaluated against the OLD row; a literal operand
      // adapts to the target column's type. The result must be assignable to the column's
      // family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
      const { node, type } = resolve(
        scope,
        a.value,
        targetScalar,
        { collecting: false, groupKeys: [], specs: [] },
        ptypes,
      );
      requireAssignable(type, targetScalar, a.column);
      plans.push({
        idx,
        name: col.name,
        target: targetScalar,
        decimal: col.decimal,
        notNull: col.notNull,
        source: node,
      });
    }

    let filter = upd.filter ? resolveBooleanFilter(scope, upd.filter, ptypes) : null;
    // The RETURNING projection resolves last (PostgreSQL's analysis order), against the same
    // one-relation scope; it evaluates each matched row's NEW values (grammar.md §32).
    const ret =
      upd.returning !== null
        ? this.resolveReturning(table, upd.returning, false, ctx.bindings, ptypes)
        : null;
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
    const meter = this.session.newMeter();
    const foldCost = { value: 0n };
    for (const p of plans) p.source = this.foldUncorrelatedInRExpr(p.source, bound, ctx, foldCost);
    if (filter !== null) filter = this.foldUncorrelatedInRExpr(filter, bound, ctx, foldCost);
    if (ret !== null) {
      ret.nodes = ret.nodes.map((node) => this.foldUncorrelatedInRExpr(node, bound, ctx, foldCost));
    }
    meter.charge(foldCost.value);
    const env: EvalEnv = {
      params: bound,
      outer: [],
      runSubquery: (p, o) => this.execQueryPlan(p, o, bound, ctx),
      seam: this.session.seam,
      rng: new StmtRng(),
      ctes: ctx,
      exec: this,
    };
    // The SCAN + spilled-value reads + compress-cost weigh the visible snapshot (the read pin under a
    // data-modifying WITH — writable-cte.md §2; else working == read-your-writes); the phase-2 REPLACE
    // writes the transaction's working set.
    const store = this.lkpStore(upd.table);
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
    const mb = mutationPkBound(table, filter, this.readSnap());
    let entries: Entry[] | null;
    let overlap: number;
    let slabs: number;
    if (mb !== null) {
      ({ entries, overlap, slabs } = scanEntries(store, mb, bound, mask));
    } else {
      // GIN bound (gin.md §6): when no PK bound applies, a GIN-accelerable WHERE conjunct bounds the
      // update's target-row scan through the index over the PRE-update state (PK-then-GIN-then-full;
      // the ordered-index bound stays SELECT-only). ginEntry charged inside; the block below.
      const gb = detectGinBound(filter, table.indexes, table.columns, 0);
      if (gb !== null) {
        const m = filter !== null ? ginMatch(filter, gb.colGlobal) : null;
        const r = this.ginBoundRows(upd.table, gb, m?.query ?? null, env, meter, mask);
        entries = r.entries;
        overlap = r.pages;
        slabs = r.slabs;
      } else {
        const u = store.scanWithUnits(mask);
        entries = u.entries;
        overlap = u.pages;
        slabs = u.slabs;
      }
    }
    if (entries === null)
      return dmlOutcome(ret?.names ?? null, ret?.types ?? null, null, 0, meter.accrued); // empty bound
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
        const istore = this.lkpIndexStore(def.name.toLowerCase());
        const batch = new Set<string>();
        for (const u of updates) {
          const prefix = indexPrefixKey(table.columns, colls, def, u.row);
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
      // The probe matches the parent's stored key, so a collated parent key column uses the
      // PARENT's collation (§2.12).
      const parentColls = this.columnCollations(parent.columns);
      const batch = new Set<string>();
      if (fk.refTable.toLowerCase() === relation.toLowerCase()) {
        for (const u of updates) {
          const p = fkProbe(fk, parent, parentColls, u.row, fk.refColumns);
          if (p !== null) batch.add(fkProbeBytes(p).join(","));
        }
      }
      for (const u of updates) {
        const probe = fkProbe(fk, parent, parentColls, u.row, fk.columns);
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
        // parent is the update target itself, so its key columns use colls (§2.12).
        const newPresent = new Set<string>();
        for (const u of updates) {
          const p = fkProbe(fk, parent, colls, u.row, fk.refColumns);
          if (p !== null) newPresent.add(fkProbeBytes(p).join(","));
        }
        const exclude = childTable.toLowerCase() === upd.table.toLowerCase() ? updatedKeys : empty;
        for (const u of updates) {
          const oldProbe = fkProbe(fk, parent, colls, u.oldRow, fk.refColumns);
          if (oldProbe === null) continue; // a NULL old referenced value was referenced by nothing
          // Unchanged tuples (incl. a NULL → already skipped) do not disappear.
          const newProbe = fkProbe(fk, parent, colls, u.row, fk.refColumns);
          if (newProbe !== null && bytesEq(fkProbeBytes(newProbe), fkProbeBytes(oldProbe)))
            continue;
          // Re-supplied by another updated row (e.g. a value swap) → not disappearing.
          if (newPresent.has(fkProbeBytes(oldProbe).join(","))) continue;
          if (this.fkChildReferences(childTable, fk, parent, fkProbeBytes(oldProbe), exclude)) {
            throw engineError(
              "foreign_key_violation",
              "update or delete on table " +
                parent.name +
                " violates foreign key constraint " +
                fk.name +
                " on table " +
                childTable,
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
        ? this.projectReturning(
            ret.nodes,
            updates.map((u) => u.row),
            updates.map((u) => u.oldRow),
            bound,
            ctx,
            meter,
          )
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
        const oldEks = indexEntryKeys(table.columns, colls, def, u.key, u.oldRow);
        const newEks = indexEntryKeys(table.columns, colls, def, u.key, u.row);
        const removals = bytesDiff(oldEks, newEks);
        const insertions = bytesDiff(newEks, oldEks);
        if (removals.length > 0 || insertions.length > 0) {
          indexMoves[k]!.push({ removals, insertions });
        }
      }
    }

    // Phase 2: apply (keys unchanged — a PK column can't be assigned), then move the
    // changed index entries (unmetered write work; cannot fail). The REPLACE targets the working set.
    const writeStore = this.writeStore(upd.table);
    for (const u of updates) writeStore.replace(u.key, u.row);
    for (let k = 0; k < table.indexes.length; k++) {
      const istore = this.writeIndexStore(table.indexes[k]!.name.toLowerCase());
      for (const mv of indexMoves[k]!) {
        for (const oldEk of mv.removals) istore.remove(oldEk);
        for (const newEk of mv.insertions) {
          if (!istore.insert(newEk, [])) {
            throw new Error("index entry keys are unique (storage-key suffix)");
          }
        }
      }
    }
    return dmlOutcome(
      ret?.names ?? null,
      ret?.types ?? null,
      returned,
      updates.length,
      meter.accrued,
    );
  }

  // executeSelect runs a SELECT as a top-level statement: runSelect, then wrap as a query
  // Outcome (the projection types are internal — only INSERT ... SELECT consumes them).
  private executeSelect(sel: Select, params: Value[]): Outcome {
    const r = this.runSelect(sel, params);
    return {
      kind: "query",
      columnNames: r.columnNames,
      columnTypes: typeNames(r.columnTypes),
      rows: r.rows,
      cost: r.cost,
    };
  }

  // executeSetOp runs a set operation as a top-level statement: runSetOp, then wrap as a query
  // Outcome. Cost is lhs.cost + rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
  private executeSetOp(so: SetOp, params: Value[]): Outcome {
    const r = this.runSetOp(so, params);
    return {
      kind: "query",
      columnNames: r.columnNames,
      columnTypes: typeNames(r.columnTypes),
      rows: r.rows,
      cost: r.cost,
    };
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
    // A WITH containing any data-modifying part (a data-modifying CTE or a data-modifying primary)
    // runs through the writable-CTE orchestrator (spec/design/writable-cte.md): it pins the
    // pre-statement snapshot and runs the parts in lexical order, all-or-nothing. A pure-query WITH
    // keeps the existing read-only path (cte.md) unchanged.
    if (withHasDml(wq)) {
      return this.executeWithDml(wq, params);
    }
    const r = this.runWith(wq, params);
    return {
      kind: "query",
      columnNames: r.columnNames,
      columnTypes: typeNames(r.columnTypes),
      rows: r.rows,
      cost: r.cost,
    };
  }

  // planCteBindings plans every CTE in a WITH list into bindings (spec/design/cte.md §2,
  // writable-cte.md). Each body is planned against the prefix of EARLIER bindings (parent = null — a
  // body is an independent query, NOT correlated to a reference site). Under WITH RECURSIVE a query
  // CTE that references its own name is the recursive shape (its binding is pushed BEFORE planning the
  // recursive term, so the self-reference resolves to it). A DATA-MODIFYING CTE body resolves only its
  // RETURNING schema here (its effect runs later, in the orchestrator) — a data-modifying body is
  // never the recursive UNION shape, so it is always non-recursive. The `refs` counters are bumped as
  // later query bodies / a query primary reference each binding (a data-modifying part's references
  // are static-counted by the orchestrator, since it is not planned here).
  private planCteBindings(ctes: Cte[], recursive: boolean, ptypes: ParamTypes): CteBinding[] {
    const bindings: CteBinding[] = [];
    for (const cte of ctes) {
      const lname = cte.name.toLowerCase();
      if (bindings.some((b) => b.name === lname)) {
        throw engineError("duplicate_alias", `WITH query name ${lname} specified more than once`);
      }
      const bodyQuery = cteBodyAsQuery(cte.body);
      const shape =
        recursive && bodyQuery !== null
          ? analyzeRecursiveCte(lname, bodyQuery)
          : { recursive: false as const, unionAll: false };
      if (shape.recursive) {
        // The body is `anchor UNION[ALL] recursive_term` (analyzeRecursiveCte verified).
        const so = bodyQuery as SetOp;
        const anchorPlan = this.planQuery(so.lhs, null, bindings, ptypes);
        const table = cteSyntheticTable(lname, anchorPlan, cte.columns);
        bindings.push({
          name: lname,
          table,
          source: { kind: "query", plan: anchorPlan },
          recursive: null,
          hint: cte.materialized,
          refs: 0,
        });
        const bi = bindings.length - 1;
        const rhsPlan = this.planQuery(so.rhs, null, bindings, ptypes);
        const anchorSrc = bindings[bi]!.source;
        if (anchorSrc.kind !== "query") {
          throw new Error("the anchor binding was just pushed as a query source");
        }
        checkRecursiveColumnTypes(anchorSrc.plan, rhsPlan, lname);
        bindings[bi]!.recursive = { plan: rhsPlan, unionAll: shape.unionAll };
        continue;
      }
      if (bodyQuery !== null) {
        const plan = this.planQuery(bodyQuery, null, bindings, ptypes);
        const table = cteSyntheticTable(lname, plan, cte.columns);
        bindings.push({
          name: lname,
          table,
          source: { kind: "query", plan },
          recursive: null,
          hint: cte.materialized,
          refs: 0,
        });
      } else {
        // A data-modifying CTE (writable-cte.md): resolve its RETURNING schema for the synthetic
        // relation + capture the statement to run later.
        const { table, dm } = this.planDmCte(lname, cte.body, bindings, cte.columns, ptypes);
        bindings.push({
          name: lname,
          table,
          source: { kind: "dml", dm },
          recursive: null,
          hint: cte.materialized,
          refs: 0,
        });
      }
    }
    return bindings;
  }

  // planDmCte plans a data-modifying CTE body (spec/design/writable-cte.md): resolve its RETURNING
  // schema (against the EARLIER bindings, so a RETURNING sublink may reference an earlier CTE) to
  // build the synthetic relation, and capture the statement to execute later. A body with no
  // RETURNING yields a zero-column relation flagged noReturning (a FROM reference to it is 0A000,
  // §5). The target must be a base table — a CTE name / missing table is 42P01 (§1).
  private planDmCte(
    lname: string,
    body: CteBody,
    bindings: CteBinding[],
    rename: string[] | null,
    ptypes: ParamTypes,
  ): { table: Table; dm: DmCte } {
    let tableName: string;
    let returning: SelectItems | null;
    let baseIsOld: boolean;
    let stmt: DmStmt;
    if (body.kind === "insert") {
      tableName = body.table;
      returning = body.returning;
      baseIsOld = false;
      stmt = body;
    } else if (body.kind === "update") {
      tableName = body.table;
      returning = body.returning;
      baseIsOld = false;
      stmt = body;
    } else if (body.kind === "delete") {
      tableName = body.table;
      returning = body.returning;
      baseIsOld = true;
      stmt = body;
    } else {
      throw new Error("planDmCte requires a data-modifying body");
    }
    const tdef = this.lkpTable(tableName); // temp-first (temp-tables.md §3)
    if (tdef === undefined) {
      throw engineError("undefined_table", "table does not exist: " + tableName);
    }
    if (returning === null) {
      const table = cteSyntheticTableCols(lname, [], [], rename);
      return { table, dm: { stmt, noReturning: true } };
    }
    const scope = Scope.returning(this, tdef, baseIsOld);
    scope.ctes = bindings;
    const { names, types } = resolveProjections(
      scope,
      returning,
      { collecting: false, groupKeys: [], specs: [] },
      ptypes,
    );
    const table = cteSyntheticTableCols(lname, names, types, rename);
    return { table, dm: { stmt, noReturning: false } };
  }

  // runWith runs a pure-query WITH (spec/design/cte.md) — the path for a WITH with no data-modifying
  // part (a data-modifying WITH goes through executeWithDml). (1) PLAN every CTE binding against the
  // prefix; (2) plan the main body with all bindings visible, threading the one ParamTypes so $N
  // infers statement-wide; (3) decide each CTE's mode from its reference count + [NOT] MATERIALIZED
  // hint; (4) MATERIALIZE each referenced materialized CTE once, in list order (a later body sees the
  // earlier buffers); (5) fold + EXECUTE the main body with the CTE context. Cost composes like set
  // operations — a sum of the parts.
  private runWith(wq: WithQuery, params: Value[]): SelectResult {
    const ptypes = new ParamTypes();
    const bindings = this.planCteBindings(wq.ctes, wq.recursive, ptypes);
    // (2) Plan the main body with all bindings visible (the pure-query path always has a query
    //     primary — a data-modifying primary routes to executeWithDml).
    const bodyQuery = cteBodyAsQuery(wq.body);
    if (bodyQuery === null) {
      throw new Error("runWith is the pure-query path");
    }
    const plan = this.planQuery(bodyQuery, null, bindings, ptypes);
    const bound = bindParams(params, ptypes.finalize());
    const modes = cteModes(bindings);
    const { buffers, totalCost } = this.materializeCtes(bindings, modes, bound);

    // (5) Fold + execute the main body against the full CTE context.
    const ctx: CteCtx = { modes, bindings, buffers };
    const subqueryCost = { value: 0n };
    this.foldUncorrelatedInPlan(plan, bound, ctx, subqueryCost);
    const r = this.execQueryPlan(plan, [], bound, ctx);
    return { ...r, cost: r.cost + subqueryCost.value + totalCost };
  }

  // materializeCtes materializes each CTE once, in list order (spec/design/cte.md §3) — the shared
  // loop for the pure-query and writable-CTE paths' query/recursive CTEs. A data-modifying CTE is NOT
  // run here (the orchestrator runs it for its effect — runWithDml); its buffer slot is left empty for
  // the orchestrator to fill. Returns the filled buffers + the accrued materialization cost (a later
  // body sees the earlier buffers).
  private materializeCtes(
    bindings: CteBinding[],
    modes: CteMode[],
    bound: Value[],
  ): { buffers: Row[][]; totalCost: bigint } {
    const totalCost = { value: 0n };
    const buffers: Row[][] = [];
    for (let i = 0; i < bindings.length; i++) {
      const rt = bindings[i]!.recursive;
      let buf: Row[];
      if (rt) {
        buf = this.materializeRecursive(i, rt, modes, bindings, buffers, bound, totalCost);
      } else if (bindings[i]!.source.kind === "dml") {
        // A data-modifying CTE's buffer is filled by the orchestrator, not here.
        buf = [];
      } else if (modes[i] === "materialize") {
        const ctx: CteCtx = {
          modes: modes.slice(0, i),
          bindings: bindings.slice(0, i),
          buffers,
        };
        const src = bindings[i]!.source;
        if (src.kind !== "query") {
          throw new Error("the data-modifying arm was handled above");
        }
        const r = this.execQueryPlan(src.plan, [], bound, ctx);
        totalCost.value += r.cost;
        buf = r.rows;
      } else {
        buf = [];
      }
      buffers.push(buf);
    }
    return { buffers, totalCost: totalCost.value };
  }

  // materializeRecursive materializes a RECURSIVE CTE by iterating to a fixpoint — the PostgreSQL
  // working-table method (spec/design/recursive-cte.md §4). rt is the recursive term (which references
  // this CTE, index ci); the anchor is bindings[ci].source. priorBuffers are the earlier CTEs'
  // materialized rows (visible to both terms). totalCost accrues every term evaluation's cost and
  // gates the per-statement ceiling between iterations, so a non-terminating recursion of cheap
  // iterations still aborts 54P01 at the identical accrued cost in every core (recursive-cte.md §5).
  private materializeRecursive(
    ci: number,
    rt: RecursiveTerm,
    modes: CteMode[],
    bindings: CteBinding[],
    priorBuffers: Row[][],
    params: Value[],
    totalCost: { value: bigint },
  ): Row[] {
    const anchorSrc = bindings[ci]!.source;
    if (anchorSrc.kind !== "query") {
      throw new Error("a recursive CTE's anchor is a query plan");
    }
    const anchorPlan = anchorSrc.plan;
    const maxCost = this.session.maxCost;
    const guard = (total: bigint): void => {
      if (maxCost > 0n && total >= maxCost) {
        throw engineError(
          "cost_limit_exceeded",
          `query exceeded the cost limit of ${maxCost} (accrued ${total})`,
        );
      }
    };
    const anchorTypes = anchorPlan.columnTypes;
    const rhsTypes = rt.plan.columnTypes;

    // Evaluate the anchor: its rows seed both the result and the first working table.
    const ar = this.execQueryPlan(anchorPlan, [], params, {
      modes: modes.slice(0, ci),
      bindings: bindings.slice(0, ci),
      buffers: priorBuffers,
    });
    totalCost.value += ar.cost;
    guard(totalCost.value);

    // For UNION (distinct) a seen set drops rows duplicating any already-emitted row, keyed by the
    // NULL-safe distinctRowKey the set operators use.
    const seen = new Set<string>();
    const keep = (row: Row): boolean => {
      if (rt.unionAll) return true;
      const k = distinctRowKey(row);
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    };
    const result: Row[] = [];
    let working: Row[] = [];
    for (const row of ar.rows) {
      if (keep(row)) {
        result.push(row);
        working.push(row);
      }
    }

    // The recursive term scans the WORKING table through the CTE's own buffer slot (ci); the earlier
    // CTEs keep their full buffers. Build the buffer array once and swap slot ci per iteration.
    const rhsBuffers: Row[][] = priorBuffers.slice(0, ci);
    rhsBuffers.push([]); // slot ci

    while (working.length > 0) {
      rhsBuffers[ci] = working;
      working = [];
      const rr = this.execQueryPlan(rt.plan, [], params, {
        modes: modes.slice(0, ci + 1),
        bindings: bindings.slice(0, ci + 1),
        buffers: rhsBuffers,
      });
      totalCost.value += rr.cost;
      guard(totalCost.value);
      coerceSetopRows(rr.rows, rhsTypes, anchorTypes);
      for (const row of rr.rows) {
        if (keep(row)) {
          result.push(row);
          working.push(row);
        }
      }
    }
    return result;
  }

  // executeWithDml runs a data-modifying WITH statement (spec/design/writable-cte.md): a WITH
  // containing a data-modifying CTE and/or a data-modifying primary. It PINS the pre-statement
  // snapshot for every sub-statement's reads (§2 — so the parts cannot see each other's table writes;
  // data crosses only via a CTE's RETURNING buffer), runs the parts in lexical order, and returns the
  // primary's result. The whole statement is one all-or-nothing transaction — the autocommit (or
  // block) wrapper publishes the accumulated working only if this returns without throwing (§6).
  private executeWithDml(wq: WithQuery, params: Value[]): Outcome {
    // Pin the pre-statement snapshot. A write statement runs with a transaction open (autocommit
    // opened one), and nothing is written yet, so the pin equals working == committed. Cleared on
    // every exit path so the next statement reads normally.
    const pin = this.readSnap().clone();
    this.session.readPin = pin;
    try {
      return this.runWithDml(wq, params);
    } finally {
      this.session.readPin = null;
    }
  }

  // runWithDml is the body of executeWithDml, run under the read pin. Plans every CTE binding + the
  // query primary, runs the data-modifying CTEs / materialized query CTEs in list order, then the
  // primary — every read against the pin, every write into the transaction's working.
  private runWithDml(wq: WithQuery, params: Value[]): Outcome {
    const { ctes, body, recursive } = wq;
    const ptypes = new ParamTypes();
    // (1) Plan every CTE binding (query plans + data-modifying RETURNING schemas).
    const bindings = this.planCteBindings(ctes, recursive, ptypes);
    // (2) Plan a query primary now (to bump refs + surface resolution errors, incl. a 0A000 FROM
    //     reference to a no-RETURNING data-modifying CTE). A data-modifying primary is resolved and
    //     run later (it sees the bindings via the threaded context); its references are
    //     static-counted in (2b).
    const primaryQuery = cteBodyAsQuery(body);
    const primaryPlan =
      primaryQuery !== null ? this.planQuery(primaryQuery, null, bindings, ptypes) : null;
    // (2b) Add the references each NON-planned data-modifying part (a data-modifying CTE body, or a
    //      data-modifying primary) contributes to each binding, so the inline-vs-materialize decision
    //      is correct for a query CTE referenced only by a data-modifying part (§3). Query bodies / a
    //      query primary were already plan-counted in (1)/(2).
    for (const cte of ctes) {
      if (cteBodyIsDataModifying(cte.body)) {
        for (const b of bindings) {
          b.refs += countCteRefsDml(cte.body, b.name);
        }
      }
    }
    if (cteBodyIsDataModifying(body)) {
      for (const b of bindings) {
        b.refs += countCteRefsDml(body, b.name);
      }
    }
    const bound = bindParams(params, ptypes.finalize());
    const modes = cteModes(bindings);

    // (3) Run each CTE in list order, filling its buffer. A data-modifying CTE executes for its
    //     effect + RETURNING buffer; the query/recursive CTEs use the shared materialize loop.
    const totalCost = { value: 0n };
    const buffers: Row[][] = [];
    for (let i = 0; i < bindings.length; i++) {
      const rt = bindings[i]!.recursive;
      let buf: Row[];
      if (rt) {
        buf = this.materializeRecursive(i, rt, modes, bindings, buffers, bound, totalCost);
      } else if (bindings[i]!.source.kind === "dml") {
        const ctx: CteCtx = {
          modes: modes.slice(0, i),
          bindings: bindings.slice(0, i),
          buffers,
        };
        const { rows, cost } = this.execDmCte(i, bindings, bound, ctx);
        totalCost.value += cost;
        buf = rows;
      } else if (modes[i] === "materialize") {
        const ctx: CteCtx = {
          modes: modes.slice(0, i),
          bindings: bindings.slice(0, i),
          buffers,
        };
        const src = bindings[i]!.source;
        if (src.kind !== "query") {
          throw new Error("the data-modifying arm was handled above");
        }
        const r = this.execQueryPlan(src.plan, [], bound, ctx);
        totalCost.value += r.cost;
        buf = r.rows;
      } else {
        buf = [];
      }
      buffers.push(buf);
    }

    // (4) Execute the primary against the full CTE context, adding the materialization cost.
    const ctx: CteCtx = { modes, bindings, buffers };
    let outcome: Outcome;
    if (body.kind === "select" || body.kind === "setOp" || body.kind === "withExpr") {
      const plan = primaryPlan!;
      const subqueryCost = { value: 0n };
      this.foldUncorrelatedInPlan(plan, bound, ctx, subqueryCost);
      const r = this.execQueryPlan(plan, [], bound, ctx);
      outcome = {
        kind: "query",
        columnNames: r.columnNames,
        columnTypes: typeNames(r.columnTypes),
        rows: r.rows,
        cost: r.cost + subqueryCost.value,
      };
    } else if (body.kind === "insert") {
      outcome = this.executeInsert(body, params, ctx);
    } else if (body.kind === "update") {
      outcome = this.executeUpdate(body, params, ctx);
    } else {
      outcome = this.executeDelete(body, params, ctx);
    }
    return addOutcomeCost(outcome, totalCost.value);
  }

  // execDmCte executes a data-modifying CTE (spec/design/writable-cte.md §3): run the
  // INSERT/UPDATE/DELETE at binding i for its effect, with the earlier bindings/buffers in scope (so
  // its inner queries may reference an earlier CTE), and return its RETURNING rows (the buffer the
  // later parts scan) + its cost. A body with no RETURNING runs for its effect and buffers no rows.
  private execDmCte(
    i: number,
    bindings: CteBinding[],
    params: Value[],
    ctx: CteCtx,
  ): { rows: Row[]; cost: bigint } {
    const src = bindings[i]!.source;
    if (src.kind !== "dml") {
      throw new Error("execDmCte requires a data-modifying binding");
    }
    const stmt = src.dm.stmt;
    let outcome: Outcome;
    if (stmt.kind === "insert") {
      outcome = this.executeInsert(stmt, params, ctx);
    } else if (stmt.kind === "update") {
      outcome = this.executeUpdate(stmt, params, ctx);
    } else {
      outcome = this.executeDelete(stmt, params, ctx);
    }
    if (outcome.kind === "query") {
      return { rows: outcome.rows, cost: outcome.cost };
    }
    return { rows: [], cost: outcome.cost };
  }

  // planQuery resolves a query expression into an owned QueryPlan against the scope chain (parent =
  // the enclosing query's scope, null at top level). A subquery is planned here, once (§26). `ctes`
  // is the statement's CTE bindings visible here (spec/design/cte.md §2) — inherited into every
  // nested scope, never via the parent chain.
  // Not private: the free function planSubquery calls it through scope.catalog (an internal seam).
  planQuery(
    qe: QueryExpr,
    parent: Scope | null,
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): QueryPlan {
    if (qe.kind === "select") return this.planSelect(qe, parent, ctes, ptypes);
    if (qe.kind === "withExpr") return this.planWithExpr(qe, parent, ptypes);
    return this.planSetOp(qe, parent, ctes, ptypes);
  }

  // planWithExpr plans a nested `WITH … query_expr` (spec/design/cte.md §7) into a WithPlan. The
  // nested CTEs establish their OWN scope: the bodies and the inner main query see ONLY these CTEs
  // (and the catalog) — the enclosing statement's CTE bindings are NOT inherited (a documented
  // narrowing, cte.md §7), so planCteBindings and the body are planned without the outer ctes. The
  // inner main query keeps the enclosing parent (so a LATERAL derived-table body still correlates to
  // its left siblings), while the CTE bodies stay independent (parent=null, inside planCteBindings).
  // A data-modifying CTE here is rejected 0A000 — PostgreSQL restricts a DML-WITH to the top level.
  private planWithExpr(we: WithExpr, parent: Scope | null, ptypes: ParamTypes): WithPlan {
    for (const c of we.ctes) {
      if (cteBodyIsDataModifying(c.body)) {
        throw engineError(
          "feature_not_supported",
          `WITH clause containing a data-modifying statement (${c.name}) is only supported at the top level`,
        );
      }
    }
    const bindings = this.planCteBindings(we.ctes, we.recursive, ptypes);
    const body = this.planQuery(we.body, parent, bindings, ptypes);
    return {
      kind: "with",
      bindings,
      modes: cteModes(bindings),
      body,
      columnNames: body.columnNames,
      columnTypes: body.columnTypes,
    };
  }

  // execQueryPlan executes a resolved plan against an outer-row environment (outer = the enclosing
  // rows, innermost last; empty at top level), the bound parameters, and the CTE context (a FROM
  // reference at any depth delivers a CTE's rows — spec/design/cte.md §5).
  private execQueryPlan(
    plan: QueryPlan,
    outer: Row[],
    params: Value[],
    ctes: CteCtx,
  ): SelectResult {
    if (plan.kind === "select") return this.execSelectPlan(plan, outer, params, ctes);
    if (plan.kind === "values") return this.execValuesPlan(plan, outer, params, ctes);
    if (plan.kind === "with") return this.execWithPlan(plan, outer, params);
    return this.execSetOpPlan(plan, outer, params, ctes);
  }

  // execWithPlan executes a nested WITH plan (spec/design/cte.md §7): materialize its CTE bindings
  // once (in list order, charging their cost), build a FRESH CTE context over them (the nested CTEs
  // establish their own scope — the enclosing context is NOT chained in, the documented narrowing
  // §7), and run the inner body against it. The body still sees the outer row environment (so a
  // LATERAL nested-WITH derived-table body correlates to its left siblings). The materialization cost
  // folds into the body's cost — the same shape as the top-level runWith (cte.md §3).
  private execWithPlan(plan: WithPlan, outer: Row[], params: Value[]): SelectResult {
    const { buffers, totalCost } = this.materializeCtes(plan.bindings, plan.modes, params);
    const ctx: CteCtx = {
      modes: plan.modes,
      bindings: plan.bindings,
      buffers,
    };
    const r = this.execQueryPlan(plan.body, outer, params, ctx);
    r.cost += totalCost;
    return r;
  }

  // planSetOp plans a set operation (spec/design/grammar.md §25): plan both operands with the same
  // parent scope, check arity + unify column types up front (so the 42601/42804 fire even over
  // empty operands), and resolve the trailing ORDER BY by output column name.
  private planSetOp(
    so: SetOp,
    parent: Scope | null,
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): SetOpPlan {
    const lhs = this.planQuery(so.lhs, parent, ctes, ptypes);
    const rhs = this.planQuery(so.rhs, parent, ctes, ptypes);

    if (lhs.columnTypes.length !== rhs.columnTypes.length) {
      throw engineError(
        "syntax_error",
        `each ${setopName(so.op)} query must have the same number of columns`,
      );
    }
    const columnTypes = lhs.columnTypes.map((l, i) =>
      unifySetopColumn(l, rhs.columnTypes[i]!, so.op),
    );
    const columnNames = lhs.columnNames;
    const order: OrderSlot[] = so.orderBy.map((key) => {
      const idx = resolveSetopOrderKey(key, columnNames);
      // An explicit COLLATE on a set-operation ORDER BY key (spec/design/collation.md §1): the output
      // column must be text (42804); the name resolves ("C", else loaded or 42704).
      let collation: Collation | null = null;
      if (key.collation !== null) {
        if (columnTypes[idx]!.kind !== "text") {
          throw typeError("collations are not supported by this column's type");
        }
        collation = resolveCollationName(this, key.collation);
      }
      return {
        idx,
        descending: key.descending,
        nullsFirst: key.nullsFirst,
        collation,
      };
    });
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
  private execSetOpPlan(
    plan: SetOpPlan,
    outer: Row[],
    params: Value[],
    ctes: CteCtx,
  ): SelectResult {
    const left = this.execQueryPlan(plan.lhs, outer, params, ctes);
    const right = this.execQueryPlan(plan.rhs, outer, params, ctes);

    coerceSetopRows(left.rows, left.columnTypes, plan.columnTypes);
    coerceSetopRows(right.rows, right.columnTypes, plan.columnTypes);

    let rows = combineSetop(plan.op, plan.all, left.rows, right.rows);
    const cost = left.cost + right.cost;

    if (plan.order.length > 0) {
      sortRows(rows, plan.order);
    }

    const n = BigInt(rows.length);
    const start = plan.offset === null ? 0n : plan.offset < n ? plan.offset : n;
    const end = plan.limit !== null && plan.limit < n - start ? start + plan.limit : n;
    rows = rows.slice(Number(start), Number(end));

    return {
      columnNames: plan.columnNames,
      columnTypes: plan.columnTypes,
      rows,
      cost,
    };
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
  private planValues(
    rows: Expr[][],
    parent: Scope | null,
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): ValuesPlan {
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
        const { node, type } = resolve(
          scope,
          val,
          null,
          { collecting: false, groupKeys: [], specs: [] },
          ptypes,
        );
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
    return {
      kind: "values",
      rows: resolvedRows,
      columnTypes: colTypes,
      columnNames,
    };
  }

  // execValuesPlan executes a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate
  // each row's values as constants over an EMPTY environment (no local row, no outer row —
  // non-LATERAL), coerce each to the unified column type (the only runtime change is int → decimal,
  // the set-operation rule), and emit the rows. Charges rowProduced per row plus each value's
  // operatorEval (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded into the
  // caller's meter via execQueryPlan.
  private execValuesPlan(
    plan: ValuesPlan,
    outer: Row[],
    params: Value[],
    ctes: CteCtx,
  ): SelectResult {
    const env: EvalEnv = {
      params,
      outer,
      runSubquery: (p, o) => this.execQueryPlan(p, o, params, ctes),
      seam: this.session.seam,
      rng: new StmtRng(),
      ctes,
      exec: this,
    };
    const meter = this.session.newMeter();
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
    return {
      columnNames: plan.columnNames,
      columnTypes: plan.columnTypes,
      rows,
      cost: meter.accrued,
    };
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
  private planSelect(
    sel: Select,
    parent: Scope | null,
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): SelectPlan {
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
          // A data-modifying CTE with no RETURNING produces no columns, so a FROM reference to it is
          // 0A000 (writable-cte.md §5; PostgreSQL's addRangeTableEntryForCTE check), raised at
          // resolution before any execution.
          const src = ctes[ci]!.source;
          if (src.kind === "dml" && src.dm.noReturning) {
            throw engineError(
              "feature_not_supported",
              `WITH query ${lname} does not have a RETURNING clause`,
            );
          }
          ctes[ci]!.refs += 1;
          t = ctes[ci]!.table;
          cteIdx = ci;
        } else {
          const tbl = this.lkpTable(tref.name); // temp-first (temp-tables.md §3)
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
        throw engineError(
          "feature_not_supported",
          "GROUP BY may not reference an outer query column",
        );
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
    // A window query (a select-list OVER call) resolves its projection in window mode, where bare
    // columns read the real row and window calls collect into synthetic slots
    // (spec/design/window.md §5.1). S0 narrows a window function combined with a GROUP BY /
    // aggregate to 0A000 (lifted in S3), so the two modes are mutually exclusive here.
    const hasWindowSyntax = itemsHaveWindow(sel.items);
    if (hasWindowSyntax && isAgg) {
      throw engineError(
        "feature_not_supported",
        "window functions with aggregates or GROUP BY are not supported yet",
      );
    }
    const projAgg: AggCtx = hasWindowSyntax
      ? {
          collecting: false,
          groupKeys,
          specs: [],
          window: { base: scope.width(), windowSpecs: [] },
        }
      : { collecting: isAgg, groupKeys, specs: [] };
    const {
      nodes: projections,
      names: columnNames,
      types: columnTypes,
    } = resolveProjections(scope, sel.items, projAgg, ptypes);
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
    // The collected window specs (empty unless a window query — a query is aggregate XOR window in
    // S0). spec/design/window.md §5.1.
    const windowSpecs: WindowSpec[] = projAgg.window?.windowSpecs ?? [];
    const hasWindow = windowSpecs.length > 0;
    // SELECT DISTINCT over an aggregate query's output (output-row dedup) is deferred (0A000).
    if (isAgg && sel.distinct) {
      throw engineError(
        "feature_not_supported",
        "SELECT DISTINCT with aggregates is not supported yet",
      );
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
      const r =
        key.qualifier !== null
          ? scope.resolveQualified(key.qualifier, key.column)
          : scope.resolveBare(key.column);
      if (r.level !== 0) {
        throw engineError(
          "feature_not_supported",
          "ORDER BY may not reference an outer query column",
        );
      }
      const idx = r.index;
      // The sort key's collation (spec/design/collation.md §1/§7). An explicit COLLATE must be on a
      // text column (42804) and name a loaded collation ("C" → byte order, else 42704); absent a
      // clause, the key inherits the column's frozen (implicit) collation — so `ORDER BY name` over
      // an en-US column sorts by en-US (slice 1d). A single column can't conflict (no 42P22 here).
      let collation: Collation | null = null;
      if (key.collation !== null) {
        if (!typeIsText(scope.columnAt(idx).type)) {
          throw typeError(
            `collations are not supported by type ${typeCanonicalName(scope.columnAt(idx).type)}`,
          );
        }
        collation = resolveCollationName(scope.catalog, key.collation);
      } else {
        const cn = scope.columnAt(idx).collation;
        if (cn !== null) collation = resolveCollationName(scope.catalog, cn);
      }
      let slot = idx;
      if (isAgg) {
        slot = groupKeys.indexOf(idx);
        if (slot < 0) throw groupingErrorColumn(key.column);
      }
      order.push({
        idx: slot,
        descending: key.descending,
        nullsFirst: key.nullsFirst,
        collation,
      });
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
      filter === null ||
      srfPlans[i] !== undefined ||
      rel.cte !== undefined ||
      derivedPlans[i] !== undefined
        ? null
        : detectScanBound(filter, rel, this.readSnap()),
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
      // A window query also reads each window function's PARTITION BY + ORDER BY key columns (real
      // input columns), beyond what the projection's window-result slots reference.
      for (const spec of windowSpecs) {
        for (const pk of spec.partition) touched[pk] = true;
        for (const o of spec.order) touched[o.idx] = true;
      }
    }
    const relMasks = planRels.map((r) => touched.slice(r.offset, r.offset + r.colCount));
    // ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a single base table,
    // non-aggregate, non-DISTINCT SELECT whose ORDER BY keys are a prefix of the relation's PRIMARY
    // KEY columns — each ASC, collation-matching the column's stored key form — needs no sort, since
    // the table scan already yields rows in that order. The streaming scan then elides the sort (and,
    // with a LIMIT, short-circuits a top-N).
    const pkOrdered =
      !isAgg &&
      !sel.distinct &&
      order.length > 0 &&
      planRels.length === 1 &&
      planRels[0]!.srf === undefined &&
      planRels[0]!.cte === undefined &&
      planRels[0]!.derived === undefined &&
      orderSatisfiedByPK(this.readSnap(), scope.rels[0]!.table, planRels[0]!.offset, order);
    return {
      kind: "select",
      rels: planRels,
      joins,
      filter,
      isAgg,
      groupKeys,
      aggSpecs,
      hasWindow,
      windowSpecs,
      having,
      order,
      projections,
      columnNames,
      columnTypes,
      distinct: sel.distinct,
      limit: sel.limit,
      offset: sel.offset,
      pkOrdered,
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
  private resolveSRF(
    name: string,
    args: Expr[],
    alias: string | null,
    parent: Scope | null,
    ctes: CteBinding[],
    ptypes: ParamTypes,
  ): { table: Table; srf: SrfPlan } {
    // The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings are
    // inherited so an arg subquery can reference a CTE (cte.md §2).
    const argScope = new Scope([], this, parent, true, ctes);
    const lname = name.toLowerCase();
    if (lname === "generate_series")
      return this.resolveGenerateSeries(args, alias, argScope, ptypes);
    if (lname === "unnest") return this.resolveUnnest(args, alias, argScope, ptypes);
    throw engineError("undefined_function", "function does not exist: " + name);
  }

  // resolveGenerateSeries resolves generate_series(start, stop[, step]) (spec/design/functions.md
  // §10): 2 or 3 integer args (a wrong arity/type → 42883). The produced column is typed at the
  // PROMOTED integer type of the args (PG); a NULL-typed arg contributes no width. All-NULL defaults
  // i64.
  private resolveGenerateSeries(
    args: Expr[],
    alias: string | null,
    argScope: Scope,
    ptypes: ParamTypes,
  ): { table: Table; srf: SrfPlan } {
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
  private resolveUnnest(
    args: Expr[],
    alias: string | null,
    argScope: Scope,
    ptypes: ParamTypes,
  ): { table: Table; srf: SrfPlan } {
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
    if (step === 0n)
      throw engineError("invalid_parameter_value", "step size cannot be equal to zero");
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
  // execStreamingScan executes the streaming primary-key-ordered scan path (spec/design/cost.md §3):
  // a single-table, no-blocking-operator query whose output order is already the table's primary-key
  // scan order — either no ORDER BY (the LIMIT short-circuit) or an ORDER BY satisfied by PK order
  // (plan.pkOrdered, set by orderSatisfiedByPK) — streams scan→filter→project with NO sort, and (when
  // there is a LIMIT) stops the scan the instant the LIMIT/OFFSET window is filled, charging
  // storageRowRead only for the rows actually read. With no LIMIT it emits every survivor after
  // OFFSET (the sort is simply elided — same rows, same cost as the eager/sort path).
  // Cost-equivalent to the eager path EXCEPT that a LIMIT reads (and filters) fewer rows — the
  // deliberate cost change. pageRead is the full block (the bound's node count); only the row reads
  // short-circuit. Rows match the eager path exactly: the offset..offset+limit slice of the
  // primary-key-ordered filtered rows (which, for a pkOrdered query, IS the ORDER BY's result).
  private execStreamingScan(
    plan: SelectPlan,
    env: EvalEnv,
    meter: Meter,
    params: Value[],
  ): SelectResult {
    const store = this.lkpStore(plan.rels[0]!.tableName);

    // Resolve the scan bound (the PK pushdown, if any) and charge the pageRead block. This path is
    // single-table (gated below), so the only relation is relBounds[0]. A correlated bound resolves
    // against env.outer (the enclosing rows).
    // An INDEX bound never streams — the dispatch gate routes it to the eager path
    // (cost.md §3 "LIMIT short-circuit").
    let bound: KeyBound = unboundedBound();
    let empty = false;
    const sb = plan.relBounds[0]!;
    if (sb !== null) {
      if (sb.kind === "index" || sb.kind === "gin")
        throw new Error("the streaming path is gated to PK/full scans");
      const b = buildKeyBound(sb.pk, params, env.outer);
      if (b === null) empty = true;
      else bound = b;
    }
    const su = empty ? { pages: 0, slabs: 0 } : store.overlapScanUnits(bound, plan.relMasks[0]!);
    meter.charge(COSTS.pageRead * BigInt(su.pages) + COSTS.valueDecompress * BigInt(su.slabs));

    // limit is optional here: a pkOrdered query may have no LIMIT (it streams every survivor in
    // order, eliding the sort), while the LIMIT short-circuit always has one.
    const limit = plan.limit;
    const offset = plan.offset ?? 0n;
    const out: Value[][] = [];
    // Skip the scan entirely for LIMIT 0 (no window to fill).
    if (!empty && limit !== 0n) {
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
        // Stop once a LIMIT window is filled; with no LIMIT, never stop early (emit every survivor
        // after OFFSET, in primary-key order).
        return limit === null ? true : BigInt(out.length) < limit;
      });
    }
    return {
      columnNames: plan.columnNames,
      columnTypes: plan.columnTypes,
      rows: out,
      cost: meter.accrued,
    };
  }

  // execStreamingSort is the streaming external sort for a single-table ORDER BY (spec/design/spill.md
  // §4/§5). It streams scan→filter→sorter, so the input is never materialized in the executor heap;
  // the sorter spills sorted runs to disk under workMem (file-backed databases) and k-way-merges them
  // at finish, then the window/projection loop pulls the sorted rows one at a time. Results + cost are
  // byte-identical to the eager sort: the same pageRead block, storageRowRead per scanned row, filter
  // operator_eval, and rowProduced per windowed row accrue — only the sort, which is unmetered
  // (cost.md §3), now spills. Gated (by the caller) to a single table, no join, non-aggregate,
  // non-DISTINCT, with an ORDER BY and no index bound.
  private execStreamingSort(
    plan: SelectPlan,
    env: EvalEnv,
    meter: Meter,
    params: Value[],
  ): SelectResult {
    const store = this.lkpStore(plan.rels[0]!.tableName);

    // Resolve the scan bound (the PK pushdown, if any) and charge the pageRead + valueDecompress block
    // up front — identical to the eager scan (cost.md §3). An INDEX bound never reaches here.
    let bound: KeyBound = unboundedBound();
    let empty = false;
    const sb = plan.relBounds[0]!;
    if (sb !== null) {
      if (sb.kind === "index" || sb.kind === "gin")
        throw new Error("the streaming sort path is gated to PK/full scans");
      const b = buildKeyBound(sb.pk, params, env.outer);
      if (b === null) empty = true;
      else bound = b;
    }
    const su = empty ? { pages: 0, slabs: 0 } : store.overlapScanUnits(bound, plan.relMasks[0]!);
    meter.charge(COSTS.pageRead * BigInt(su.pages) + COSTS.valueDecompress * BigInt(su.slabs));

    // A collated ORDER BY cannot use the C-ordered Sorter / spill (collated keys are slice 1e), and
    // collation is in-memory only this slice — so materialize the survivors and sort them with the
    // collation-aware decorate sorter (spec/design/collation.md §8). The metered costs (storageRowRead
    // per scanned row, rowProduced per windowed output) are identical to the Sorter path; the sort
    // itself is unmetered like every sort (cost.md §3).
    if (plan.order.some((k) => k.collation !== null)) {
      const rows: Row[] = [];
      if (!empty) {
        store.scanRange(bound, (_key, rawRow) => {
          meter.guard();
          meter.charge(COSTS.storageRowRead);
          const row = store.resolveColumns(rawRow, plan.relMasks[0]!);
          if (plan.filter !== null && !isTrue(evalExpr(plan.filter, row, env, meter))) {
            return true;
          }
          rows.push(row);
          return true;
        });
      }
      sortRows(rows, plan.order);
      const total = BigInt(rows.length);
      const offset = plan.offset ?? 0n;
      const start = offset < total ? offset : total;
      let end = total;
      if (plan.limit !== null && plan.limit < total - start) {
        end = start + plan.limit;
      }
      const out: Value[][] = [];
      for (let i = start; i < end; i++) {
        const row = rows[Number(i)]!;
        meter.guard();
        meter.charge(COSTS.rowProduced);
        out.push(plan.projections.map((p) => evalExpr(p, row, env, meter)));
      }
      return {
        columnNames: plan.columnNames,
        columnTypes: plan.columnTypes,
        rows: out,
        cost: meter.accrued,
      };
    }

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
      return {
        columnNames: plan.columnNames,
        columnTypes: plan.columnTypes,
        rows: out,
        cost: meter.accrued,
      };
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
    return new Sorter(compare, this.session.workMem, this.spillSink);
  }

  // materializeRel materializes one FROM relation ri into its rows, given the current outer-row stack
  // `outer` (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
  // outer); an SRF is generated; a CTE / derived table is delivered / run in place. For a CORRELATED
  // LATERAL relation (§44) the caller passes outer EXTENDED with the combined left-hand row, so the
  // body / SRF args read that row as their immediate outer; a non-lateral relation is passed the
  // query's own outer and its parent=null body simply ignores it (a parent=null plan holds no
  // outerColumn, so the two are observably identical).
  private materializeRel(
    plan: SelectPlan,
    ri: number,
    outer: Row[],
    baseEnv: EvalEnv,
    params: Value[],
    meter: Meter,
  ): Row[] {
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
      // Only a plain (query) CTE is ever inlined; a data-modifying CTE is always materialized
      // (writable-cte.md §3), so its buffer was filled above.
      const src = env.ctes.bindings[ci]!.source;
      if (src.kind !== "query") {
        throw new Error("a data-modifying CTE is always materialized, never inlined");
      }
      const r = this.execQueryPlan(src.plan, outer, params, env.ctes);
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
    const store = this.lkpStore(rel.tableName);
    let rows: Row[];
    let nodeCount: number;
    let slabs = 0;
    const relBound = plan.relBounds[ri]!;
    if (relBound !== null && relBound.kind === "index") {
      const r = this.indexBoundRows(
        rel.tableName,
        relBound.index,
        params,
        outer,
        plan.relMasks[ri]!,
      );
      rows = r.rows;
      nodeCount = r.pages;
      slabs = r.slabs;
    } else if (relBound !== null && relBound.kind === "gin") {
      // Re-find the constant query Q in the WHERE filter (the same conjunct plan-time ginMatch
      // chose — gin.md §6); the @>/&& predicate also stays the residual filter downstream.
      const m = plan.filter !== null ? ginMatch(plan.filter, relBound.gin.colGlobal) : null;
      const r = this.ginBoundRows(
        rel.tableName,
        relBound.gin,
        m?.query ?? null,
        env,
        meter,
        plan.relMasks[ri]!,
      );
      // SELECT discards the storage keys (UPDATE/DELETE keep them — gin.md §6).
      rows = r.entries.map((e) => e.row);
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

  private execSelectPlan(
    plan: SelectPlan,
    outer: Row[],
    params: Value[],
    ctes: CteCtx,
  ): SelectResult {
    const env: EvalEnv = {
      params,
      outer,
      // A subquery inherits the same CTE context (a CTE reference works at any nesting depth —
      // cte.md §2/§5).
      runSubquery: (p, o) => this.execQueryPlan(p, o, params, ctes),
      seam: this.session.seam,
      rng: new StmtRng(),
      ctes,
      exec: this,
    };
    const meter = this.session.newMeter();

    // Streaming primary-key-ordered scan (spec/design/cost.md §3): a single-table query with no
    // blocking operator beyond an ORDER BY the scan already satisfies — either no ORDER BY with a
    // LIMIT (the LIMIT short-circuit), or an ORDER BY satisfied by the table's primary-key scan order
    // (plan.pkOrdered) — streams scan→filter→project with NO sort, and with a LIMIT STOPS the scan
    // once the window is filled, so storageRowRead counts only the rows actually read. A non-PK-ordered
    // ORDER BY, DISTINCT, aggregate, or join must see every row, so it keeps the sort/eager path below.
    // pageRead stays the full block; only row reads short-circuit.
    if (
      plan.rels.length === 1 &&
      plan.joins.length === 0 &&
      !plan.isAgg &&
      !plan.hasWindow &&
      !plan.distinct &&
      (plan.pkOrdered || (plan.order.length === 0 && plan.limit !== null)) &&
      // An index- or GIN-bounded scan does not stream (cost.md §3 "index-bounded scan",
      // gin.md §6): it reads the full admitted set via the eager path below.
      plan.relBounds[0]?.kind !== "index" &&
      plan.relBounds[0]?.kind !== "gin" &&
      // A set-returning relation is generated, not scanned — it takes the eager path
      // (functions.md §10); the streaming reader assumes a table store.
      plan.rels[0]!.srf === undefined &&
      // A CTE reference is a computed/buffered source, not a table store — the eager path
      // (cte.md §5) delivers its rows; the streaming reader assumes a store.
      plan.rels[0]!.cte === undefined &&
      // A derived table is a computed source too (grammar.md §42) — eager path.
      plan.rels[0]!.derived === undefined
    ) {
      return this.execStreamingScan(plan, env, meter, params);
    }

    // Streaming external sort (spec/design/spill.md §5): a single-table, no-join, non-aggregate,
    // non-DISTINCT query with an ORDER BY the scan does NOT already satisfy (!plan.pkOrdered — caught
    // above) streams scan→filter→sorter, so the input is never materialized in the executor heap and
    // the sort spills sorted runs to disk under workMem (file-backed databases). DISTINCT/aggregate/
    // join take the eager path below, and an index bound does not stream (like the LIMIT
    // short-circuit). Results + cost are identical to the eager sort (the sort is unmetered —
    // cost.md §3; spill.md §6).
    if (
      plan.order.length > 0 &&
      !plan.pkOrdered &&
      plan.rels.length === 1 &&
      plan.joins.length === 0 &&
      !plan.isAgg &&
      !plan.hasWindow &&
      !plan.distinct &&
      plan.relBounds[0]?.kind !== "index" &&
      plan.relBounds[0]?.kind !== "gin" &&
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

    // WINDOW stage (spec/design/window.md §5.2): a blocking operator over the post-WHERE rows,
    // running BEFORE the query ORDER BY / DISTINCT / LIMIT. Each window function's per-row result is
    // APPENDED to its row (so the projection reads result i at flat slot inputWidth + i), the rows
    // keep their scan order, and the query ORDER BY below re-sorts the extended rows. A window query
    // never enters the streaming fast-paths above.
    if (plan.hasWindow) {
      applyWindowStage(rows, plan.windowSpecs, meter);
    }

    // ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
    // and a full tie keeps the scan order (JS Array#sort is stable). Each key's NULL placement
    // is decoupled from its value-direction flip (spec/design/grammar.md §10). Aggregate queries
    // sort their GROUP rows in the aggregate branch below — not these pre-aggregation rows — so
    // this is gated to plain queries.
    if (!plan.isAgg && plan.order.length > 0) {
      sortRows(rows, plan.order);
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
        sortRows(groupRows, plan.order);
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
    return {
      columnNames: plan.columnNames,
      columnTypes: plan.columnTypes,
      rows: out,
      cost: meter.accrued,
    };
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

  private foldUncorrelatedInPlan(
    plan: QueryPlan,
    bound: Value[],
    ctes: CteCtx,
    cost: { value: bigint },
  ): void {
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
    if (plan.kind === "with") {
      // A nested WITH body is not folded here against the enclosing ctes — its inner subqueries
      // reference the nested CTEs (a different scope, materialized only when the node runs), so they
      // are left to the evaluator, exactly like a derived table's body (spec/design/cte.md §7). The
      // whole nested-WITH subquery is itself folded by the caller if uncorrelated (executed once via
      // execWithPlan).
      return;
    }
    this.foldUncorrelatedInPlan(plan.lhs, bound, ctes, cost);
    this.foldUncorrelatedInPlan(plan.rhs, bound, ctes, cost);
  }

  private foldUncorrelatedInSelect(
    sp: SelectPlan,
    bound: Value[],
    ctes: CteCtx,
    cost: { value: bigint },
  ): void {
    for (const j of sp.joins)
      if (j.on !== null) j.on = this.foldUncorrelatedInRExpr(j.on, bound, ctes, cost);
    if (sp.filter !== null) sp.filter = this.foldUncorrelatedInRExpr(sp.filter, bound, ctes, cost);
    if (sp.having !== null) sp.having = this.foldUncorrelatedInRExpr(sp.having, bound, ctes, cost);
    for (const s of sp.aggSpecs) {
      if (s.operand !== null)
        s.operand = this.foldUncorrelatedInRExpr(s.operand, bound, ctes, cost);
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
  private foldUncorrelatedInRExpr(
    e: RExpr,
    bound: Value[],
    ctes: CteCtx,
    cost: { value: bigint },
  ): RExpr {
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
      case "regex":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost);
        e.rhs = this.foldUncorrelatedInRExpr(e.rhs, bound, ctes, cost);
        return e;
      case "casing":
        e.arg = this.foldUncorrelatedInRExpr(e.arg, bound, ctes, cost);
        return e;
      case "atTimeZone":
        e.zone = this.foldUncorrelatedInRExpr(e.zone, bound, ctes, cost);
        e.value = this.foldUncorrelatedInRExpr(e.value, bound, ctes, cost);
        return e;
      case "dateTrunc":
        e.unit = this.foldUncorrelatedInRExpr(e.unit, bound, ctes, cost);
        e.value = this.foldUncorrelatedInRExpr(e.value, bound, ctes, cost);
        if (e.zone) e.zone = this.foldUncorrelatedInRExpr(e.zone, bound, ctes, cost);
        return e;
      case "extract":
        e.value = this.foldUncorrelatedInRExpr(e.value, bound, ctes, cost);
        return e;
      case "dateConvert":
        e.inner = this.foldUncorrelatedInRExpr(e.inner, bound, ctes, cost);
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
      case "regexFunc":
      case "rangeFunc":
      case "rangeCtor":
      case "rangeOp":
      case "rangeSetOp":
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
                lower:
                  s.lower === null
                    ? null
                    : this.foldUncorrelatedInRExpr(s.lower, bound, ctes, cost),
                upper:
                  s.upper === null
                    ? null
                    : this.foldUncorrelatedInRExpr(s.upper, bound, ctes, cost),
              }
            : {
                isSlice: false,
                index: this.foldUncorrelatedInRExpr(s.index, bound, ctes, cost),
              },
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

// ScanBound is a per-relation scan bound (cost.md §3): a primary-key range, a
// secondary-index equality (spec/design/indexes.md §5), or a GIN-bounded scan over an array
// column (spec/design/gin.md §6). The PK bound wins when several apply (it is the row's own
// key — no second tree, range-capable, strictly cheaper); the ordered-index equality bound
// wins over GIN (gin.md §6).
type ScanBound =
  | { kind: "pk"; pk: PkBound }
  | { kind: "index"; index: IndexBound }
  | { kind: "gin"; gin: GinBound };

// GinStrategy is which array operator a GIN bound accelerates (spec/design/gin.md §6): @>
// (contains, mode ALL → posting-list intersection), && (overlaps, mode ANY → union), = ANY
// ("member" — `c = ANY(col)`, the single-term @> reduction: one scalar term, its lone posting
// list), or array = ("equal" — `col = Q`, the @>-superset gather + residual =). For "member" the
// query operand recovered by ginMatch is the scalar c, not an array; for "equal" it is the array Q
// whose distinct non-NULL elements gather the same superset as `@> Q` (equal arrays have identical
// element multisets, so col = Q ⟹ col @> Q), made exact by the residual = filter.
type GinStrategy = "contains" | "overlaps" | "member" | "equal";

// GinBound is the plan-time result of GIN analysis (spec/design/gin.md §6): the chosen GIN
// index (lowest lowercased name whose array column has a `col @> const` / `col && const`
// conjunct), the array ELEMENT type (for encode(term) — the term bytes), the operator
// strategy, and the column's global scope index. The constant query Q is NOT stored; it is
// re-found in plan.filter at exec time by ginMatch and evaluated there.
type GinBound = {
  nameKey: string;
  elemType: ScalarType;
  strategy: GinStrategy;
  colGlobal: number;
};

// IndexBound is the plan-time result of index analysis (indexes.md §5): the chosen index
// (lowest lowercased name whose FIRST key column has an equality conjunct), that column's
// storage type, and every equality const-source on it. At exec time the sources must
// agree on one value (else the bound is provably empty) and the index is range-scanned
// over that value's presence-tagged prefix.
// tailTypes is the REMAINING key components' types (columns[1..]): an admitted entry's
// row-key suffix sits after every component slot, so the fetch skips these (each slot is
// self-delimiting — a 0x01 NULL tag alone, or 0x00 + the type's fixed width).
type IndexBound = {
  nameKey: string;
  colType: ScalarType;
  eqs: RExpr[];
  // coll is the leading key column's resolved collation when it is collated AND Full — the equality
  // probe encodes via its UCA sort key (encoding.md §2.12) to match the index's stored key form
  // (spec/design/collation.md §8). null for a C (raw-byte) column. A Skewed collated index never
  // produces an IndexBound (keyCollationCtx refuses it — collation.md §12).
  coll: Collation | null;
  tailTypes: ScalarType[];
};

// detectScanBound picks one relation's scan bound (cost.md §3; indexes.md §5): the
// single-column PK bound first; else, among the relation's indexes (held in ascending
// lowercased-name order — the deterministic tie-break), the first whose FIRST key column
// has at least one equality conjunct against a type-matched const-source; else null
// (full scan).
function detectScanBound(filter: RExpr, rel: ScopeRel, snap: Snapshot): ScanBound | null {
  const pkLocal = primaryKeyIndex(rel.table);
  if (pkLocal >= 0) {
    // Ordered-equality pushdown is scalar-only; a non-scalar (range) PK skips it (point-lookup
    // deferred for containers — ranges.md §10), falling through to a full scan + residual filter.
    const sty = typeAsScalar(rel.table.columns[pkLocal]!.type);
    if (sty !== undefined) {
      // The PK column's key collation form (collation.md §8/§12): null ⇒ collated but Skewed ⇒
      // refuse pushdown (full heap-scan recompute — the read-safety rule §12); else { coll } where
      // coll is null (C, raw-byte key) or the Full-collated table (push via the sort key).
      const ctx = keyCollationCtx(snap, rel.table.columns[pkLocal]!);
      if (ctx !== null) {
        const bp = detectPkBound(filter, rel.offset + pkLocal, sty, ctx.coll);
        if (bp !== null) return { kind: "pk", pk: bp };
      }
    }
  }
  for (const idx of rel.table.indexes) {
    // A GIN index is not an ordered-equality bound — its array column is keyed by terms, not the
    // whole value (handled by the GIN pass below, gin.md §6).
    if (idx.kind === "gin") continue;
    const ci = idx.columns[0]!;
    // An ordered index whose leading (or any) key column is a non-scalar (range) does not pushdown —
    // point-lookup is deferred for containers (ranges.md §10); the index is still maintained.
    const ty = typeAsScalar(rel.table.columns[ci]!.type);
    if (ty === undefined) continue;
    // The tail-slot skip in indexBoundRows advances over each trailing key component by its FIXED
    // width (widthBytes), which exists only for the fixed-width scalars. A tail column that is
    // non-scalar (range/array/composite) OR a variable-width scalar (text/decimal/bytea/interval)
    // has no fixed width, so the index cannot pushdown: fall through to the full scan + residual
    // filter (rows identical, just no index bound).
    if (
      idx.columns.slice(1).some((c) => {
        const ts = typeAsScalar(rel.table.columns[c]!.type);
        return ts === undefined || !isFixedWidth(ts);
      })
    ) {
      continue;
    }
    // The leading column's key collation form (as for the PK above). A Skewed collated index is
    // skipped (ctx === null) — its stored keys are at the file's pinned version, wrong for the
    // loaded one, so it must not be seeked (collation.md §12; the tripwire suites/collation/skew.test).
    const ctx = keyCollationCtx(snap, rel.table.columns[ci]!);
    if (ctx === null) continue;
    const bp = detectPkBound(filter, rel.offset + ci, ty, ctx.coll);
    const eqs: RExpr[] = [];
    if (bp !== null) {
      for (const t of bp.terms) if (t.op === "eq") eqs.push(t.src);
    }
    if (eqs.length > 0) {
      const tailTypes = idx.columns.slice(1).map((c) => typeScalar(rel.table.columns[c]!.type));
      return {
        kind: "index",
        index: { nameKey: idx.name.toLowerCase(), colType: ty, eqs, coll: ctx.coll, tailTypes },
      };
    }
  }
  // GIN bound (gin.md §6) — after the PK and ordered-index equality bounds.
  const gb = detectGinBound(filter, rel.table.indexes, rel.table.columns, rel.offset);
  return gb !== null ? { kind: "gin", gin: gb } : null;
}

// keyCollationCtx reports the collation a key over col is STORED under, deciding whether — and how —
// a comparison bound may push down to that key (spec/design/collation.md §8/§12). Three outcomes:
//   - { coll: null }  — col is C (or non-text): the key is raw bytes (encoding.md §2.4), always
//     pushable, the unchanged fast path.
//   - { coll }        — col is collated and the collation is Full (its file pin matches the loaded
//     bundle): the key is the UCA sort key (encoding.md §2.12), pushable using coll to encode the
//     probe in the same form.
//   - null            — col is collated but Skewed (its file pin differs from the loaded bundle):
//     push is REFUSED. The scan stays a full heap-scan that recomputes against the LOADED table (the
//     read-safety rule §12; seeking a loaded-version probe in a file-version B-tree would mis-match —
//     the tripwire suites/collation/skew.test stays green only because this refuses). An unresolvable
//     collation likewise refuses rather than mis-encoding.
function keyCollationCtx(snap: Snapshot, col: Column): { coll: Collation | null } | null {
  if (col.collation === null) return { coll: null };
  if (snap.collationSkew(col.collation) !== undefined) return null;
  const c = snap.resolveCollation(col.collation);
  return c !== undefined ? { coll: c } : null;
}

// orderSatisfiedByPK reports whether a single base relation's ORDER BY is satisfied BY ITS
// PRIMARY-KEY scan order (spec/design/cost.md §3 "ORDER BY satisfied by primary-key order") — the
// table tree, walked forward in storage-key order, already delivers rows in the requested order, so
// the sort is a no-op. True iff the ORDER BY keys are a PREFIX of the PK columns (in key order),
// each ASC (a DESC reverse scan is a follow-on) and sorting by the SAME order the stored PK key
// realizes (collation.md §8/§12). The PK columns are NOT NULL, so a key's NULLS FIRST|LAST is a
// no-op (no NULLs to place) and is ignored. An ORDER BY shorter than the PK is a prefix (ties broken
// by the remaining PK columns — the canonical tie-break the eager stable sort produces); an ORDER BY
// longer than the PK matches the whole PK and its extra keys are redundant (the PK is unique).
function orderSatisfiedByPK(
  snap: Snapshot,
  table: Table,
  offset: number,
  order: OrderSlot[],
): boolean {
  const pk = pkIndices(table);
  if (pk.length === 0) return false; // no PK (synthetic rowid order is not a user-visible column)
  const m = Math.min(order.length, pk.length);
  for (let i = 0; i < m; i++) {
    const o = order[i]!;
    if (o.descending) return false; // ASC only this slice (a DESC reverse scan is a follow-on)
    if (o.idx !== offset + pk[i]!) return false; // must be the i-th PK column, in key order
    // The ORDER BY key must sort by the SAME order the stored PK key realizes. A raw-byte
    // (C/non-text) key matches a key with no collation; a Full-collated key matches the SAME
    // collation; a Skewed/unresolvable collation never matches (its stored keys are at the file's
    // pinned version, so the scan order would be wrong for the loaded one — §12).
    const ctx = keyCollationCtx(snap, table.columns[pk[i]!]!);
    if (ctx === null) return false; // Skewed / unresolvable
    if (ctx.coll === null) {
      if (o.collation !== null) return false; // raw-byte key, but the ORDER BY key carries a collation
    } else if (o.collation === null || o.collation.name !== ctx.coll.name) {
      return false;
    }
  }
  return true;
}

// detectGinBound detects a GIN-bounded scan over columns/indexes (gin.md §6): the lowest-named GIN
// index whose array column at offset+ci has a GIN-accelerable conjunct (`col @> const`,
// `col && const`, `const = ANY(col)`, or `col = const`). Factored out so the SELECT planner
// (detectScanBound) and the UPDATE/DELETE scan both use the identical detection — the mutations
// pass their own table's indexes/columns at offset 0.
function detectGinBound(
  filter: RExpr | null,
  indexes: IndexDef[],
  columns: Column[],
  offset: number,
): GinBound | null {
  if (filter === null) return null;
  for (const idx of indexes) {
    if (idx.kind !== "gin") continue;
    const ci = idx.columns[0]!;
    const colGlobal = offset + ci;
    const colType = columns[ci]!.type;
    if (colType.kind !== "array") continue; // a GIN column is always an array (the gate); defensive
    const m = ginMatch(filter, colGlobal);
    if (m !== null) {
      return {
        nameKey: idx.name.toLowerCase(),
        elemType: typeScalar(colType.elem),
        strategy: m.strategy,
        colGlobal,
      };
    }
  }
  return null;
}

// ginMatch finds the first WHERE AND-chain conjunct a GIN index on colGlobal accelerates
// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
// (membership), or `col = Q` (exact array equality) where the query operand is a constant
// (references no column / outer / subquery). @> is asymmetric (the indexed column must be the LEFT
// operand — `Q @> col` is the non-accelerated <@); && and array = are symmetric; = ANY requires the
// column be ANY's array operand and c the scalar. Returns the strategy and the constant query
// operand (the scalar c for "member", the array Q otherwise). Used at plan time (strategy) and exec
// time (recover the operand from plan.filter), so the two agree on the same conjunct by construction.
function ginMatch(
  filter: RExpr,
  colGlobal: number,
): { strategy: GinStrategy; query: RExpr } | null {
  if (filter.kind === "and") {
    return ginMatch(filter.lhs, colGlobal) ?? ginMatch(filter.rhs, colGlobal);
  }
  if (filter.kind === "arrayFunc" && filter.args.length === 2) {
    const a = filter.args[0]!;
    const b = filter.args[1]!;
    if (filter.func === "contains") {
      if (isColumnRef(a, colGlobal) && rexprIsConstant(b))
        return { strategy: "contains", query: b };
    } else if (filter.func === "overlaps") {
      if (isColumnRef(a, colGlobal) && rexprIsConstant(b))
        return { strategy: "overlaps", query: b };
      if (isColumnRef(b, colGlobal) && rexprIsConstant(a))
        return { strategy: "overlaps", query: a };
    }
  }
  // `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either operand, the
  // constant array Q the other. Recovered operand is Q; ginBoundRows reads it via "equal" (the
  // @>-superset gather + the residual =). <> is NOT matched (only "eq"). When the column is an array,
  // the other constant operand is necessarily an array too (resolve rejects an array/scalar =).
  if (filter.kind === "compare" && filter.op === "eq") {
    if (isColumnRef(filter.lhs, colGlobal) && rexprIsConstant(filter.rhs))
      return { strategy: "equal", query: filter.rhs };
    if (isColumnRef(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs))
      return { strategy: "equal", query: filter.lhs };
  }
  // `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be ANY's
  // ARRAY operand and c (the scalar lhs) a constant. Only = ANY (not = ALL, not any other
  // comparison/quantifier — those are not a single-term posting gather). The recovered query operand
  // is the scalar c; ginBoundRows reads it via "member".
  if (
    filter.kind === "quantified" &&
    filter.op === "eq" &&
    !filter.all &&
    isColumnRef(filter.array, colGlobal) &&
    rexprIsConstant(filter.lhs)
  ) {
    return { strategy: "member", query: filter.lhs };
  }
  return null;
}

// isColumnRef reports whether e is a reference to the column at global scope index colGlobal.
function isColumnRef(e: RExpr, colGlobal: number): boolean {
  return e.kind === "column" && e.index === colGlobal;
}

// rexprIsConstant reports whether e is evaluable without a current/outer row (so its value is the
// same for every scanned row — computable once). False for any column, correlated outer column, or
// subquery; true for literals, params, and pure operations over them. Used to admit a GIN query
// operand Q (spec/design/gin.md §6: a constant query only this slice).
function rexprIsConstant(e: RExpr): boolean {
  switch (e.kind) {
    case "column":
    case "outerColumn":
    case "subquery":
      return false;
    case "row":
      return e.fields.every(rexprIsConstant);
    case "array":
      return e.elements.every(rexprIsConstant);
    case "field":
      return rexprIsConstant(e.base);
    case "cast":
    case "neg":
    case "not":
    case "isNull":
      return rexprIsConstant(e.operand);
    case "arith":
    case "compare":
    case "and":
    case "or":
    case "distinct":
    case "like":
    case "regex":
      return rexprIsConstant(e.lhs) && rexprIsConstant(e.rhs);
    case "casing":
      return rexprIsConstant(e.arg);
    case "atTimeZone":
      return rexprIsConstant(e.zone) && rexprIsConstant(e.value);
    case "dateTrunc":
      return (
        rexprIsConstant(e.unit) &&
        rexprIsConstant(e.value) &&
        (e.zone === null || rexprIsConstant(e.zone))
      );
    case "extract":
      return rexprIsConstant(e.value);
    case "dateConvert":
      return rexprIsConstant(e.inner);
    case "case":
      return (
        e.arms.every((a) => rexprIsConstant(a.cond) && rexprIsConstant(a.result)) &&
        rexprIsConstant(e.els)
      );
    case "scalarFunc":
    case "arrayFunc":
    case "regexFunc":
    case "rangeFunc":
    case "rangeCtor":
    case "rangeOp":
    case "rangeSetOp":
    case "variadic":
      return e.args.every(rexprIsConstant);
    case "inValues":
      return rexprIsConstant(e.lhs);
    case "quantified":
      return rexprIsConstant(e.lhs) && rexprIsConstant(e.array);
    default:
      // Every leaf constant (constInt/constArray/param/…) is constant; a subscript / any other node
      // is treated conservatively as non-constant (no GIN accel; the residual filter still applies).
      return e.kind === "param" || e.kind.startsWith("const");
  }
}

// indexEntryKey builds a secondary-index entry key (spec/design/indexes.md §3): each
// indexed column as the encoding.md §2.2 nullable slot — 0x00 + the type's bare
// order-preserving key bytes when present, the lone 0x01 for NULL (always tagged, even
// for a NOT NULL column) — then the row's storage key as the suffix. Indexable types are
// fixed-width and never spill, so the values are always resident (never unfetched).
function indexEntryKey(
  columns: Column[],
  colls: (Collation | null)[],
  def: IndexDef,
  storageKey: Uint8Array,
  row: Row,
): Uint8Array {
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
    } else if (v.kind === "text") {
      // text: C terminated-escape (§2.4) or the collated UCA sort key (§2.12).
      parts.push(Uint8Array.of(0x00), collatedTextKey(colls[ci]!, v.text));
    } else if (v.kind === "bytea") {
      parts.push(Uint8Array.of(0x00), encodeTerminated(v.bytes));
    } else if (v.kind === "decimal") {
      parts.push(Uint8Array.of(0x00), v.dec.encodeKey());
    } else if (v.kind === "interval") {
      parts.push(Uint8Array.of(0x00), intervalEncodeKey(v.iv));
    } else if (v.kind === "range") {
      // the recursive range-bounds container key (encoding.md §2.11)
      parts.push(Uint8Array.of(0x00), encodeTypedKey(columns[ci]!.type, v, null));
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
function indexEntryKeys(
  columns: Column[],
  colls: (Collation | null)[],
  def: IndexDef,
  storageKey: Uint8Array,
  row: Row,
): Uint8Array[] {
  if (def.kind === "gin") return ginEntries(columns, def, storageKey, row);
  return [indexEntryKey(columns, colls, def, storageKey, row)];
}

// ginEntries builds a GIN index's entry keys for one row (spec/design/gin.md §4): one entry per
// DISTINCT non-NULL array element — encode(element) ‖ storage_key, NO presence tag (a term is never
// NULL) and an empty payload. A NULL array column value and an empty array yield no entries (so
// they appear in no posting list). Returned sorted by term (= encoded-byte order for the integer
// element types). This slice: a single integer-element array column.
// isGinElementType reports whether elem is an element type a GIN (array_ops) index admits — the
// integers, boolean, uuid, date, timestamp, timestamptz (spec/design/gin.md §3): a GIN term IS the
// element's order-preserving key encoding (§4) and a term carries no length/terminator framing, so
// only the FIXED-WIDTH keyables qualify. The variable-width keyables (text, bytea, decimal) — valid
// ordered-index / PK keys — are 0A000 here, as is float. interval is fixed-width keyable (its 16-byte
// span key landed, encoding.md §2.10) but its GIN element support is a separate follow-on slice
// (gin.md §3/§10), so it is not yet admitted here.
function isGinElementType(elem: Type): boolean {
  return (
    typeIsInteger(elem) ||
    typeIsBoolean(elem) ||
    typeIsUuid(elem) ||
    typeIsTimestamp(elem) ||
    typeIsTimestamptz(elem) ||
    typeIsDate(elem)
  );
}

function ginEntries(
  columns: Column[],
  def: IndexDef,
  storageKey: Uint8Array,
  row: Row,
): Uint8Array[] {
  const ci = def.columns[0]!;
  const colType = columns[ci]!.type;
  if (colType.kind !== "array")
    throw new Error("a GIN index column is an array (CREATE INDEX gate)");
  const elemTy = typeScalar(colType.elem);
  const v = row[ci]!;
  if (v.kind !== "array") return [];
  // Dedup by the encoded term (the encoding is a bijection: byte-dedup == value-dedup, byte-sort ==
  // value-sort) generically over every admitted element type.
  const seen = new Set<string>();
  const terms: Uint8Array[] = [];
  for (const el of v.elements) {
    if (el.kind === "null") continue; // a NULL element carries no term; a non-keyable element is impossible
    // a GIN element is fixed-width (isGinElementType excludes text), so it never collates.
    const term = encodeKeyValue(elemTy, el, null);
    const k = byteKey(term);
    if (!seen.has(k)) {
      seen.add(k);
      terms.push(term);
    }
  }
  terms.sort(cmpBytes);
  return terms.map((term) => {
    const out = new Uint8Array(term.length + storageKey.length);
    out.set(term, 0);
    out.set(storageKey, term.length);
    return out;
  });
}

// cmpBytes compares two byte strings lexicographically (-1/0/1), the on-disk storage-key order.
function cmpBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    if (a[i]! !== b[i]!) return a[i]! - b[i]!;
  }
  return a.length - b.length;
}

// byteKey is a stable string key for a (small) byte string — used to dedup / count storage keys in
// the GIN posting combine. Storage keys are short (an encoded PK or rowid), so per-char is fine.
function byteKey(a: Uint8Array): string {
  let s = "";
  for (let i = 0; i < a.length; i++) s += String.fromCharCode(a[i]!);
  return s;
}

// intersectPostings returns the storage keys present in EVERY posting list (the @> mode-ALL
// combine), sorted ascending. Each posting list holds distinct keys (one (term,row) entry per row),
// so a per-list count == the number of lists means the key is in all of them.
function intersectPostings(postings: Uint8Array[][]): Uint8Array[] {
  if (postings.length === 0) return [];
  const count = new Map<string, number>();
  for (const list of postings) {
    for (const k of list) {
      const key = byteKey(k);
      count.set(key, (count.get(key) ?? 0) + 1);
    }
  }
  const need = postings.length;
  const out: Uint8Array[] = [];
  for (const k of postings[0]!) {
    if (count.get(byteKey(k)) === need) out.push(k);
  }
  out.sort(cmpBytes);
  return out;
}

// unionPostings returns the storage keys present in ANY posting list (the && mode-ANY combine),
// deduplicated and sorted ascending.
function unionPostings(postings: Uint8Array[][]): Uint8Array[] {
  const seen = new Set<string>();
  const out: Uint8Array[] = [];
  for (const list of postings) {
    for (const k of list) {
      const key = byteKey(k);
      if (!seen.has(key)) {
        seen.add(key);
        out.push(k);
      }
    }
  }
  out.sort(cmpBytes);
  return out;
}

// bytesDiff returns the entries in a that are not in b (set difference over byte strings),
// preserving a's order — the UPDATE symmetric-difference for GIN / B-tree maintenance (gin.md §5).
function bytesDiff(a: Uint8Array[], b: Uint8Array[]): Uint8Array[] {
  return a.filter((x) => !b.some((y) => bytesEq(x, y)));
}

// encodePkKey is a row's PRIMARY-KEY STORAGE KEY (spec/design/encoding.md §2.3): the
// concatenation of the members' bare encodings in key order — every keyable type is fixed-width,
// so the concatenation is self-delimiting and byte comparison equals the tuple's logical order.
// Shared by the INSERT duplicate check and the ON CONFLICT arbiter probe (upsert.md §3); a PK
// column is NOT NULL, so there is no presence tag.
function encodePkKey(
  table: Table,
  pk: number[],
  colls: (Collation | null)[],
  row: Row,
): Uint8Array {
  const parts: Uint8Array[] = [];
  for (const i of pk) {
    const pkv = row[i]!; // non-null: a PK member is NOT NULL
    if (pkv.kind === "uuid") {
      parts.push(pkv.bytes.slice());
    } else if (pkv.kind === "bool") {
      parts.push(encodeBool(pkv.value));
    } else if (pkv.kind === "int") {
      parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.int));
    } else if (pkv.kind === "timestamp" || pkv.kind === "timestamptz") {
      parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.micros));
    } else if (pkv.kind === "date") {
      parts.push(encodeInt(typeScalar(table.columns[i]!.type), pkv.days));
    } else if (pkv.kind === "text") {
      // text: C terminated-escape (§2.4) or the collated UCA sort key (§2.12).
      parts.push(collatedTextKey(colls[i]!, pkv.text));
    } else if (pkv.kind === "bytea") {
      parts.push(encodeTerminated(pkv.bytes));
    } else if (pkv.kind === "decimal") {
      parts.push(pkv.dec.encodeKey());
    } else if (pkv.kind === "interval") {
      parts.push(intervalEncodeKey(pkv.iv));
    } else if (pkv.kind === "range") {
      // the recursive range-bounds container key (encoding.md §2.11, the first container key)
      parts.push(encodeTypedKey(table.columns[i]!.type, pkv, null));
    } else {
      throw engineError(
        "data_corrupted",
        "a primary key must be an integer, boolean, uuid, text, bytea, decimal, interval, range, or timestamp value",
      );
    }
  }
  const total = parts.reduce((acc, b) => acc + b.length, 0);
  const key = new Uint8Array(total);
  let off = 0;
  for (const b of parts) {
    key.set(b, off);
    off += b.length;
  }
  return key;
}

// Arbiter is which uniqueness constraint an ON CONFLICT arbitrates (spec/design/upsert.md §2):
// the primary key (isPK), or a unique index by position in table.indexes (indexPos).
type Arbiter = { isPK: true } | { isPK: false; indexPos: number };

// ConflictPlan is a resolved ON CONFLICT clause (spec/design/upsert.md), built by resolveOnConflict.
type ConflictPlan = {
  // arb is the arbiter constraint; null = no target (legal only with DO NOTHING — any uniqueness
  // conflict is then skipped).
  arb: Arbiter | null;
  doUpdate: boolean;
  assignments: AssignPlan[];
  filter: RExpr | null;
};

// resolveArbiter resolves an ON CONFLICT target into an Arbiter (spec/design/upsert.md §2): a
// column list is matched as an order-independent SET against a unique index / the primary key (no
// match → 42P10); ON CONSTRAINT name names a unique index or the synthesized <table>_pkey (miss →
// 42704). A null target → null arbiter (legal only with DO NOTHING).
function resolveArbiter(table: Table, target: ConflictTarget | null): Arbiter | null {
  if (target === null) return null;
  const pk = pkIndices(table);
  if (target.kind === "columns") {
    const want = new Set<number>();
    for (const c of target.columns) {
      const idx = columnIndex(table, c);
      if (idx < 0) throw engineError("undefined_column", "column does not exist: " + c);
      want.add(idx);
    }
    if (pk.length > 0 && sameIntSet(pk, want)) return { isPK: true };
    for (let i = 0; i < table.indexes.length; i++) {
      const def = table.indexes[i]!;
      if (def.unique && sameIntSet(def.columns, want)) return { isPK: false, indexPos: i };
    }
    throw engineError(
      "invalid_column_reference",
      "there is no unique or exclusion constraint matching the ON CONFLICT specification",
    );
  }
  const pkey = table.name.toLowerCase() + "_pkey";
  if (pk.length > 0 && target.name.toLowerCase() === pkey) return { isPK: true };
  for (let i = 0; i < table.indexes.length; i++) {
    const def = table.indexes[i]!;
    if (def.unique && def.name.toLowerCase() === target.name.toLowerCase())
      return { isPK: false, indexPos: i };
  }
  throw engineError(
    "undefined_object",
    `constraint ${target.name} for table ${table.name} does not exist`,
  );
}

// sameIntSet reports whether the array's values (as a set) equal the given set.
function sameIntSet(arr: number[], set: Set<number>): boolean {
  const seen = new Set(arr);
  if (seen.size !== set.size) return false;
  for (const v of seen) if (!set.has(v)) return false;
  return true;
}

// arbiterKey is the arbiter key of a candidate row (spec/design/upsert.md §3): the storage key for
// a PK arbiter (never NULL), or the unique-index prefix for an index arbiter (null when a nullable
// arbiter column is NULL — NULLS DISTINCT, so the row never conflicts).
function arbiterKey(
  arb: Arbiter,
  table: Table,
  pk: number[],
  colls: (Collation | null)[],
  row: Row,
): Uint8Array | null {
  if (arb.isPK) return encodePkKey(table, pk, colls, row);
  return indexPrefixKey(table.columns, colls, table.indexes[arb.indexPos]!, row);
}

// indexPrefixKey builds a row's UNIQUENESS PROBE KEY for one unique index
// (spec/design/indexes.md §8): the §3 entry key's slot prefix — without the storage-key
// suffix — or null when any component is NULL (NULLS DISTINCT: such a tuple never
// conflicts). Two rows conflict iff they yield the same non-null prefix.
function indexPrefixKey(
  columns: Column[],
  colls: (Collation | null)[],
  def: IndexDef,
  row: Row,
): Uint8Array | null {
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
    } else if (v.kind === "text") {
      // text: C terminated-escape (§2.4) or the collated UCA sort key (§2.12).
      parts.push(Uint8Array.of(0x00), collatedTextKey(colls[ci]!, v.text));
    } else if (v.kind === "bytea") {
      parts.push(Uint8Array.of(0x00), encodeTerminated(v.bytes));
    } else if (v.kind === "decimal") {
      parts.push(Uint8Array.of(0x00), v.dec.encodeKey());
    } else if (v.kind === "interval") {
      parts.push(Uint8Array.of(0x00), intervalEncodeKey(v.iv));
    } else if (v.kind === "range") {
      // the recursive range-bounds container key (encoding.md §2.11)
      parts.push(Uint8Array.of(0x00), encodeTypedKey(columns[ci]!.type, v, null));
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
// collatedTextKey is the order-preserving key body for a text value (encoding.md §2.12): the
// collation's UCA sort key when coll is non-null (a non-C collated column), else the C
// text-terminated-escape body (§2.4). The sort key throws (0A000) on a code point the collation does
// not map — propagated, so a collated INSERT of an unmapped string aborts the write.
function collatedTextKey(coll: Collation | null, s: string): Uint8Array {
  return coll !== null ? collationSortKey(coll, s) : encodeTerminated(SQL_BYTE_ENCODER.encode(s));
}

function encodeKeyValue(ty: ScalarType, value: Value, coll: Collation | null): Uint8Array {
  if (value.kind === "int") return encodeInt(ty, value.int);
  if (value.kind === "bool") return encodeBool(value.value);
  if (value.kind === "uuid") return value.bytes.slice();
  if (value.kind === "timestamp" || value.kind === "timestamptz")
    return encodeInt(ty, value.micros);
  if (value.kind === "date") return encodeInt(ty, value.days);
  if (value.kind === "text") return collatedTextKey(coll, value.text);
  if (value.kind === "bytea") return encodeTerminated(value.bytes);
  if (value.kind === "decimal") return value.dec.encodeKey();
  if (value.kind === "interval") return intervalEncodeKey(value.iv);
  throw new Error("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)");
}

// encodeTypedKey is the order-preserving key bytes for one keyable value given its column Type — the
// range-aware encoder threaded through every key path (PK, index entry/prefix, FK probe). A range
// recurses into the range-bounds container codec (encoding.md §2.11), pulling its element scalar from
// the column type; every other keyable value ignores the wrapper and dispatches on its scalar via
// encodeKeyValue. value is non-NULL (callers handle the NULL slot tag), and a range column always
// holds a range value, so the scalar arm never sees a range type. coll selects a text column's key
// form (§2.12); it never applies to a range element (no range subtype is text).
function encodeTypedKey(ty: Type, value: Value, coll: Collation | null): Uint8Array {
  if (value.kind === "range") {
    if (ty.kind !== "range") {
      throw new Error("a range key value has a range column type");
    }
    return encodeRangeKey(typeScalar(ty.elem), value);
  }
  return encodeKeyValue(typeScalar(ty), value, coll);
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
function fkProbe(
  fk: ForeignKey,
  parent: Table,
  parentColls: (Collation | null)[],
  row: Row,
  ordinals: number[],
): FkProbe | null {
  // MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
  for (const o of ordinals) {
    if (row[o]!.kind === "null") return null;
  }
  // The value supplying parent column `pcol` (the fk pairing: refColumns[i] ⇄ ordinals[i]).
  const valueFor = (pcol: number): Value => {
    const i = fk.refColumns.indexOf(pcol);
    return row[ordinals[i]!]!;
  };
  // The probe must match the PARENT's stored key, so a collated parent key column uses the PARENT's
  // collation (encoding.md §2.12), independent of the child column's own collation.
  const refSet = sortedUnique(fk.refColumns);
  const pkSet = sortedUnique(parent.pk);
  if (parent.pk.length > 0 && sameSet(pkSet, refSet)) {
    const parts: Uint8Array[] = [];
    for (const pcol of parent.pk) {
      parts.push(encodeTypedKey(parent.columns[pcol]!.type, valueFor(pcol), parentColls[pcol]!));
    }
    return { kind: "pk", bytes: concatBytes(parts) };
  }
  const idx = parent.indexes.find((i) => i.unique && sameSet(sortedUnique(i.columns), refSet))!;
  const parts: Uint8Array[] = [];
  for (const pcol of idx.columns) {
    parts.push(Uint8Array.of(0x00));
    parts.push(encodeTypedKey(parent.columns[pcol]!.type, valueFor(pcol), parentColls[pcol]!));
  }
  return {
    kind: "unique",
    index: idx.name.toLowerCase(),
    prefix: concatBytes(parts),
  };
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
  if (a.kind === "range" && b.kind === "range") return fkTypesEqual(a.elem, b.elem);
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
// coll is the key column's resolved collation when it is collated AND Full (loaded version matches
// the file pin) — the probe encodes via this collation's UCA sort key (encoding.md §2.12), seeking
// the same key FORM the B-tree stores (spec/design/collation.md §8). null for a C (raw-byte) key. A
// Skewed collated key never produces a PkBound (keyCollationCtx refuses the bound — collation.md §12).
type PkBound = { pkType: ScalarType; terms: BoundTerm[]; coll: Collation | null };

// BoundKey is the outcome of encoding a const-source into the PK key space: a usable key, a NULL const
// (the comparison is 3VL-unknown ⇒ empty range), or an out-of-range integer (drop this half-bound).
type BoundKey = { kind: "key"; key: Uint8Array } | { kind: "null" } | { kind: "outOfRange" };

// detectPkBound flattens the WHERE's top-level AND-chain (an OR is never descended — a disjunction is
// not a contiguous range) and collects every `pk <cmp> const-source` conjunct. null ⇒ full scan.
// Conservative + sound: an unrecognized conjunct contributes no bound and stays in the residual filter.
function detectPkBound(
  filter: RExpr,
  pkIdx: number,
  pkType: ScalarType,
  coll: Collation | null,
): PkBound | null {
  const colColl = coll !== null ? coll.name : null;
  const terms: BoundTerm[] = [];
  const walk = (e: RExpr): void => {
    if (e.kind === "and") {
      walk(e.lhs);
      walk(e.rhs);
      return;
    }
    const t = asBoundTerm(e, pkIdx, pkType, colColl);
    if (t !== null) terms.push(t);
  };
  walk(filter);
  return terms.length === 0 ? null : { pkType, terms, coll };
}

// asBoundTerm recognizes a single PK comparison conjunct: a comparison (=,<,<=,>,>=) with the bare LOCAL
// PK column ("column" at pkIdx — a correlated "outerColumn" is a different kind, so it never matches) on
// one side and a const-source of the PK's own type on the other (a promoted comparison — e.g. intpk = 2.5
// → a constDecimal — does not match, so it stays residual). The op is flipped when the PK is on the right.
function asBoundTerm(
  e: RExpr,
  pkIdx: number,
  pkType: ScalarType,
  colColl: string | null,
): BoundTerm | null {
  if (e.kind !== "compare") return null;
  // A comparison bounds the key only when ITS resolved collation matches the key column's frozen
  // collation (colColl) — so the comparison orders text the SAME way the B-tree is keyed
  // (spec/design/collation.md §8). C key ⇔ a C/byte comparison (both null); a collated key ⇔ a
  // comparison under the SAME collation (the column's implicit collation, or an explicit
  // COLLATE "<that name>"). A comparison under a DIFFERENT collation — name COLLATE "C" over a
  // unicode column, COLLATE "de" over unicode — does NOT match: its order disagrees with the stored
  // keys, so it stays a full scan + residual filter. (A *skewed* collated key never reaches here —
  // keyCollationCtx refuses the whole bound, §12.) The probe is then encoded in the key column's
  // form (sort key for a Full-collated column — buildKeyBound/indexBoundRows).
  const cmpColl = e.collation !== null ? e.collation.name : null;
  if (cmpColl !== colColl) return null;
  if (e.op !== "eq" && e.op !== "lt" && e.op !== "le" && e.op !== "gt" && e.op !== "ge")
    return null;
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
    case "constText":
      return isText(pkType);
    case "constBytea":
      return isBytea(pkType);
    case "constDecimal":
      return isDecimal(pkType);
    case "constInterval":
      return isInterval(pkType);
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
    const r = encodeBoundKey(bp.pkType, t.src, params, outer, bp.coll);
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
function encodeBoundKey(
  pkType: ScalarType,
  src: RExpr,
  params: Value[],
  outer: Row[],
  coll: Collation | null,
): BoundKey {
  switch (src.kind) {
    case "constNull":
      return { kind: "null" };
    case "constInt":
      return inRange(pkType, src.value)
        ? { kind: "key", key: encodeInt(pkType, src.value) }
        : { kind: "outOfRange" };
    case "constBool":
      return { kind: "key", key: encodeBool(src.value) };
    case "constUuid":
      return { kind: "key", key: src.value.slice() };
    case "constTimestamp":
    case "constTimestamptz":
    case "constDate":
      return { kind: "key", key: encodeInt(pkType, src.value) };
    case "constText":
      return encodeTextBound(src.value, coll);
    case "constBytea":
      return { kind: "key", key: encodeTerminated(src.value) };
    case "constDecimal":
      return { kind: "key", key: src.value.encodeKey() };
    case "constInterval":
      return { kind: "key", key: intervalEncodeKey(src.value) };
    case "param":
      return encodeValueKey(pkType, params[src.index]!, coll);
    case "outerColumn":
      // A correlated reference: column index of the enclosing row level hops out — the same indexing
      // the evaluator uses for "outerColumn" (innermost outer row is last).
      return encodeValueKey(pkType, outer[outer.length - src.level]![src.index]!, coll);
    default:
      return { kind: "outOfRange" };
  }
}

// encodeTextBound encodes a text probe into a key bound: the raw text-terminated-escape bytes for a C
// key (coll === null, the fast path, encoding.md §2.4), or the collation's UCA sort key
// (text-collated-sortkey, §2.12) for a Full-collated key. A sort-key build that fails on an unmapped
// code point (the 0A000 the write/compare path raises, collation.md §6) becomes outOfRange here: the
// probe matches no stored (always-mapped) key, so the term contributes no bound and the scan widens
// to a full scan + residual filter — which reproduces the exact non-pushdown answer (empty for =,
// since equality is byte-identity §7; the 0A000 for an ordering compare iff any row is scanned).
// Identical across cores (mirrors Rust encode_text_bound / Go encodeTextBound).
function encodeTextBound(s: string, coll: Collation | null): BoundKey {
  if (coll === null) return { kind: "key", key: encodeTerminated(SQL_BYTE_ENCODER.encode(s)) };
  try {
    return { kind: "key", key: collationSortKey(coll, s) };
  } catch {
    return { kind: "outOfRange" };
  }
}

// encodeValueKey encodes a runtime Value (a bound param or a resolved outer column) into the PK's storage
// key. A NULL value makes the comparison 3VL-unknown (an empty range); a value of a kind no key can hold
// (or an integer outside the PK width) drops its half-bound, widening — still sound. coll selects a text
// value's key form (collated sort key vs raw bytes — encodeTextBound).
function encodeValueKey(pkType: ScalarType, v: Value, coll: Collation | null): BoundKey {
  if (v.kind === "null") return { kind: "null" };
  if (v.kind === "bool") return { kind: "key", key: encodeBool(v.value) };
  if (v.kind === "uuid") return { kind: "key", key: v.bytes.slice() };
  if (v.kind === "text") return encodeTextBound(v.text, coll);
  if (v.kind === "bytea") return { kind: "key", key: encodeTerminated(v.bytes) };
  if (v.kind === "decimal") return { kind: "key", key: v.dec.encodeKey() };
  if (v.kind === "interval") return { kind: "key", key: intervalEncodeKey(v.iv) };
  if (v.kind === "int")
    return inRange(pkType, v.int)
      ? { kind: "key", key: encodeInt(pkType, v.int) }
      : { kind: "outOfRange" };
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
function mutationPkBound(table: Table, filter: RExpr | null, snap: Snapshot): PkBound | null {
  if (filter === null) return null;
  const pkIdx = primaryKeyIndex(table);
  if (pkIdx < 0) return null;
  // Point-lookup pushdown is scalar-only; a non-scalar (range) PK skips it (deferred — ranges.md
  // §10), so a range PK WHERE k = … full-scans + residual-filters.
  const sty = typeAsScalar(table.columns[pkIdx]!.type);
  if (sty === undefined) return null;
  // A collated Skewed PK refuses pushdown (ctx === null) — though a skewed table's write is already
  // refused XX002 upstream (ensureCollationsWritable), so this is reached only for a C or
  // Full-collated PK (collation.md §8/§12).
  const ctx = keyCollationCtx(snap, table.columns[pkIdx]!);
  if (ctx === null) return null;
  return detectPkBound(filter, pkIdx, sty, ctx.coll);
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
    for (const row of plan.rows)
      for (const e of row) if (rexprReferencesOuter(e, depth)) return true;
    return false;
  }
  if (plan.kind === "with") {
    // A nested WITH adds no correlation frame: its body is at the same depth, and the CTE bodies are
    // planned parent=null (no outer reference), so only the body can correlate (cte.md §7).
    return queryPlanReferencesOuter(plan.body, depth);
  }
  for (const j of plan.joins) if (j.on !== null && rexprReferencesOuter(j.on, depth)) return true;
  if (plan.filter !== null && rexprReferencesOuter(plan.filter, depth)) return true;
  if (plan.having !== null && rexprReferencesOuter(plan.having, depth)) return true;
  for (const s of plan.aggSpecs)
    if (s.operand !== null && rexprReferencesOuter(s.operand, depth)) return true;
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
    case "regex":
      return rexprReferencesOuter(e.lhs, depth) || rexprReferencesOuter(e.rhs, depth);
    case "casing":
      return rexprReferencesOuter(e.arg, depth);
    case "atTimeZone":
      return rexprReferencesOuter(e.zone, depth) || rexprReferencesOuter(e.value, depth);
    case "dateTrunc":
      return (
        rexprReferencesOuter(e.unit, depth) ||
        rexprReferencesOuter(e.value, depth) ||
        (e.zone !== null && rexprReferencesOuter(e.zone, depth))
      );
    case "extract":
      return rexprReferencesOuter(e.value, depth);
    case "dateConvert":
      return rexprReferencesOuter(e.inner, depth);
    case "case":
      return (
        e.arms.some(
          (arm) => rexprReferencesOuter(arm.cond, depth) || rexprReferencesOuter(arm.result, depth),
        ) || rexprReferencesOuter(e.els, depth)
      );
    case "scalarFunc":
    case "arrayFunc":
    case "regexFunc":
    case "rangeFunc":
    case "rangeCtor":
    case "rangeOp":
    case "rangeSetOp":
    case "variadic":
      return e.args.some((a) => rexprReferencesOuter(a, depth));
    case "row":
      return e.fields.some((f) => rexprReferencesOuter(f, depth));
    case "array":
      return e.elements.some((el) => rexprReferencesOuter(el, depth));
    case "field":
      return rexprReferencesOuter(e.base, depth);
    case "subscript":
      return (
        rexprReferencesOuter(e.base, depth) ||
        rSubscriptBounds(e.subscripts).some((b) => rexprReferencesOuter(b, depth))
      );
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
      // A Column index beyond the real columns is a SYNTHETIC slot (an aggregate or window result,
      // spec/design/window.md §5.1), not a table column — it touches no stored data, so the bound
      // check skips it rather than going out of range.
      if (depth === 0 && e.index < touched.length) touched[e.index] = true;
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
    case "regex":
      collectTouched(e.lhs, depth, touched);
      collectTouched(e.rhs, depth, touched);
      return;
    case "casing":
      collectTouched(e.arg, depth, touched);
      return;
    case "atTimeZone":
      collectTouched(e.zone, depth, touched);
      collectTouched(e.value, depth, touched);
      return;
    case "dateTrunc":
      collectTouched(e.unit, depth, touched);
      collectTouched(e.value, depth, touched);
      if (e.zone !== null) collectTouched(e.zone, depth, touched);
      return;
    case "extract":
      collectTouched(e.value, depth, touched);
      return;
    case "dateConvert":
      collectTouched(e.inner, depth, touched);
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
    case "regexFunc":
    case "rangeFunc":
    case "rangeCtor":
    case "rangeOp":
    case "rangeSetOp":
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
    for (const s of plan.aggSpecs)
      if (s.operand !== null) collectTouched(s.operand, depth, touched);
    for (const p of plan.projections) collectTouched(p, depth, touched);
  } else if (plan.kind === "values") {
    for (const row of plan.rows) for (const e of row) collectTouched(e, depth, touched);
  } else if (plan.kind === "with") {
    // A nested WITH's correlated references live in its body (the CTE bodies are parent=null);
    // recurse into the body at the same depth (spec/design/cte.md §7).
    collectTouchedPlan(plan.body, depth, touched);
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
    case "range":
      // A folded range constant (already canonical).
      return { kind: "constRange", value: v };
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
  switch (v.kind) {
    case "composite":
      // Length-prefix the field count and each field's key so a composite never collides with a
      // scalar key and nested composites stay unambiguous.
      return (
        "c" + v.fields.length.toString() + ":" + v.fields.map((f) => distinctValueKey(f)).join(",")
      );
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
    case "range":
      // A range keys structurally over its CANONICAL form (spec/design/ranges.md §6): the empty
      // flag, the inclusivity flags, and each bound's own key (an infinite/null bound keys as
      // 'n', like a NULL array element). Two equal canonical ranges have identical fields, so
      // this buckets exactly like the range_cmp equality. The 'r' tag + flag chars keep it
      // collision-free against scalar/array keys.
      if (v.empty) return "re";
      return (
        "r" +
        (v.lowerInc ? "[" : "(") +
        (v.upperInc ? "]" : ")") +
        (v.lower === null ? "n" : distinctValueKey(v.lower)) +
        "," +
        (v.upper === null ? "n" : distinctValueKey(v.upper))
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
  | {
      kind: "composite";
      name: string | null;
      fields: { name: string; type: ResolvedType }[];
    }
  // An array type (spec/design/array.md §2), carrying its resolved element type. Two arrays are
  // comparable iff their element types are comparable; assignable to an array column of the same
  // element type.
  | { kind: "array"; elem: ResolvedType }
  // A range type (spec/design/ranges.md §2), carrying its resolved element (subtype) type. Two
  // ranges are comparable iff their elements are equal; the element is one of the six scalar subtypes.
  | { kind: "range"; elem: ResolvedType }
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
  // A folded range constant ('[1,5)'::i32range, already canonicalized); evaluates to it directly.
  | { kind: "constRange"; value: Value }
  // Field selection `(composite).field` (spec/design/composite.md §S4): evaluate `base` to a
  // composite value and return its `index`-th field (the field ordinal, fixed at resolve). A
  // whole-value-NULL composite yields NULL for any field. One operator_eval per node.
  | { kind: "field"; base: RExpr; index: number }
  // Array subscript `base[..][..]` (spec/design/array.md §6): one or more subscript specs applied to
  // `base`. All-index access reads one element (NULL when the subscript count ≠ ndim or any index is
  // out of range); a slice (any spec a slice) returns a sub-array, with a scalar index i meaning 1:i.
  // A NULL array or any NULL bound yields NULL. One operator_eval per node.
  | {
      kind: "subscript";
      base: RExpr;
      subscripts: RSubscript[];
      isSlice: boolean;
    }
  | {
      kind: "cast";
      target: ScalarType;
      typmod: DecimalTypmod | null;
      operand: RExpr;
    }
  | { kind: "neg"; result: ScalarType; operand: RExpr }
  | { kind: "not"; operand: RExpr }
  | { kind: "arith"; op: BinaryOp; result: ScalarType; lhs: RExpr; rhs: RExpr }
  // The derived collation (spec/design/collation.md §7): null is the C / default byte order (the
  // unchanged fast path); a non-null collation orders the ORDERING comparisons (< <= > >=) by its UCA
  // sort key. =/<> stay byte-equality (deterministic-collation equality IS byte-identity), but it is
  // derived + conflict-checked (42P21) for every comparison op.
  | {
      kind: "compare";
      op: BinaryOp;
      lhs: RExpr;
      rhs: RExpr;
      collation: Collation | null;
    }
  | { kind: "and"; lhs: RExpr; rhs: RExpr }
  | { kind: "or"; lhs: RExpr; rhs: RExpr }
  | { kind: "isNull"; operand: RExpr; negated: boolean }
  | { kind: "distinct"; lhs: RExpr; rhs: RExpr; negated: boolean }
  | { kind: "like"; lhs: RExpr; rhs: RExpr; negated: boolean; insensitive: boolean }
  // `lhs ~ rhs` / `~*` / `!~` / `!~*` — regex match (regex.md), matched by the hand-written Pike VM
  // (regex.ts). `program` is the precompiled NFA for a CONSTANT pattern (compiled once at resolve,
  // the `col ~ 'literal'` case — regex.md §5); null means the pattern is non-constant (compiled per
  // row at eval). `compileCharged` is the one-shot flag charging a precompiled program's
  // regex_compile cost once per statement execution (on first eval), not per row.
  | {
      kind: "regex";
      lhs: RExpr;
      rhs: RExpr;
      negated: boolean;
      insensitive: boolean;
      program: RegexProgram | null;
      compileCharged: boolean;
    }
  // upper(text)/lower(text) — Unicode case folding (collation.md §16). upper selects the direction;
  // folds via the engine-global property table or the ASCII baseline. A NULL operand propagates.
  | { kind: "casing"; upper: boolean; arg: RExpr }
  | { kind: "atTimeZone"; zone: RExpr; value: RExpr; toTimestamptz: boolean }
  // date_trunc(unit, value[, zone]) (timezones.md §9.1): truncate value down to unit. For a
  // timestamptz value the truncation is in zone (3-arg) or the session zone (2-arg). The result
  // family is the value family.
  | { kind: "dateTrunc"; unit: RExpr; value: RExpr; zone: RExpr | null }
  // EXTRACT(field FROM value) (timezones.md §9.2): the numeric value of field (lowercased, validated
  // at resolve). For a timestamptz value every field but `epoch` is computed in the session zone.
  | { kind: "extract"; field: string; value: RExpr }
  // A cross-family datetime cast (timezones.md §9.3) to `to` (timestamp/timestamptz/date) from
  // another datetime family. The casts crossing the timestamptz boundary consult the session zone.
  | { kind: "dateConvert"; inner: RExpr; to: ScalarType }
  | {
      kind: "case";
      arms: { cond: RExpr; result: RExpr }[];
      els: RExpr;
      coerceDecimal: boolean;
    }
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
  // A polymorphic range-accessor call (spec/design/range-functions.md §1, RF1), evaluated per row.
  // Like "arrayFunc", it resolves over a pseudo-family (anyrange, binding ELEM := the element type)
  // and the result type lives in the surrounding ResolvedType (carried out of resolve), not on the
  // node; the kernel recovers everything from the operand range value (self-describing). All are
  // STRICT (a NULL range → NULL), handled in the eval kernel.
  | { kind: "rangeFunc"; func: RangeFuncName; args: RExpr[] }
  // A regex scalar function (spec/design/regex.md §8 — regexp_replace → text, regexp_match → text[]).
  // Like "arrayFunc" the result type lives in the surrounding ResolvedType. STRICT (a NULL arg →
  // NULL). `program` is the precompiled NFA for a constant pattern (regex.md §5), `compileCharged`
  // the one-shot flag charging its regex_compile cost once per execution.
  | {
      kind: "regexFunc";
      func: "replace" | "match";
      args: RExpr[];
      program: RegexProgram | null;
      compileCharged: boolean;
    }
  // A range CONSTRUCTOR call (spec/design/range-functions.md §2 — `i32range(lo, hi[, bounds])` and
  // the five siblings, plus the int4range/int8range aliases). `elem` is the range's element scalar
  // (the result range type is recovered from it, a bijection); `args` are the 2 bounds plus an
  // optional bounds-flags TEXT. Non-strict (null = "none"): a NULL bound is an infinite bound,
  // handled in the kernel. The kernel coerces each bound to `elem` (assignment-style), reads the
  // bounds flags, and finalizes (canonicalize / order-check / empty-normalize).
  | { kind: "rangeCtor"; elem: ScalarType; args: RExpr[] }
  // A range BOOLEAN operator (spec/design/range-functions.md §3 — `@> <@ && << >> &< &> -|-`).
  // `args` are the two operands. STRICT: a NULL operand → NULL (handled in the eval arm). `elem` is
  // the range's element scalar — used only by the "containsElem"/"elemContainedBy" element overloads
  // to coerce the bare-element operand to the range's element type at eval; unused (but carried) for
  // the range-against-range operators.
  | { kind: "rangeOp"; op: RangeOpName; args: RExpr[]; elem: ScalarType }
  // A range SET operator (spec/design/range-functions.md §4 — `+` union, `-` difference, `*`
  // intersection, and range_merge). `args` are the two range operands. STRICT: a NULL operand → NULL
  // (handled in the eval arm). Unlike "rangeOp" it carries no element scalar — the kernels work off
  // the self-describing operand values, and the result range type is fixed at resolve. The kernels
  // (rangeUnion/rangeIntersect/rangeMinus) live in range.ts; `+`/`-` raise 22000 on a non-contiguous
  // result.
  | { kind: "rangeSetOp"; op: RangeSetOpName; args: RExpr[] }
  // A VARIADIC argument-counting call (spec/design/array-functions.md §12 — num_nulls/num_nonnulls).
  // Non-strict (null = "none"), like "arrayFunc": no blanket NULL short-circuit. `arrayForm` records
  // the call shape — false = the spread form (count `args`' null-ness directly, never NULL); true =
  // the VARIADIC-array form (one `args` operand — a NULL array → NULL, else count its flattened
  // elements' null-ness). Result is always i32.
  | {
      kind: "variadic";
      func: VariadicFuncName;
      args: RExpr[];
      arrayForm: boolean;
    }
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
  | {
      kind: "quantified";
      op: BinaryOp;
      all: boolean;
      lhs: RExpr;
      array: RExpr;
    };

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
  | "lastval"
  // Session-variable read (spec/design/session.md §6.1): current_setting(text[, bool]) → text reads
  // the named session variable from the session's variable map. STABLE; 42704 on an unset name unless
  // the two-arg missing_ok is true (→ NULL).
  | "current_setting";

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

// RangeFuncName is the internal identity of a polymorphic range-accessor node
// (spec/design/range-functions.md §1, RF1). Each name is single-arity; the kernel recovers
// everything from the operand range value (self-describing). lower/upper yield the bound value
// (ELEM) or NULL when empty/unbounded; the rest yield boolean. All are STRICT (a NULL range → NULL).
type RangeFuncName =
  | "lower"
  | "upper"
  | "isempty"
  | "lower_inc"
  | "upper_inc"
  | "lower_inf"
  | "upper_inf";

// RangeOpName is the internal identity of a range BOOLEAN operator node
// (spec/design/range-functions.md §3, RF3). Each is a binary infix operator returning a definite
// boolean (a NULL operand short-circuits to NULL at eval, like the array containment operators).
// "containsElem"/"elemContainedBy" are the element overloads of @>/<@ (the other operand is a bare
// element coerced to the range's element type); the rest are range-against-range. The kernels live in
// range.ts.
type RangeOpName =
  | "contains" // a @> b — range a contains range b
  | "containsElem" // r @> e — range r contains element e (the element overload of @>)
  | "containedBy" // a <@ b — range a is contained by range b
  | "elemContainedBy" // e <@ r — element e is contained by range r (the element overload of <@)
  | "overlaps" // a && b — ranges a and b overlap
  | "before" // a << b — a is strictly left of b
  | "after" // a >> b — a is strictly right of b
  | "overleft" // a &< b — a does not extend to the right of b
  | "overright" // a &> b — a does not extend to the left of b
  | "adjacent"; // a -|- b — a and b are adjacent

// RangeSetOpName is the internal identity of a range SET operator node (spec/design/range-functions.md
// §4, RF4). Each combines two ranges over a common element type into a new range. "union"/"difference"
// raise 22000 on a non-contiguous result; "intersect"/"merge" never error. The kernels live in
// range.ts.
type RangeSetOpName =
  | "union" // a + b — the smallest single range covering both (22000 if they leave a gap)
  | "intersect" // a * b — the overlap (empty when the ranges are disjoint)
  | "difference" // a - b — the part of a not in b (22000 if b splits a in two)
  | "merge"; // range_merge(a, b) — like union but spans any gap silently (never errors)

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
type PlanRel = {
  tableName: string;
  offset: number;
  colCount: number;
  srf?: SrfPlan;
  cte?: number;
  derived?: QueryPlan;
  lateral?: boolean;
};

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
    columns: [
      {
        name: alias ?? funcName,
        type: colTy,
        decimal: null,
        primaryKey: false,
        notNull: false,
        default: null,
        defaultExpr: null,
        identity: null,
        collation: null,
      },
    ],
    pk: [],
    checks: [],
    indexes: [],
    fks: [],
  };
}

// PlanJoin is one join in a SELECT plan: its kind and resolved ON predicate (null for CROSS). The
// right relation is rels[k+1].
type PlanJoin = { kind: JoinKind; on: RExpr | null };

// OrderSlot is a resolved ORDER BY key: a flat/synthetic slot + per-key direction flags + an optional
// collation. A null collation is the C/value order; a non-null collation orders this key by its UCA
// sort key (spec/design/collation.md §8) via the decorate sorter — it never reaches the spill Sorter
// (collation is in-memory only this slice), which ignores the field.
type OrderSlot = {
  idx: number;
  descending: boolean;
  nullsFirst: boolean;
  collation: Collation | null;
};

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
  // hasWindow is true when the select list has a window function — the query runs the blocking
  // WINDOW stage (after WHERE, before ORDER BY/LIMIT) and takes the eager path (never streaming).
  // Mutually exclusive with isAgg in S0 (spec/design/window.md §5.2).
  hasWindow: boolean;
  // windowSpecs holds one resolved window function per select-list OVER call (empty unless
  // hasWindow). The window stage appends each spec's per-row result after the input columns, so the
  // projection references result i as flat slot inputWidth + i (spec/design/window.md §5.1).
  windowSpecs: WindowSpec[];
  having: RExpr | null;
  order: OrderSlot[];
  projections: RExpr[];
  columnNames: string[];
  columnTypes: ResolvedType[];
  distinct: boolean;
  limit: bigint | null;
  offset: bigint | null;
  // pkOrdered reports that ORDER BY is satisfied by the single base relation's PRIMARY-KEY scan
  // order — the table tree already yields rows in this order, so the sort is elided (and with a
  // LIMIT the scan short-circuits a top-N). True iff the query is a single-table, non-aggregate,
  // non-DISTINCT SELECT whose ORDER BY keys are a prefix of the PK columns, each ASC with the
  // column's stored key collation (spec/design/cost.md §3 "ORDER BY satisfied by primary-key
  // order"). DESC (reverse scan) and secondary-index order are follow-ons.
  pkOrdered: boolean;
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

// WithPlan is a planned nested `WITH … query_expr` (spec/design/cte.md §7): the nested CTE bindings
// + their inline/materialize modes, and the inner query plan that references them. At execution the
// bindings are materialized once and `body` runs against a fresh CTE context (they establish their
// own scope — the enclosing context is NOT chained in, the documented narrowing §7). columnTypes /
// columnNames mirror the body's, so a WithPlan exposes its output columns like any other plan.
type WithPlan = {
  kind: "with";
  bindings: CteBinding[];
  modes: CteMode[];
  body: QueryPlan;
  columnNames: string[];
  columnTypes: ResolvedType[];
};

// QueryPlan is a resolved query expression: a SELECT plan, a set-op plan, a VALUES-body relation, or
// a nested WITH plan (mirrors QueryExpr's bodies). A VALUES plan is only ever produced as a
// derived-table body.
type QueryPlan = SelectPlan | SetOpPlan | ValuesPlan | WithPlan;

// CteMode is how a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE
// from its reference count and [NOT] MATERIALIZED hint: a single-reference CTE is "inline", a
// multi-reference (or MATERIALIZED) one is "materialize".
//   "inline":      run the body in place at each reference (re-evaluates per outer row under
//                  correlation, matching PostgreSQL); charges the body's intrinsic cost, no
//                  cte_scan_row.
//   "materialize": run the body once, buffer the rows; each reference scans the buffer, charging
//                  cte_scan_row per buffered row.
type CteMode = "inline" | "materialize";

// CteBinding is a planned common table expression (spec/design/cte.md), built by planCteBindings for
// the whole statement so the scopes that reference its synthetic `table` can see it. `name` is
// lowercased for case-insensitive FROM matching; `table` is the synthetic relation exposing the
// body's output columns; `source` is the planned body (a query plan, or — spec/design/writable-cte.md
// — a data-modifying statement); `hint` is the [NOT] MATERIALIZED override (true/false/null); `refs`
// counts the FROM references resolved to it during planning (the inline-vs-materialize decision —
// cost.md §3).
// For a RECURSIVE CTE (spec/design/recursive-cte.md) `source` holds the non-recursive (anchor) term
// (its column types fix the synthetic relation's) and `recursive` carries the recursive term + the
// UNION ALL flag; the binding is in scope inside its own recursive term, so the self-reference
// resolves to it.
type RecursiveTerm = { plan: QueryPlan; unionAll: boolean };
type CteBinding = {
  name: string;
  table: Table;
  source: CteSource;
  recursive: RecursiveTerm | null;
  hint: boolean | null;
  refs: number;
};

// CteSource is what a CTE binding evaluates to (spec/design/cte.md, writable-cte.md). A plain CTE
// holds a planned query body; a DATA-MODIFYING CTE holds the statement to execute (for its effect +
// RETURNING buffer). A data-modifying CTE is always materialized (writable-cte.md §3), so the
// inline-execution path never touches a "dml" source.
type CteSource = { kind: "query"; plan: QueryPlan } | { kind: "dml"; dm: DmCte };

// DmCte is a data-modifying CTE's body (spec/design/writable-cte.md): the INSERT/UPDATE/DELETE to run
// (cloned from the AST, executed with the statement's CTE context threaded in) and whether it has no
// RETURNING clause — in which case a FROM reference to it is 0A000 (§5).
type DmCte = { stmt: DmStmt; noReturning: boolean };

// DmStmt is a data-modifying statement in a writable-CTE position (a CTE body or the WITH primary).
type DmStmt = Insert | Update | Delete;

// CteCtx is the per-statement CTE execution context, threaded through exec_* and EvalEnv so a FROM
// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). `modes` and
// `bindings` are fixed after planning; `buffers` is filled before the main query runs — one slot per
// CTE in list order, holding the materialized rows of a "materialize" CTE (an empty placeholder for
// an "inline" one, whose body is run in place from `bindings[ci].source` instead). `bindings` also
// serves a data-modifying CTE's own inner queries, which resolve against the earlier bindings when
// the writable-CTE orchestrator executes them (writable-cte.md §2). EMPTY_CTE_CTX is the empty
// context for every non-WITH execution path.
type CteCtx = { modes: CteMode[]; bindings: CteBinding[]; buffers: Row[][] };
const EMPTY_CTE_CTX: CteCtx = { modes: [], bindings: [], buffers: [] };

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
type AggSpec = {
  plan: AggPlan;
  operand: RExpr | null;
  floatWidth?: ScalarType;
};

// AggCtx threads the aggregate-resolution mode through resolve. collecting === false is the
// Forbidden mode (a funcCall is 42803; columns resolve normally); collecting === true is an
// aggregate query's projection (a funcCall collects into specs and resolves to a synthetic slot
// groupKeys.length + index; a column resolves to its position among groupKeys if it is a
// grouping key, else 42803). groupKeys holds the resolved flat indices of the GROUP BY columns
// (empty for whole-table aggregation). The synthetic row is [group_key_values..., agg_results...].
//
// `window` is set (and collecting === false) for a non-aggregate WINDOW query's projection
// (spec/design/window.md §5.1). Bare columns resolve to the real input row (like Forbidden); a
// funcCall carrying an OVER clause collects into windowSpecs and resolves to the synthetic slot
// base + windowIndex, where base is the input row's flat width — the window stage appends each
// function's result after the input columns. S0 narrows window + aggregate/GROUP BY to 0A000 (so
// `collecting` and `window` are never both set), and an aggregate in a window context is 42803.
type AggCtx = {
  collecting: boolean;
  groupKeys: number[];
  specs: AggSpec[];
  window?: { base: number; windowSpecs: WindowSpec[] };
};

// WindowSpec is one resolved window function (spec/design/window.md §5.1): its plan, the resolved
// PARTITION BY key column slots (flat input-row indices), and the resolved within-partition ORDER
// BY (sort keys over the input row, PK tie-break applied by the stable sort over the PK-ordered
// scan).
type WindowSpec = {
  plan: WindowPlan;
  partition: number[];
  order: OrderSlot[];
};

// WindowPlan is the runtime plan for one window function (spec/design/window.md §4). S0:
// row_number only; ranking / offset / aggregate-window / frame plans land in S1–S4.
type WindowPlan =
  // ROW_NUMBER() — the 1-based sequence position within the partition (frame-insensitive).
  "rowNumber";

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
    case "extract":
      return exprHasAggregate(e.source);
    case "collate":
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
    case "regex":
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

// itemsHaveWindow reports whether any select item contains a window-function call (a funcCall
// carrying OVER). A window query resolves its projection in window mode (spec/design/window.md §5.1).
function itemsHaveWindow(items: SelectItems): boolean {
  if (items.kind === "all") return false;
  return items.items.some((it) => exprHasWindow(it.expr));
}

// exprHasWindow reports whether an expression tree contains a window-function call anywhere (a
// funcCall whose `over` is set). An ordinary call may CONTAIN one in its arguments
// (abs(row_number() OVER ())), so the arguments are walked; a window call's own PARTITION BY /
// ORDER BY may not contain a window function (rejected at resolve, 42P20), so they are not walked
// here. A subquery is an independent query — a window inside it is the subquery's own.
function exprHasWindow(e: Expr): boolean {
  switch (e.kind) {
    case "funcCall":
      return (e.over !== undefined && e.over !== null) || e.args.some(exprHasWindow);
    case "cast":
      return exprHasWindow(e.inner);
    case "extract":
      return exprHasWindow(e.source);
    case "collate":
      return exprHasWindow(e.inner);
    case "unary":
      return exprHasWindow(e.operand);
    case "isNull":
      return exprHasWindow(e.operand);
    case "binary":
    case "isDistinct":
      return exprHasWindow(e.lhs) || exprHasWindow(e.rhs);
    case "in":
      return exprHasWindow(e.lhs) || e.list.some(exprHasWindow);
    case "between":
      return exprHasWindow(e.lhs) || exprHasWindow(e.lo) || exprHasWindow(e.hi);
    case "like":
    case "regex":
      return exprHasWindow(e.lhs) || exprHasWindow(e.rhs);
    case "case":
      return (
        (e.operand !== null && exprHasWindow(e.operand)) ||
        e.whens.some((w) => exprHasWindow(w.cond) || exprHasWindow(w.result)) ||
        (e.els !== null && exprHasWindow(e.els))
      );
    case "row":
      return e.fields.some(exprHasWindow);
    case "array":
      return e.elements.some(exprHasWindow);
    case "fieldAccess":
    case "fieldStar":
      return exprHasWindow(e.base);
    case "subscript":
      return exprHasWindow(e.base) || astSubscriptExprs(e.subscripts).some(exprHasWindow);
    case "quantified":
      return exprHasWindow(e.lhs) || exprHasWindow(e.array);
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
function evalChecks(
  checks: NamedCheck[],
  relation: string,
  row: Row,
  env: EvalEnv,
  meter: Meter,
): void {
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
        throw engineError(
          "grouping_error",
          "aggregate functions are not allowed in check constraints",
        );
      }
      for (const a of e.args) rejectCheckStructure(a);
      return;
    case "cast":
      return rejectCheckStructure(e.inner);
    case "extract":
      return rejectCheckStructure(e.source);
    case "collate":
      return rejectCheckStructure(e.inner);
    case "unary":
    case "isNull":
      return rejectCheckStructure(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
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
      throw engineError(
        "feature_not_supported",
        "cannot use column reference in DEFAULT expression",
      );
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      throw engineError("feature_not_supported", "cannot use subquery in DEFAULT expression");
    case "param":
      throw engineError("undefined_parameter", "there is no parameter $" + e.index.toString());
    case "funcCall":
      if (isAggregateName(e.name)) {
        throw engineError(
          "grouping_error",
          "aggregate functions are not allowed in DEFAULT expressions",
        );
      }
      for (const a of e.args) rejectDefaultStructure(a);
      return;
    case "cast":
      return rejectDefaultStructure(e.inner);
    case "extract":
      return rejectDefaultStructure(e.source);
    case "collate":
      return rejectDefaultStructure(e.inner);
    case "unary":
    case "isNull":
      return rejectDefaultStructure(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
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
      case "extract":
        return walk(e.source);
      case "collate":
        return walk(e.inner);
      case "unary":
      case "isNull":
        return walk(e.operand);
      case "binary":
      case "isDistinct":
      case "like":
      case "regex":
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
    if (!aggregateHasStar(name))
      throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    plan = "countStar";
    operand = null;
    result = { kind: "int", ty: "i64" };
  } else {
    // One operand, resolved in a fresh Forbidden sub-context. The registry validates the (surface,
    // operand-family) overload exists (else 42883) and yields its result code; the plan + result
    // type follow from it (the PG widening). Each aggregate takes exactly one argument.
    if (e.args.length !== 1) {
      throw engineError(
        "undefined_function",
        "no aggregate function matches the given argument count",
      );
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
// aggregate context. In Forbidden mode (and Window mode — a window query's bare columns are not
// grouped, spec/design/window.md §5.1) it reads the real input row directly; in collect mode it
// must be a grouping key — resolved to its synthetic-row slot (its position among the group keys)
// — else 42803.
function collectColumn(
  scope: Scope,
  ag: AggCtx,
  idx: number,
  name: string,
): { node: RExpr; type: ResolvedType } {
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

// isWindowOnlyName reports whether name is a registered WINDOW-only function surface
// (row_number/rank/…). Data-driven over the catalog (WINDOWS). Such a function REQUIRES an OVER
// clause — used without one it is 42809 (spec/design/window.md §7). The catalog aggregates double
// as window functions but are not in WINDOWS, so they are still valid without OVER.
function isWindowOnlyName(name: string): boolean {
  const lname = name.toLowerCase();
  return WINDOWS.some((w) => w.surface.toLowerCase() === lname);
}

// resolveWindowCall resolves a window-function call `f(args) OVER (window_definition)`
// (spec/design/window.md §5.1). Valid only in a window query's projection (ag.window set);
// anywhere else (WHERE / JOIN ON / HAVING / an aggregate query) it is 42P20. The call collects
// into a WindowSpec and resolves to the synthetic slot base + windowIndex. S0: only row_number().
function resolveWindowCall(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean; over: WindowDef },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const lname = e.name.toLowerCase();
  // The plan + result type from the function name. S0: only row_number(); an aggregate name with
  // OVER (a window aggregate) is deferred to S3; any other name is 42883.
  let plan: WindowPlan;
  let result: ResolvedType;
  if (lname === "row_number") {
    if (e.star || e.args.length !== 0) {
      throw engineError("undefined_function", "row_number takes no arguments");
    }
    plan = "rowNumber";
    result = { kind: "int", ty: "i64" };
  } else if (isAggregateName(lname)) {
    throw engineError("feature_not_supported", "aggregate window functions are not supported yet");
  } else {
    throw engineError("undefined_function", `${lname} is not a window function`);
  }
  // Resolve the window definition (PARTITION BY columns → flat slots, ORDER BY → sort keys) — all
  // against the input scope, never recursing into `ag`. Done before collecting the spec.
  const [partition, order] = resolveWindowDef(scope, e.over, params);
  // A window function is allowed only in a window query's projection. In WHERE / a JOIN ON /
  // HAVING / an aggregate query `ag.window` is unset → 42P20 (window.md §7).
  if (ag.window === undefined) {
    throw engineError("windowing_error", "window functions are not allowed here");
  }
  const slot = ag.window.base + ag.window.windowSpecs.length;
  ag.window.windowSpecs.push({ plan, partition, order });
  return { node: { kind: "column", index: slot }, type: result };
}

// resolveWindowDef resolves the PARTITION BY column list (→ flat input-row slots) and the
// within-partition ORDER BY (→ sort keys) of an OVER (...) clause, against the (non-aggregate)
// input scope. Mirrors the query ORDER BY resolution (the collation / direction / NULLS handling).
// S0: partition keys are columns only. A window function in a partition/order key, or an outer
// reference, is rejected.
function resolveWindowDef(
  scope: Scope,
  wd: WindowDef,
  _params: ParamTypes,
): [number[], OrderSlot[]] {
  const partition: number[] = [];
  for (const key of wd.partition) {
    let r: Resolved;
    if (key.kind === "column") {
      r = scope.resolveBare(key.name);
    } else if (key.kind === "qualifiedColumn") {
      r = scope.resolveQualified(key.qualifier, key.name);
    } else {
      throw engineError("feature_not_supported", "PARTITION BY supports only column references");
    }
    if (r.level !== 0) {
      throw engineError(
        "feature_not_supported",
        "PARTITION BY may not reference an outer query column",
      );
    }
    partition.push(r.index);
  }
  const order: OrderSlot[] = [];
  for (const key of wd.order) {
    const r =
      key.qualifier !== null
        ? scope.resolveQualified(key.qualifier, key.column)
        : scope.resolveBare(key.column);
    if (r.level !== 0) {
      throw engineError(
        "feature_not_supported",
        "window ORDER BY may not reference an outer query column",
      );
    }
    const idx = r.index;
    let collation: Collation | null = null;
    if (key.collation !== null) {
      if (!typeIsText(scope.columnAt(idx).type)) {
        throw typeError(
          `collations are not supported by type ${typeCanonicalName(scope.columnAt(idx).type)}`,
        );
      }
      collation = resolveCollationName(scope.catalog, key.collation);
    } else {
      const cn = scope.columnAt(idx).collation;
      if (cn !== null) collation = resolveCollationName(scope.catalog, cn);
    }
    // A non-C collated window ORDER BY is deferred in S0 (the window stage's per-partition sort is
    // the plain comparator) — 0A000, a documented narrowing (spec/design/window.md §11).
    if (collation !== null) {
      throw engineError("feature_not_supported", "collated window ORDER BY is not supported yet");
    }
    order.push({ idx, descending: key.descending, nullsFirst: key.nullsFirst, collation });
  }
  return [partition, order];
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
    case "range":
      // No concrete built-in argument family for ranges this slice (the polymorphic `anyrange`
      // family is matched separately by the range resolver — RF1).
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
  if (surface === "sum" && code === "decimal")
    return ["sumDecimal", { kind: "decimal" }, undefined];
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
  e: {
    name: string;
    args: Expr[];
    argNames: (string | null)[];
    star: boolean;
    variadic: boolean;
  },
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
  // lower/upper are overloaded across the range accessors (range → element) and the text casing
  // functions (text → text, collation.md §16). Resolve the single argument once and branch on its
  // type, BEFORE the by-name kind dispatch (which would force the range path for both). functions.md §9
  if (lname === "lower" || lname === "upper") {
    rejectNamed(lname, e.argNames);
    if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    return resolveLowerUpper(scope, lname, e, ag, params);
  }
  // timezone(zone, value) is the desugar of `value AT TIME ZONE zone` (grammar.md §49, timezones.md
  // §6) and a callable function. Overloaded on the value's family (timestamptz → timestamp, timestamp
  // → timestamptz), so it resolves before the generic by-name dispatch. functions.md §9
  if (lname === "timezone") {
    rejectNamed(lname, e.argNames);
    if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    return resolveTimezone(scope, e, ag, params);
  }
  // date_trunc(unit, value[, zone]) (timezones.md §9.1) — polymorphic on the value family (the result
  // type is the value type) + an optional 3rd zone arg only on a timestamptz, so it resolves before
  // the generic by-name dispatch (which has no such polymorphism).
  if (lname === "date_trunc") {
    rejectNamed(lname, e.argNames);
    if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
    return resolveDateTrunc(scope, e, ag, params);
  }
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
  // The polymorphic range accessors (range-functions.md §1) are kind === "function" too, with an
  // anyrange slot the generic scalar path cannot unify — intercept BEFORE the scalar gate.
  if (isRangeFuncName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveRangeFunc(scope, e, ag, params);
  }
  // A range CONSTRUCTOR (range-functions.md §2): a call whose name is a range type name/alias. Like
  // the array/range functions it is kind === "function", so it must be intercepted BEFORE the generic
  // scalar path (isScalarFuncName matches every function row, the constructor rows included) — its
  // concrete-range result + element coercion are not the family-matched scalar mold.
  if (isRangeCtorName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveRangeCtor(scope, e, ag, params);
  }
  // The regex scalar functions (regex.md §8) are kind === "function" too, but return text / text[]
  // via a dedicated regexFunc node, so they are intercepted before the generic scalar path.
  if (lname === "regexp_replace" || lname === "regexp_match") {
    rejectNamed(lname, e.argNames);
    return resolveRegexFunc(scope, e, ag, params);
  }
  if (isScalarFuncName(lname)) {
    rejectNamed(lname, e.argNames);
    return resolveScalarFunc(scope, e, ag, params);
  }
  throw engineError("undefined_function", "function does not exist: " + e.name);
}

// resolveRegexFunc resolves regexp_replace/regexp_match (regex.md §8) → a regexFunc node whose result
// type (text / text[]) lives in the surrounding ResolvedType. Both are STRICT (text args, NULL
// propagates). A constant pattern is precompiled once here (regex.md §5) — but only when the
// case-insensitive `i` flag is statically known (the flags arg absent or a constant).
function resolveRegexFunc(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase();
  let func: "replace" | "match";
  let flagsIdx = -1;
  if (name === "regexp_replace" && e.args.length === 3) func = "replace";
  else if (name === "regexp_replace" && e.args.length === 4) {
    func = "replace";
    flagsIdx = 3;
  } else if (name === "regexp_match" && e.args.length === 2) func = "match";
  else if (name === "regexp_match" && e.args.length === 3) {
    func = "match";
    flagsIdx = 2;
  } else throw noFuncOverload(name);

  const rargs: RExpr[] = [];
  for (const a of e.args) {
    const r = resolve(scope, a, "text", ag, params);
    requireTextOrNull(r.type);
    rargs.push(r.node);
  }
  // Precompile a constant pattern (rargs[1]) once, folding it for a statically-constant `i` flag.
  let insensitive: boolean | null = false;
  if (flagsIdx >= 0) {
    const f = rargs[flagsIdx];
    insensitive = f.kind === "constText" ? f.value.includes("i") : null;
  }
  let program: RegexProgram | null = null;
  if (rargs[1].kind === "constText" && insensitive !== null) {
    const pat = insensitive ? foldLowerSimple(rargs[1].value, loadedProperty()) : rargs[1].value;
    program = compileRegex(pat);
  }
  const type: ResolvedType =
    func === "replace" ? { kind: "text" } : { kind: "array", elem: { kind: "text" } };
  return { node: { kind: "regexFunc", func, args: rargs, program, compileCharged: false }, type };
}

// rejectNamed throws 42883 if any argument is named — named notation is valid only for a function
// that declares parameter names (PG's "function ... has no parameter named X").
function rejectNamed(name: string, argNames: (string | null)[]): void {
  for (const n of argNames) {
    if (n !== null) {
      throw engineError(
        "undefined_function",
        "function " + name + ' has no parameter named "' + n + '"',
      );
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
      throw engineError(
        "undefined_function",
        "function " + desc.name + ' has no parameter named "' + nm + '"',
      );
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
  return {
    node: { kind: "scalarFunc", func, args, result, argWidth },
    type: resolvedTypeOf(result),
  };
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
        if (varFamily !== "any" && !familyMatches(varFamily, r.type.elem))
          throw noFuncOverload(name);
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
  return {
    node: { kind: "variadic", func: name, args: rargs, arrayForm: e.variadic },
    type: resolvedTypeOf(result),
  };
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
    (o) =>
      o.kind === "function" &&
      o.name === name &&
      o.argFamilies.some((f) => f === "anyarray" || f === "anyelement"),
  );
}

// resolvedTypeEqual reports structural equality of two resolved types (the unification check):
// integers/floats by width, arrays recursively by element type, composites by name + field types,
// everything else by kind.
function resolvedTypeEqual(a: ResolvedType, b: ResolvedType): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "int" || a.kind === "float") return a.ty === (b as { ty: ScalarType }).ty;
  if (a.kind === "array" || a.kind === "range")
    return resolvedTypeEqual(a.elem, (b as { elem: ResolvedType }).elem);
  if (a.kind === "composite") {
    const bc = b as {
      name: string | null;
      fields: { name: string; type: ResolvedType }[];
    };
    if (a.name !== bc.name || a.fields.length !== bc.fields.length) return false;
    return a.fields.every((f, i) => resolvedTypeEqual(f.type, bc.fields[i]!.type));
  }
  return true;
}

// matchPoly matches an overload's slots (which may contain anyarray/anyelement) against the resolved
// argument types, returning { elem, matched }. When matched, elem is null if every polymorphic arg was
// an untyped NULL (ELEM undeterminable). Three passes: anyarray (binds ELEM := the element type),
// anyelement (may precede its binding array — array_prepend), then concrete family slots.
function matchPoly(
  slots: readonly string[],
  tys: ResolvedType[],
): { elem: ResolvedType | null; matched: boolean } {
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
  // anyrange binds ELEM := the range's element type, like anyarray (both definitive, before
  // anyelement) — range-functions.md §1.
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] === "anyrange") {
      const t = tys[j]!;
      if (t.kind === "range") {
        if (!unify(t.elem)) return { elem: null, matched: false };
      } else if (t.kind !== "null") {
        return { elem: null, matched: false }; // a non-range where anyrange is required
      }
    }
  }
  for (let j = 0; j < slots.length; j++) {
    if (slots[j] === "anyelement" && tys[j]!.kind !== "null") {
      if (!unify(tys[j]!)) return { elem: null, matched: false };
    }
  }
  for (let j = 0; j < slots.length; j++) {
    if (
      slots[j] !== "anyarray" &&
      slots[j] !== "anyrange" &&
      slots[j] !== "anyelement" &&
      !familyMatches(slots[j]!, tys[j]!)
    ) {
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
  if (code === "anyrange") {
    if (elem === null) throw indeterminatePoly();
    return { kind: "range", elem };
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
  return engineError(
    "indeterminate_datatype",
    "could not determine polymorphic type because input has type unknown",
  );
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
  const desc = OPERATORS.find(
    (o) => o.kind === "function" && o.name === name && o.arity === e.args.length,
  );
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

// isRangeFuncName reports whether name (lowercased) is a polymorphic range function — a
// kind === "function" catalog row whose argFamilies mention anyrange (range-functions.md §1).
// Data-driven, so a new range-function row wires here without touching this gate.
function isRangeFuncName(name: string): boolean {
  return OPERATORS.some(
    (o) => o.kind === "function" && o.name === name && o.argFamilies.some((f) => f === "anyrange"),
  );
}

// rangeFuncId is the kernel id for range accessor name (each is single-arity, so the name selects
// the kernel). Total over the catalog's range-function names (isRangeFuncName gates the call).
function rangeFuncId(name: string): RangeFuncName {
  switch (name) {
    case "lower":
    case "upper":
    case "isempty":
    case "lower_inc":
    case "upper_inc":
    case "lower_inf":
    case "upper_inf":
      return name;
    default:
      throw new Error("rangeFuncId: " + name + " is not a catalog range function");
  }
}

// resolveLowerUpper resolves lower/upper, overloaded across the range accessors and the text casing
// functions (functions.md §9, collation.md §16). The single argument resolves once (offering "text" as
// the literal-adaptation hint, so a bare NULL / untyped $1 adapts to text — the common case; a typed
// range keeps its range type and ignores the scalar hint). A text/NULL argument folds case ("casing",
// result text); a range argument is the bound accessor ("rangeFunc", result the element type);
// anything else is 42883 (no overload).
function resolveLowerUpper(
  scope: Scope,
  name: string,
  e: { name: string; args: Expr[] },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.args.length !== 1) throw noFuncOverload(name);
  const r = resolve(scope, e.args[0], "text", ag, params);
  if (r.type.kind === "text" || r.type.kind === "null") {
    return {
      node: { kind: "casing", upper: name === "upper", arg: r.node },
      type: { kind: "text" },
    };
  }
  if (r.type.kind === "range") {
    return {
      node: { kind: "rangeFunc", func: rangeFuncId(name), args: [r.node] },
      type: r.type.elem,
    };
  }
  throw noFuncOverload(name);
}

// resolveTimezone resolves timezone(zone, value) — the desugar of `value AT TIME ZONE zone`
// (timezones.md §6). zone must be text (else 42804); the result family is the OTHER timestamp family
// of value: timestamptz → timestamp (render the instant locally) and timestamp → timestamptz
// (interpret the wall clock in the zone). Any other value family — or an untyped/NULL value, which
// cannot pick an overload — is 42883.
function resolveTimezone(
  scope: Scope,
  e: { name: string; args: Expr[] },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.args.length !== 2) throw noFuncOverload("timezone");
  const zone = resolve(scope, e.args[0], "text", ag, params);
  const value = resolve(scope, e.args[1], null, ag, params);
  // A non-text zone, or a non-timestamp value, is 42883 — PG resolves AT TIME ZONE via function
  // overload (timezone(text, timestamptz) / timezone(text, timestamp)), so any other arg pair is "no
  // such function" (PG-matching, oracle-pinned), not a datatype_mismatch. A NULL zone is allowed (it
  // propagates to NULL at eval).
  const zoneOk = zone.type.kind === "text" || zone.type.kind === "null";
  if (zoneOk && value.type.kind === "timestamptz") {
    return {
      node: { kind: "atTimeZone", zone: zone.node, value: value.node, toTimestamptz: false },
      type: { kind: "timestamp" },
    };
  }
  if (zoneOk && value.type.kind === "timestamp") {
    return {
      node: { kind: "atTimeZone", zone: zone.node, value: value.node, toTimestamptz: true },
      type: { kind: "timestamptz" },
    };
  }
  throw noFuncOverload("timezone");
}

// resolveDateTrunc resolves date_trunc(unit, value[, zone]) (timezones.md §9.1). unit is text (a
// runtime value, validated at eval); value is timestamp / timestamptz / interval; the optional zone
// (text) is the 3-arg form, valid only for a timestamptz value. The result family is the value
// family. A non-text unit/zone, a non-datetime value, or the 3-arg form on a non-timestamptz value is
// 42883 (a date value also has no overload — jed has no implicit date->timestamp cast).
function resolveDateTrunc(
  scope: Scope,
  e: { name: string; args: Expr[] },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.args.length !== 2 && e.args.length !== 3) throw noFuncOverload("date_trunc");
  const unit = resolve(scope, e.args[0], "text", ag, params);
  const value = resolve(scope, e.args[1], null, ag, params);
  if (unit.type.kind !== "text" && unit.type.kind !== "null") throw noFuncOverload("date_trunc");
  const vk = value.type.kind;
  if (vk !== "timestamp" && vk !== "timestamptz" && vk !== "interval") {
    throw noFuncOverload("date_trunc");
  }
  let zone: RExpr | null = null;
  if (e.args.length === 3) {
    if (vk !== "timestamptz") throw noFuncOverload("date_trunc");
    const z = resolve(scope, e.args[2], "text", ag, params);
    if (z.type.kind !== "text" && z.type.kind !== "null") throw noFuncOverload("date_trunc");
    zone = z.node;
  }
  return {
    node: { kind: "dateTrunc", unit: unit.node, value: value.node, zone },
    type: value.type,
  };
}

// resolveRangeFunc resolves a polymorphic range accessor over the anyrange pseudo-family
// (range-functions.md §1). Simpler than resolveArrayFunc — the accessors take a single anyrange arg
// with no anyelement arg, so there is no element-hint literal adaptation. lower/upper resolve to ELEM
// (the bound type), the rest to boolean. The kernel id is the name; NULL handling lives in the eval
// kernel.
function resolveRangeFunc(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase();
  const desc = OPERATORS.find(
    (o) => o.kind === "function" && o.name === name && o.arity === e.args.length,
  );
  if (!desc) throw noFuncOverload(name);
  const slots = desc.argFamilies;

  const rargs: RExpr[] = [];
  const tys: ResolvedType[] = [];
  for (const a of e.args) {
    const r = resolve(scope, a, null, ag, params);
    rargs.push(r.node);
    tys.push(r.type);
  }
  const { elem, matched } = matchPoly(slots, tys);
  if (!matched) throw noFuncOverload(name);
  const type = polyResultType(desc.result, elem);
  // range_merge(anyrange, anyrange) → anyrange is a SET operation (= union, non-strict), not a scalar
  // accessor: emit the shared "rangeSetOp" node (range-functions.md §4). polyResultType already raised
  // 42P18 if the element was indeterminate (both args untyped NULL), so the result type is bound here.
  if (name === "range_merge") {
    return { node: { kind: "rangeSetOp", op: "merge", args: rargs }, type };
  }
  return {
    node: { kind: "rangeFunc", func: rangeFuncId(name), args: rargs },
    type,
  };
}

// isRangeCtorName reports whether name (lowercased) is a range CONSTRUCTOR call
// (range-functions.md §2): a call whose name is a range type name or alias (i32range/int4range/
// numrange/…). The constructor functions are the only ones whose name is a range type name, so
// rangeByName resolving is exactly the gate — data-driven over the RANGES table, no hand-written
// name list.
function isRangeCtorName(name: string): boolean {
  return rangeByName(name) !== undefined;
}

// rangeBoundAssignable reports whether a bound argument of resolved type `t` is assignable to range
// element `elem`, mirroring the storeValue coercions the kernel will apply (range-functions.md §2):
// a NULL is an infinite bound (always ok); an integer adapts to an integer (range-checked) or
// decimal element; a decimal to a decimal element; an already-temporal value to its own element;
// and a string literal/text to a temporal element (parsed at eval). Anything else is no overload
// (42883).
function rangeBoundAssignable(t: ResolvedType, elem: ScalarType): boolean {
  switch (t.kind) {
    case "null":
      return true;
    case "int":
      return isInteger(elem) || isDecimal(elem);
    case "decimal":
      return isDecimal(elem);
    case "timestamp":
      return isTimestamp(elem);
    case "timestamptz":
      return isTimestamptz(elem);
    case "date":
      return isDate(elem);
    case "text":
      return isTimestamp(elem) || isTimestamptz(elem) || isDate(elem);
    default:
      return false;
  }
}

// resolveRangeCtor resolves a range constructor call (i32range(lo, hi[, bounds]) and the five
// siblings, plus the int4range/int8range aliases — range-functions.md §2). The target range type
// comes from the call name (rangeByName, alias-aware); the result type is fixed (concrete), not
// polymorphic. Each bound resolves with the element scalar as the literal-adaptation context (so `1`
// adapts to the element width, `'2024-01-01'` to a date), then is type-checked assignable to the
// element; the optional third argument is the bounds-flags TEXT. The kernel (evalRangeCtor) does the
// element coercion (assignment-style, 22003), the flags parse (42601 / 22000), and finalize.
function resolveRangeCtor(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  if (e.star) throw engineError("syntax_error", "* is only valid as the argument of COUNT");
  const name = e.name.toLowerCase();
  const desc = rangeByName(name);
  if (desc === undefined) throw new Error("isRangeCtorName gated the call");
  const elem = elementScalar(desc);
  // Only the 2-arg (lo, hi) and 3-arg (lo, hi, bounds) overloads exist.
  if (e.args.length !== 2 && e.args.length !== 3) throw noFuncOverload(name);
  const rargs: RExpr[] = [];
  for (let i = 0; i < e.args.length; i++) {
    const a = e.args[i]!;
    if (i < 2) {
      // A bound: offer the element scalar as the literal-adaptation hint, then check the resolved
      // type is assignable to the element (else no overload).
      const r = resolve(scope, a, elem, ag, params);
      if (!rangeBoundAssignable(r.type, elem)) throw noFuncOverload(name);
      rargs.push(r.node);
    } else {
      // The bounds-flags argument: TEXT (a NULL is allowed at resolve — the kernel traps it 22000
      // at eval, matching PG "flags argument must not be null").
      const r = resolve(scope, a, null, ag, params);
      if (r.type.kind !== "text" && r.type.kind !== "null") throw noFuncOverload(name);
      rargs.push(r.node);
    }
  }
  return {
    node: { kind: "rangeCtor", elem, args: rargs },
    type: { kind: "range", elem: resolvedTypeOf(elem) },
  };
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
type ScopeRel = {
  label: string;
  table: Table;
  offset: number;
  qualifierOnly?: boolean;
  cte?: number;
};

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
        rels.push({
          label: pseudo.label,
          table: t,
          offset: pseudo.offset,
          qualifierOnly: true,
        });
      }
    }
    return new Scope(rels, catalog, null, true);
  }

  // onConflictExcluded is the scope a DO UPDATE's SET/WHERE resolve against
  // (spec/design/upsert.md §5): the target table at offset 0 (bare and table-qualified references
  // read the EXISTING conflicting row), plus `excluded` as a QUALIFIER-ONLY relation at offset n
  // over the combined row [existing | proposed] (excluded.col reads the proposed row). A target
  // table literally named `excluded` SHADOWS the pseudo-relation (PostgreSQL's rule, like the
  // RETURNING old/new qualifiers).
  static onConflictExcluded(catalog: Database, t: Table): Scope {
    const n = t.columns.length;
    const label = t.name.toLowerCase();
    const rels: ScopeRel[] = [{ label, table: t, offset: 0 }];
    if (label !== "excluded") {
      rels.push({
        label: "excluded",
        table: t,
        offset: n,
        qualifierOnly: true,
      });
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

  // width returns the flat column count of THIS scope (the input-row width). It is the window base
  // offset: a window query appends each window function's result after the input columns
  // (spec/design/window.md §5.1), so window slot = width() + windowIndex.
  width(): number {
    return this.rels.reduce((sum, r) => sum + r.table.columns.length, 0);
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
  if (ty.kind === "range") return { kind: "range", elem: resolvedTypeOfCol(ty.elem, db) };
  const def = db.compositeType(ty.name);
  if (def === undefined) throw new Error("composite type reference resolved at load / CREATE TYPE");
  return {
    kind: "composite",
    name: def.name,
    fields: def.fields.map((f) => ({
      name: f.name,
      type: resolvedTypeOfCol(f.type, db),
    })),
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
    // A range source never assigns to a scalar column (a range column is not storable yet — R2).
    case "range":
      return false;
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
    case "range": {
      // A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
      const s = resolvedRangeElementScalar(t.elem);
      if (s !== undefined) {
        const name = rangeNameForElement(s);
        if (name !== undefined) return name;
      }
      return `range<${rtName(t.elem)}>`;
    }
    case "null":
      return "unknown";
  }
}

// resolvedRangeElementScalar returns the scalar element type of a resolved range element. A range's
// element is always one of the six scalar subtypes; undefined for anything else (never a valid
// range). Used to name a range and to build its codec.
function resolvedRangeElementScalar(elem: ResolvedType): ScalarType | undefined {
  switch (elem.kind) {
    case "int":
      return elem.ty;
    case "decimal":
      return "decimal";
    case "timestamp":
      return "timestamp";
    case "timestamptz":
      return "timestamptz";
    case "date":
      return "date";
    default:
      return undefined;
  }
}

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A WITH RECURSIVE CTE is recursive iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's checkWellFormedRecursion, run on the parsed AST
// before planning; the error surface is recursive-cte.md §6. The `name` argument is already
// lowercased (the CTE's lname).

// analyzeRecursiveCte classifies a CTE body for WITH RECURSIVE (recursive-cte.md §6). It returns
// { recursive: false } when the body does not reference name (an ordinary CTE, even under
// RECURSIVE); otherwise it validates the recursive shape and returns { recursive: true, unionAll },
// or throws (42P19 for a malformed recursion, 0A000 for a deferred shape).
function analyzeRecursiveCte(
  name: string,
  body: QueryExpr,
): { recursive: boolean; unionAll: boolean } {
  if (countSelfRefsQuery(body, name) === 0) {
    return { recursive: false, unionAll: false };
  }
  if (body.kind !== "setOp" || body.op !== "union") {
    throw engineError(
      "invalid_recursion",
      `recursive query "${name}" does not have the form non-recursive-term UNION [ALL] recursive-term`,
    );
  }
  if (body.orderBy.length > 0) {
    throw engineError("feature_not_supported", "ORDER BY in a recursive query is not implemented");
  }
  if (body.limit !== null || body.offset !== null) {
    throw engineError("feature_not_supported", "LIMIT in a recursive query is not implemented");
  }
  if (countSelfRefsQuery(body.lhs, name) > 0) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within its non-recursive term`,
    );
  }
  if (body.rhs.kind === "withExpr") {
    throw engineError(
      "feature_not_supported",
      "a nested WITH in the recursive term of a recursive query is not supported yet",
    );
  }
  if (body.rhs.kind !== "select") {
    throw engineError(
      "feature_not_supported",
      "a set operation in the recursive term of a recursive query is not supported yet",
    );
  }
  validateRecursiveTerm(name, body.rhs);
  return { recursive: true, unionAll: body.all };
}

// validateRecursiveTerm validates the recursive term (the UNION's right SELECT) of a recursive CTE
// (recursive-cte.md §6). The self-reference must appear exactly once, as a direct FROM/JOIN
// relation, not on the nullable side of an outer join; the term must contain no aggregate. The
// checks fire in PostgreSQL's order — a self-reference in a bad CONTEXT (a sublink, an outer join)
// is reported as that context even when a valid FROM reference also exists.
function validateRecursiveTerm(name: string, sel: Select): void {
  if (countSublinkSelfRefs(sel, name) >= 1) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within a subquery`,
    );
  }
  if (countFromSubquerySelfRefs(sel, name) >= 1) {
    throw engineError(
      "feature_not_supported",
      `recursive reference to query "${name}" inside a FROM subquery is not supported yet`,
    );
  }
  const direct = countDirectFromSelfRefs(sel, name);
  if (direct > 1) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear more than once`,
    );
  }
  if (itemsHaveAggregate(sel.items) || (sel.having !== null && exprHasAggregate(sel.having))) {
    throw engineError(
      "invalid_recursion",
      "aggregate functions are not allowed in a recursive query's recursive term",
    );
  }
  if (direct === 1 && directSelfRefOnNullableSide(sel, name)) {
    throw engineError(
      "invalid_recursion",
      `recursive reference to query "${name}" must not appear within an outer join`,
    );
  }
}

// withHasDml reports whether a WITH statement contains any data-modifying part — a data-modifying CTE
// body or a data-modifying primary (spec/design/writable-cte.md). Such a statement runs through the
// writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a pure-query
// WITH keeps the runWith path.
function withHasDml(wq: WithQuery): boolean {
  return cteBodyIsDataModifying(wq.body) || wq.ctes.some((c) => cteBodyIsDataModifying(c.body));
}

// cteModes computes each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a
// RECURSIVE or data-modifying CTE is ALWAYS materialized; otherwise a MATERIALIZED hint or >=2
// references → materialize, else inline.
function cteModes(bindings: CteBinding[]): CteMode[] {
  return bindings.map((b) => {
    if (b.recursive !== null || b.source.kind === "dml") return "materialize";
    if (b.hint === true) return "materialize";
    if (b.hint === false) return "inline";
    return b.refs >= 2 ? "materialize" : "inline";
  });
}

// addOutcomeCost adds extra cost to an outcome (the writable-CTE orchestrator folds the
// materialization cost of the data-modifying / query CTEs into the primary's result —
// spec/design/writable-cte.md §8).
function addOutcomeCost(outcome: Outcome, extra: bigint): Outcome {
  return { ...outcome, cost: outcome.cost + extra };
}

// countCteRefsDml counts references to CTE `name` reachable through a cte_body's inner queries — the
// writable-CTE analogue of countSelfRefsQuery (spec/design/writable-cte.md §3). A query body delegates
// to the query counter; a data-modifying body counts the references in its source query / WHERE / SET
// RHSs / ON CONFLICT / RETURNING sublinks. Used by the orchestrator to count the references a
// NON-planned data-modifying part contributes to the inline-vs-materialize decision.
function countCteRefsDml(body: CteBody, name: string): number {
  if (body.kind === "select" || body.kind === "setOp" || body.kind === "withExpr") {
    return countSelfRefsQuery(body, name);
  }
  if (body.kind === "insert") {
    let n =
      body.source.kind === "select"
        ? countSelfRefsSelect(body.source.select, name)
        : // VALUES slots hold literals / params / ROW / ARRAY (no sublinks this slice).
          0;
    if (body.onConflict !== null && body.onConflict.doUpdate) {
      for (const a of body.onConflict.assignments) n += countSelfRefsExpr(a.value, name);
      if (body.onConflict.filter !== null) n += countSelfRefsExpr(body.onConflict.filter, name);
    }
    return n + countReturningRefs(body.returning, name);
  }
  if (body.kind === "update") {
    let n = 0;
    for (const a of body.assignments) n += countSelfRefsExpr(a.value, name);
    if (body.filter !== null) n += countSelfRefsExpr(body.filter, name);
    return n + countReturningRefs(body.returning, name);
  }
  // delete
  let n = 0;
  if (body.filter !== null) n += countSelfRefsExpr(body.filter, name);
  return n + countReturningRefs(body.returning, name);
}

// countReturningRefs counts references to CTE `name` in a RETURNING item list's sublinks.
function countReturningRefs(returning: SelectItems | null, name: string): number {
  if (returning === null || returning.kind !== "list") return 0;
  return returning.items.reduce((a, it) => a + countSelfRefsExpr(it.expr, name), 0);
}

// countSelfRefsQuery counts self-references to name anywhere in a query expression (deep — FROM
// relations at every nesting level plus expression sublinks).
function countSelfRefsQuery(qe: QueryExpr, name: string): number {
  if (qe.kind === "select") return countSelfRefsSelect(qe, name);
  // A nested WITH establishes its own CTE scope (spec/design/cte.md §7): an enclosing CTE name is
  // NOT visible inside it (a reference there resolves to a base table / the nested CTE, never the
  // enclosing one), so it contributes no self-reference to the enclosing name.
  if (qe.kind === "withExpr") return 0;
  return countSelfRefsQuery(qe.lhs, name) + countSelfRefsQuery(qe.rhs, name);
}

// countSelfRefsSelect counts self-references in a SELECT: its FROM relations (deep) plus all of its
// expressions' sublinks.
function countSelfRefsSelect(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) n += countSelfRefsTableref(tref, name);
  for (const e of selectExprs(s)) n += countSelfRefsExpr(e, name);
  return n;
}

// countSelfRefsTableref counts self-references reachable through one FROM relation: a plain table
// reference with the matching name (+1), a derived-table subquery (recurse), or a table-function's
// / VALUES' argument exprs.
function countSelfRefsTableref(tref: TableRef, name: string): number {
  if (isPlainRelation(tref)) return tref.name.toLowerCase() === name ? 1 : 0;
  let n = 0;
  if (tref.subquery !== undefined) n += countSelfRefsQuery(tref.subquery, name);
  if (tref.args !== null && tref.args !== undefined) {
    for (const a of tref.args) n += countSelfRefsExpr(a, name);
  }
  if (tref.values !== undefined) {
    for (const row of tref.values) for (const e of row) n += countSelfRefsExpr(e, name);
  }
  return n;
}

// countSelfRefsExpr counts self-references inside an expression — only reachable through a sublink
// (a subquery is an independent query whose own FROM may reference the CTE). The walk is exhaustive
// (like exprHasAggregate).
function countSelfRefsExpr(e: Expr, name: string): number {
  switch (e.kind) {
    case "scalarSubquery":
    case "exists":
      return countSelfRefsQuery(e.query, name);
    case "inSubquery":
    case "quantifiedSubquery":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsQuery(e.query, name);
    case "cast":
    case "collate":
      return countSelfRefsExpr(e.inner, name);
    case "extract":
      return countSelfRefsExpr(e.source, name);
    case "unary":
    case "isNull":
      return countSelfRefsExpr(e.operand, name);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsExpr(e.rhs, name);
    case "in":
      return (
        countSelfRefsExpr(e.lhs, name) + e.list.reduce((a, x) => a + countSelfRefsExpr(x, name), 0)
      );
    case "between":
      return (
        countSelfRefsExpr(e.lhs, name) +
        countSelfRefsExpr(e.lo, name) +
        countSelfRefsExpr(e.hi, name)
      );
    case "case":
      return (
        (e.operand !== null ? countSelfRefsExpr(e.operand, name) : 0) +
        e.whens.reduce(
          (a, w) => a + countSelfRefsExpr(w.cond, name) + countSelfRefsExpr(w.result, name),
          0,
        ) +
        (e.els !== null ? countSelfRefsExpr(e.els, name) : 0)
      );
    case "funcCall":
      return e.args.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "row":
      return e.fields.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "array":
      return e.elements.reduce((a, x) => a + countSelfRefsExpr(x, name), 0);
    case "fieldAccess":
    case "fieldStar":
      return countSelfRefsExpr(e.base, name);
    case "subscript":
      return (
        countSelfRefsExpr(e.base, name) +
        astSubscriptExprs(e.subscripts).reduce((a, x) => a + countSelfRefsExpr(x, name), 0)
      );
    case "quantified":
      return countSelfRefsExpr(e.lhs, name) + countSelfRefsExpr(e.array, name);
    default:
      return 0;
  }
}

// countDirectFromSelfRefs counts self-references that are DIRECT FROM/JOIN relations of this SELECT
// (a plain table ref matching the name). This is the only valid position for a recursive reference.
function countDirectFromSelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) {
    if (isPlainRelation(tref) && tref.name.toLowerCase() === name) n++;
  }
  return n;
}

// countFromSubquerySelfRefs counts self-references nested inside a FROM-position subquery /
// table-function args / VALUES of this SELECT (the deferred 0A000 shape).
function countFromSubquerySelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const tref of fromRelations(s)) {
    if (!isPlainRelation(tref)) n += countSelfRefsTableref(tref, name);
  }
  return n;
}

// countSublinkSelfRefs counts self-references reachable only through an expression sublink in this
// SELECT's top-level expressions — the `within a subquery` position.
function countSublinkSelfRefs(s: Select, name: string): number {
  let n = 0;
  for (const e of selectExprs(s)) n += countSelfRefsExpr(e, name);
  return n;
}

// directSelfRefOnNullableSide reports whether the SELECT's single direct self-reference sits on the
// NULLABLE side of an outer join — the position PostgreSQL rejects. The FROM is a left-deep chain:
// relation 0 is `from`, relation i+1 is joins[i].table, combined by joins[i].kind. A LEFT/FULL join
// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
function directSelfRefOnNullableSide(s: Select, name: string): boolean {
  const rels = fromRelations(s);
  const nullable = new Array<boolean>(rels.length).fill(false);
  for (let j = 0; j < s.joins.length; j++) {
    const right = j + 1;
    switch (s.joins[j]!.kind) {
      case "left":
        nullable[right] = true;
        break;
      case "right":
        for (let i = 0; i <= j; i++) nullable[i] = true;
        break;
      case "full":
        for (let i = 0; i <= right; i++) nullable[i] = true;
        break;
      default:
        break;
    }
  }
  return rels.some(
    (tref, i) => isPlainRelation(tref) && tref.name.toLowerCase() === name && nullable[i]!,
  );
}

// isPlainRelation reports whether a FROM relation is a plain table NAME — not a derived-table
// subquery, a table function, or a VALUES body. Only a plain relation can resolve to a CTE.
function isPlainRelation(tref: TableRef): boolean {
  return (
    (tref.args === null || tref.args === undefined) &&
    tref.subquery === undefined &&
    tref.values === undefined
  );
}

// fromRelations returns the FROM relations of a SELECT in left-deep order: from (if present) then
// each join's table.
function fromRelations(s: Select): TableRef[] {
  const rels: TableRef[] = [];
  if (s.from !== null) rels.push(s.from);
  for (const j of s.joins) rels.push(j.table);
  return rels;
}

// selectExprs returns every top-level expression of a SELECT that can hold a sublink (select items,
// WHERE, GROUP BY, HAVING, join ON conditions). ORDER BY keys are bare/qualified column references
// (never expressions), so they carry no sublink.
function selectExprs(s: Select): Expr[] {
  const v: Expr[] = [];
  if (s.items.kind === "list") for (const it of s.items.items) v.push(it.expr);
  if (s.filter !== null) v.push(s.filter);
  for (const g of s.groupBy) v.push(g);
  if (s.having !== null) v.push(s.having);
  for (const j of s.joins) if (j.on !== null) v.push(j.on);
  return v;
}

// checkRecursiveColumnTypes checks a recursive CTE's column types (recursive-cte.md §2): the output
// types are FIXED by the non-recursive (anchor) term, and the recursive term's columns must be
// assignable to them — a literal adapts, an equal type passes, a WIDER type is 42804 (matching
// PostgreSQL). Mechanically the would-be UNION unified type must EQUAL the anchor type; any widening
// of the anchor is the error. An arity mismatch is 42601, like a plain UNION.
function checkRecursiveColumnTypes(anchor: QueryPlan, recursive: QueryPlan, name: string): void {
  const a = anchor.columnTypes;
  const r = recursive.columnTypes;
  if (a.length !== r.length) {
    throw engineError("syntax_error", "each UNION query must have the same number of columns");
  }
  for (let i = 0; i < a.length; i++) {
    const unified = unifySetopColumn(a[i]!, r[i]!, "union");
    if (rtName(unified) !== rtName(a[i]!)) {
      throw engineError(
        "datatype_mismatch",
        `recursive query "${name}" column ${i + 1} has type ${rtName(a[i]!)} in non-recursive term but type ${rtName(unified)} overall`,
      );
    }
  }
}

// cteSyntheticTable builds the synthetic relation a CTE reference resolves against
// (spec/design/cte.md §2): one column per body output, named by the rename list (a count mismatch
// with MORE aliases is 42P10) or the body's own output names, typed from the planned body. The
// relation has no primary key / constraints — it is read-only and its rows come from the CTE
// context, never a store.
function cteSyntheticTable(name: string, plan: QueryPlan, rename: string[] | null): Table {
  return cteSyntheticTableCols(name, plan.columnNames, plan.columnTypes, rename);
}

// cteSyntheticTableCols is the shared core of cteSyntheticTable, over explicit body column names +
// types — so a data-modifying CTE (whose "body output" is its RETURNING projection, not a QueryPlan)
// builds its synthetic relation the same way (spec/design/writable-cte.md §1).
function cteSyntheticTableCols(
  name: string,
  bodyNames: string[],
  bodyTypes: ResolvedType[],
  rename: string[] | null,
): Table {
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
    identity: null,
    collation: null,
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
      throw engineError(
        "feature_not_supported",
        "an anonymous composite column in a CTE is not supported yet",
      );
    case "array":
      return arrayT(typeFromResolved(rt.elem));
    case "range":
      // A range-typed CTE column is deferred (range columns are not storable yet — R2); the value
      // itself works in expression position, just not as a materialized column type.
      throw engineError("feature_not_supported", "a range column in a CTE is not supported yet");
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

// buildSequenceDef resolves a parsed SeqOptions set into a validated SequenceDef
// (spec/design/sequences.md §1/§14), shared by CREATE SEQUENCE and an IDENTITY column's
// `( seq_options )` (§13). The AS type (or the serial/identity-supplied default) sets the default +
// validated bounds; then validates INCREMENT (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type
// range, MINVALUE ≤ MAXVALUE, and START in [min, max] (each 22023); a fresh sequence starts with
// lastValue = start, isCalled = false. ownedBy carries the IDENTITY / serial owner link (undefined
// for a plain CREATE SEQUENCE).
function buildSequenceDef(
  name: string,
  options: SeqOptions,
  ownedBy: SeqOwner | undefined,
): SequenceDef {
  // The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
  let dtype: SeqDataType = "bigint";
  if (options.dataType !== null) {
    const dt = seqDataTypeFromName(options.dataType);
    if (dt === undefined) {
      throw engineError(
        "invalid_parameter_value",
        "sequence type must be smallint, integer, or bigint",
      );
    }
    dtype = dt;
  }
  const [typeMin, typeMax] = seqDataTypeRange(dtype);
  const increment = options.increment ?? 1n;
  if (increment === 0n) {
    throw engineError("invalid_parameter_value", "INCREMENT must not be zero");
  }
  const cache = options.cache ?? 1n;
  if (cache < 1n) {
    throw engineError("invalid_parameter_value", `CACHE (${cache}) must be greater than zero`);
  }
  const [defMin, defMax] = seqDataTypeDefaultBounds(dtype, increment);
  // An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
  // BEFORE the MIN > MAX consistency check (§14.2).
  if (
    options.maxValue !== null &&
    options.maxValue.value !== null &&
    options.maxValue.value > typeMax
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MAXVALUE (${options.maxValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  if (
    options.minValue !== null &&
    options.minValue.value !== null &&
    options.minValue.value < typeMin
  ) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${options.minValue.value}) is out of range for sequence data type ${seqDataTypePgName(dtype)}`,
    );
  }
  // `{ value: v }` MINVALUE v / `{ value: null }` NO MINVALUE / outer null unset → the type default.
  const minValue =
    options.minValue !== null && options.minValue.value !== null ? options.minValue.value : defMin;
  const maxValue =
    options.maxValue !== null && options.maxValue.value !== null ? options.maxValue.value : defMax;
  // PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
  // previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
  if (minValue >= maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${minValue}) must be less than MAXVALUE (${maxValue})`,
    );
  }
  // START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
  const start = options.start ?? (increment < 0n ? maxValue : minValue);
  seqBoundCheckStart(start, minValue, maxValue);
  return {
    name,
    increment,
    minValue,
    maxValue,
    start,
    cache,
    cycle: options.cycle ?? false,
    lastValue: start,
    isCalled: false,
    ownedBy,
  };
}

// seqBoundCheckStart is PG's START-in-bounds cross-check (init_params): start ∈ [min, max], else
// 22023 with PG's wording. Shared by CREATE (buildSequenceDef) and ALTER (applySeqAlter).
function seqBoundCheckStart(start: bigint, minValue: bigint, maxValue: bigint): void {
  if (start < minValue) {
    throw engineError(
      "invalid_parameter_value",
      `START value (${start}) cannot be less than MINVALUE (${minValue})`,
    );
  }
  if (start > maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `START value (${start}) cannot be greater than MAXVALUE (${maxValue})`,
    );
  }
}

// seqBoundCheckLast is PG's last_value (RESTART) cross-check (init_params): the post-edit last_value ∈
// [min, max], else 22023. PG uses the "RESTART value …" wording even with no RESTART written (§15.2).
function seqBoundCheckLast(lastValue: bigint, minValue: bigint, maxValue: bigint): void {
  if (lastValue < minValue) {
    throw engineError(
      "invalid_parameter_value",
      `RESTART value (${lastValue}) cannot be less than MINVALUE (${minValue})`,
    );
  }
  if (lastValue > maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `RESTART value (${lastValue}) cannot be greater than MAXVALUE (${maxValue})`,
    );
  }
}

// applySeqAlter re-edits an existing SequenceDef per ALTER SEQUENCE s <options>
// (spec/design/sequences.md §15.2) — PG init_params with isInit=false. Only the WRITTEN options
// change; lastValue/isCalled are preserved unless restart is given. The value type is not persisted
// (§14.4), so NO MINVALUE/NO MAXVALUE reset the open direction to the bigint bound and an explicit
// bound is i64-checked only. options.dataType must be null (the caller rejects AS as 0A000 first).
function applySeqAlter(
  existing: SequenceDef,
  options: SeqOptions,
  restart: SeqRestart | null,
): SequenceDef {
  const def = { ...existing };
  if (options.increment !== null) {
    if (options.increment === 0n) {
      throw engineError("invalid_parameter_value", "INCREMENT must not be zero");
    }
    def.increment = options.increment;
  }
  if (options.cache !== null) {
    if (options.cache < 1n) {
      throw engineError(
        "invalid_parameter_value",
        `CACHE (${options.cache}) must be greater than zero`,
      );
    }
    def.cache = options.cache;
  }
  // NO MINVALUE/NO MAXVALUE recompute the default for the (possibly new) INCREMENT sign — against the
  // bigint range (the value type is not persisted, §14.4). An explicit bound is taken as written; an
  // unwritten bound is preserved (PG keeps it even when the sign flips).
  const [defMin, defMax] = seqDataTypeDefaultBounds("bigint", def.increment);
  if (options.minValue !== null) {
    def.minValue = options.minValue.value === null ? defMin : options.minValue.value;
  }
  if (options.maxValue !== null) {
    def.maxValue = options.maxValue.value === null ? defMax : options.maxValue.value;
  }
  if (def.minValue >= def.maxValue) {
    throw engineError(
      "invalid_parameter_value",
      `MINVALUE (${def.minValue}) must be less than MAXVALUE (${def.maxValue})`,
    );
  }
  if (options.start !== null) def.start = options.start;
  // Cross-check 1: START ∈ [min, max].
  seqBoundCheckStart(def.start, def.minValue, def.maxValue);
  // RESTART (applied last, before the last_value cross-check).
  if (restart !== null) {
    def.lastValue = restart.toStart ? def.start : restart.value;
    def.isCalled = false;
  }
  // Cross-check 2: the preserved/restarted last_value ∈ [min, max].
  seqBoundCheckLast(def.lastValue, def.minValue, def.maxValue);
  if (options.cycle !== null) def.cycle = options.cycle;
  return def;
}

// serialPseudoType maps a serial pseudo-type name to its underlying integer scalar
// (spec/design/sequences.md §12) — serial/serial4 → i32, bigserial/serial8 → i64,
// smallserial/serial2 → i16. undefined for any other name. Recognized only in a CREATE TABLE
// column-type position; the match is case-insensitive.
function serialPseudoType(name: string): ScalarType | undefined {
  switch (name.toLowerCase()) {
    case "serial":
    case "serial4":
      return "i32";
    case "bigserial":
    case "serial8":
      return "i64";
    case "smallserial":
    case "serial2":
      return "i16";
    default:
      return undefined;
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
    // A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE effects
    // — writable-cte.md): it must take the write gate, accumulate into working, and commit.
    (stmt.kind === "with" && withHasDml(stmt)) ||
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
      return (
        stmt.ctes.some((c) => cteBodyCallsSeqMutator(c.body)) || cteBodyCallsSeqMutator(stmt.body)
      );
    default:
      return false;
  }
}

// cteBodyCallsSeqMutator reports whether a cte_body calls a sequence-mutating function. A query body
// delegates to the query walk; a data-modifying body already makes the WITH a write (via withHasDml),
// so this is not reached for it — it is treated as a write regardless (writable-cte.md).
function cteBodyCallsSeqMutator(body: CteBody): boolean {
  const q = cteBodyAsQuery(body);
  return q !== null ? queryCallsSeqMutator(q) : true;
}

function queryCallsSeqMutator(qe: QueryExpr): boolean {
  if (qe.kind === "setOp") return setOpCallsSeqMutator(qe);
  if (qe.kind === "withExpr") {
    // A nested WITH's CTE bodies and main body may call a sequence mutator (cte.md §7).
    for (const c of qe.ctes) if (cteBodyCallsSeqMutator(c.body)) return true;
    return queryCallsSeqMutator(qe.body);
  }
  return selectCallsSeqMutator(qe);
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
    case "extract":
      return exprCallsSeqMutator(e.source);
    case "collate":
      return exprCallsSeqMutator(e.inner);
    case "unary":
    case "isNull":
      return exprCallsSeqMutator(e.operand);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.rhs);
    case "in":
      return exprCallsSeqMutator(e.lhs) || e.list.some(exprCallsSeqMutator);
    case "between":
      return exprCallsSeqMutator(e.lhs) || exprCallsSeqMutator(e.lo) || exprCallsSeqMutator(e.hi);
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

// PrivReq is the privilege requirements collected from one statement (spec/design/session.md §5.3):
// the per-table privileges (each (table, privilege) pair), the named functions (each needs EXECUTE),
// and whether the statement is DDL (gated by allowDdl). Collected by an exhaustive AST walk
// (mirroring exprCallsSeqMutator).
type PrivReq = {
  tables: { name: string; priv: Privilege }[];
  functions: string[];
  isDdl: boolean;
  // isTempDdl is whether the DDL targets a SESSION-LOCAL temporary table (CREATE TEMP TABLE) — gated
  // by allowTempDdl instead of allowDdl (spec/design/temp-tables.md §5). Set only for a session-local
  // CREATE TEMP (a SHARED create sets isSharedTempDdl); a DROP is classified by resolving the name.
  isTempDdl: boolean;
  // isSharedTempDdl is whether the DDL targets a DATABASE-WIDE shared temporary table (CREATE SHARED
  // TEMP TABLE) — gated by allowSharedTempDdl (temp-tables.md §5). A DROP of a shared temp table is
  // classified by resolving the name.
  isSharedTempDdl: boolean;
};

// collectStmtPrivs collects the privilege requirements of stmt (spec/design/session.md §5.3).
// Transaction control carries none (handled before dispatch); DDL just sets isDdl.
function collectStmtPrivs(stmt: Statement, req: PrivReq): void {
  const locals = new Set<string>();
  switch (stmt.kind) {
    case "createTable":
      req.isDdl = true;
      // A temp table's DDL is gated by the temp-scoped split of allowDdl (temp-tables.md §5):
      // allowSharedTempDdl for a SHARED table, allowTempDdl for a session-local one.
      req.isSharedTempDdl = stmt.shared;
      req.isTempDdl = stmt.temp && !stmt.shared;
      break;
    case "dropTable":
    case "createIndex":
    case "dropIndex":
    case "createType":
    case "dropType":
    case "createSequence":
    case "dropSequence":
    case "alterSequence":
      req.isDdl = true;
      break;
    case "insert":
      collectInsertPrivs(stmt, req, locals);
      break;
    case "select":
      collectSelectPrivs(stmt, req, locals);
      break;
    case "setOp":
      collectSetOpPrivs(stmt, req, locals);
      break;
    case "with":
      collectWithPrivs(stmt, req, locals);
      break;
    case "update":
      collectUpdatePrivs(stmt, req, locals);
      break;
    case "delete":
      collectDeletePrivs(stmt, req, locals);
      break;
    default:
      // Transaction control (begin/commit/rollback) carries no privilege requirement.
      break;
  }
}

function collectInsertPrivs(ins: Insert, req: PrivReq, locals: Set<string>): void {
  // The write target needs INSERT. A bare INSERT … VALUES reads nothing (the slots are literals /
  // params), so it needs only INSERT; an INSERT … SELECT source needs SELECT on its tables.
  req.tables.push({ name: ins.table, priv: "insert" });
  if (ins.source.kind === "select") {
    collectSelectPrivs(ins.source.select, req, locals);
  }
  if (ins.onConflict !== null && ins.onConflict.doUpdate) {
    for (const a of ins.onConflict.assignments) collectExprPrivs(a.value, req, locals);
    if (ins.onConflict.filter !== null) collectExprPrivs(ins.onConflict.filter, req, locals);
  }
  collectItemsPrivs(ins.returning, req, locals);
}

function collectUpdatePrivs(upd: Update, req: PrivReq, locals: Set<string>): void {
  req.tables.push({ name: upd.table, priv: "update" });
  // SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
  // referencing assignment RHS (a constant-only SET a = 1 with no WHERE/RETURNING reads nothing).
  const reads =
    upd.filter !== null ||
    upd.returning !== null ||
    upd.assignments.some((a) => exprReadsColumns(a.value));
  if (reads) req.tables.push({ name: upd.table, priv: "select" });
  for (const a of upd.assignments) collectExprPrivs(a.value, req, locals);
  if (upd.filter !== null) collectExprPrivs(upd.filter, req, locals);
  collectItemsPrivs(upd.returning, req, locals);
}

function collectDeletePrivs(del: Delete, req: PrivReq, locals: Set<string>): void {
  req.tables.push({ name: del.table, priv: "delete" });
  // DELETE reads the target's columns through a WHERE or a RETURNING.
  if (del.filter !== null || del.returning !== null) {
    req.tables.push({ name: del.table, priv: "select" });
  }
  if (del.filter !== null) collectExprPrivs(del.filter, req, locals);
  collectItemsPrivs(del.returning, req, locals);
}

function collectQueryPrivs(qe: QueryExpr, req: PrivReq, locals: Set<string>): void {
  if (qe.kind === "setOp") collectSetOpPrivs(qe, req, locals);
  else if (qe.kind === "withExpr") {
    // A nested WITH establishes its own CTE scope (spec/design/cte.md §7): the enclosing locals are
    // NOT inherited (an enclosing CTE name resolves to a base table inside, so it is
    // privilege-checked), and the nested CTE names shadow base tables only within this node.
    const scope = new Set<string>();
    for (const cte of qe.ctes) {
      collectCteBodyPrivs(cte.body, req, scope);
      scope.add(cte.name.toLowerCase());
    }
    collectQueryPrivs(qe.body, req, scope);
  } else collectSelectPrivs(qe, req, locals);
}

function collectSetOpPrivs(so: SetOp, req: PrivReq, locals: Set<string>): void {
  collectQueryPrivs(so.lhs, req, locals);
  collectQueryPrivs(so.rhs, req, locals);
}

function collectWithPrivs(wq: WithQuery, req: PrivReq, locals: Set<string>): void {
  // A CTE name shadows a base table inside the WITH (a FROM <cte> is not a catalog object), so it is
  // added to the local scope and never privilege-checked. Forward-only visibility: each CTE body sees
  // the CTE names declared before it. A data-modifying body / primary needs the write privilege on
  // its target table (writable-cte.md).
  const scope = new Set(locals);
  for (const cte of wq.ctes) {
    collectCteBodyPrivs(cte.body, req, scope);
    scope.add(cte.name.toLowerCase());
  }
  collectCteBodyPrivs(wq.body, req, scope);
}

// collectCteBodyPrivs collects the privilege requirements of a cte_body — a query, or a
// data-modifying statement (spec/design/writable-cte.md) which needs the write privilege on its
// target.
function collectCteBodyPrivs(body: CteBody, req: PrivReq, locals: Set<string>): void {
  if (body.kind === "insert") collectInsertPrivs(body, req, locals);
  else if (body.kind === "update") collectUpdatePrivs(body, req, locals);
  else if (body.kind === "delete") collectDeletePrivs(body, req, locals);
  else collectQueryPrivs(body, req, locals);
}

function collectSelectPrivs(s: Select, req: PrivReq, locals: Set<string>): void {
  if (s.from !== null) collectTableRefPrivs(s.from, req, locals);
  for (const j of s.joins) {
    collectTableRefPrivs(j.table, req, locals);
    if (j.on !== null) collectExprPrivs(j.on, req, locals);
  }
  if (s.items.kind === "list") {
    for (const it of s.items.items) collectExprPrivs(it.expr, req, locals);
  }
  if (s.filter !== null) collectExprPrivs(s.filter, req, locals);
  for (const g of s.groupBy) collectExprPrivs(g, req, locals);
  if (s.having !== null) collectExprPrivs(s.having, req, locals);
}

function collectTableRefPrivs(t: TableRef, req: PrivReq, locals: Set<string>): void {
  if (t.args !== null) {
    // A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
    req.functions.push(t.name);
    for (const a of t.args) collectExprPrivs(a, req, locals);
  } else if (t.subquery !== undefined) {
    collectQueryPrivs(t.subquery, req, locals);
  } else if (t.values !== undefined) {
    for (const row of t.values) for (const e of row) collectExprPrivs(e, req, locals);
  } else if (!locals.has(t.name.toLowerCase())) {
    // A base-table reference (not a CTE / derived-table label) — needs SELECT.
    req.tables.push({ name: t.name, priv: "select" });
  }
}

function collectItemsPrivs(items: SelectItems | null, req: PrivReq, locals: Set<string>): void {
  if (items !== null && items.kind === "list") {
    for (const it of items.items) collectExprPrivs(it.expr, req, locals);
  }
}

// collectExprPrivs is exhaustive over Expr (mirroring exprCallsSeqMutator): collect every named
// function call (EXECUTE) and walk every subquery (its tables need SELECT).
function collectExprPrivs(e: Expr, req: PrivReq, locals: Set<string>): void {
  switch (e.kind) {
    case "funcCall":
      req.functions.push(e.name);
      for (const a of e.args) collectExprPrivs(a, req, locals);
      break;
    case "column":
    case "qualifiedColumn":
    case "literal":
    case "typedLiteral":
    case "param":
      break;
    case "row":
      for (const f of e.fields) collectExprPrivs(f, req, locals);
      break;
    case "array":
      for (const el of e.elements) collectExprPrivs(el, req, locals);
      break;
    case "fieldAccess":
    case "fieldStar":
      collectExprPrivs(e.base, req, locals);
      break;
    case "subscript":
      collectExprPrivs(e.base, req, locals);
      for (const s of e.subscripts) {
        if (s.isSlice) {
          if (s.lower !== null) collectExprPrivs(s.lower, req, locals);
          if (s.upper !== null) collectExprPrivs(s.upper, req, locals);
        } else {
          collectExprPrivs(s.index, req, locals);
        }
      }
      break;
    case "cast":
    case "collate":
      collectExprPrivs(e.inner, req, locals);
      break;
    case "extract":
      collectExprPrivs(e.source, req, locals);
      break;
    case "unary":
    case "isNull":
      collectExprPrivs(e.operand, req, locals);
      break;
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.rhs, req, locals);
      break;
    case "in":
      collectExprPrivs(e.lhs, req, locals);
      for (const x of e.list) collectExprPrivs(x, req, locals);
      break;
    case "between":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.lo, req, locals);
      collectExprPrivs(e.hi, req, locals);
      break;
    case "case":
      if (e.operand !== null) collectExprPrivs(e.operand, req, locals);
      for (const w of e.whens) {
        collectExprPrivs(w.cond, req, locals);
        collectExprPrivs(w.result, req, locals);
      }
      if (e.els !== null) collectExprPrivs(e.els, req, locals);
      break;
    case "scalarSubquery":
    case "exists":
      collectQueryPrivs(e.query, req, locals);
      break;
    case "inSubquery":
    case "quantifiedSubquery":
      collectExprPrivs(e.lhs, req, locals);
      collectQueryPrivs(e.query, req, locals);
      break;
    case "quantified":
      collectExprPrivs(e.lhs, req, locals);
      collectExprPrivs(e.array, req, locals);
      break;
  }
}

// exprReadsColumns reports whether e reads a stored column or a subquery's rows — the trigger for an
// UPDATE's SELECT requirement on its target (spec/design/session.md §5.3). A column reference or any
// subquery counts; a pure constant / parameter expression does not. Exhaustive over Expr.
function exprReadsColumns(e: Expr): boolean {
  switch (e.kind) {
    case "column":
    case "qualifiedColumn":
      return true;
    case "scalarSubquery":
    case "exists":
    case "inSubquery":
    case "quantifiedSubquery":
      return true;
    case "literal":
    case "typedLiteral":
    case "param":
      return false;
    case "row":
      return e.fields.some(exprReadsColumns);
    case "array":
      return e.elements.some(exprReadsColumns);
    case "fieldAccess":
    case "fieldStar":
      return exprReadsColumns(e.base);
    case "subscript":
      return (
        exprReadsColumns(e.base) ||
        e.subscripts.some((s) =>
          s.isSlice
            ? (s.lower !== null && exprReadsColumns(s.lower)) ||
              (s.upper !== null && exprReadsColumns(s.upper))
            : exprReadsColumns(s.index),
        )
      );
    case "cast":
    case "collate":
      return exprReadsColumns(e.inner);
    case "extract":
      return exprReadsColumns(e.source);
    case "unary":
    case "isNull":
      return exprReadsColumns(e.operand);
    case "funcCall":
      return e.args.some(exprReadsColumns);
    case "binary":
    case "isDistinct":
    case "like":
    case "regex":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.rhs);
    case "in":
      return exprReadsColumns(e.lhs) || e.list.some(exprReadsColumns);
    case "between":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.lo) || exprReadsColumns(e.hi);
    case "case":
      return (
        (e.operand !== null && exprReadsColumns(e.operand)) ||
        e.whens.some((w) => exprReadsColumns(w.cond) || exprReadsColumns(w.result)) ||
        (e.els !== null && exprReadsColumns(e.els))
      );
    case "quantified":
      return exprReadsColumns(e.lhs) || exprReadsColumns(e.array);
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
    return {
      kind: "query",
      columnNames: retNames,
      columnTypes: retTypes ?? [],
      rows: returned ?? [],
      cost,
    };
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
          "column notation .* applied to type " +
            rtName(baseType) +
            ", which is not a composite type",
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
  const { node, type } = resolve(
    scope,
    e,
    null,
    { collecting: false, groupKeys: [], specs: [] },
    params,
  );
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
  return {
    node: { kind: "outerColumn", level: r.level, index: r.index },
    type: resolvedTypeOfCol(scope.columnOf(r).type, scope.catalog),
  };
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
      "column notation ." +
        field +
        " applied to type " +
        rtName(baseType) +
        ", which is not a composite type",
    );
  }
  const lower = field.toLowerCase();
  const idx = baseType.fields.findIndex((f) => f.name.toLowerCase() === lower);
  if (idx < 0) throw undefinedColumn(field);
  return {
    node: { kind: "field", base: baseNode, index: idx },
    type: baseType.fields[idx]!.type,
  };
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
    throw engineError(
      "feature_not_supported",
      "subqueries are only supported in a SELECT statement",
    );
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
      return {
        node: { kind: "row", fields: nodes },
        type: { kind: "composite", name: null, fields },
      };
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
        return {
          node: { kind: "array", elements: nodes, nested: true },
          type: common,
        };
      }
      return {
        node: { kind: "array", elements: nodes, nested: false },
        type: { kind: "array", elem: common },
      };
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
      throw engineError(
        "feature_not_supported",
        "row expansion (.*) is not supported in this context",
      );
    case "subscript": {
      // `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array (else
      // 42804). Each subscript bound is an integer (a literal adapts; a non-integer is 42804). If any
      // spec is a slice the result is the array type (a sub-array); otherwise the element type. OOB /
      // NULL → NULL is an evaluation-time rule, not a resolve error.
      const base = resolve(scope, e.base, null, ag, params);
      if (base.type.kind !== "array") {
        throw typeError(
          `cannot subscript a value of type ${rtName(base.type)}, which is not an array`,
        );
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
      return {
        node: {
          kind: "subscript",
          base: base.node,
          subscripts: rsubs,
          isSlice,
        },
        type,
      };
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
    case "funcCall": {
      // A trailing OVER makes this a window-function call (spec/design/window.md §5.1).
      if (e.over !== undefined && e.over !== null) {
        return resolveWindowCall(
          scope,
          { name: e.name, args: e.args, star: e.star, over: e.over },
          ag,
          params,
        );
      }
      // A window-only function (row_number/…) used WITHOUT OVER is 42809 (PG's wrong_object_type,
      // not the windowing_error 42P20 it uses for a window in WHERE — window.md §7, oracle-verified).
      if (isWindowOnlyName(e.name)) {
        throw engineError(
          "wrong_object_type",
          `window function ${e.name.toLowerCase()} requires an OVER clause`,
        );
      }
      return resolveFuncCall(scope, e, ag, params);
    }
    case "typedLiteral": {
      // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
      // `type 'string'`, equal to CAST('string' AS type) over a string-literal operand. Resolve the
      // type by name (unknown → 42704) and coerce the string to it at resolve, context-free. No
      // typmod rides on the literal (the parser's one-token lookahead admits none).
      // A composite type name (`addr '(Main,90210)'`) coerces the string via record_in
      // (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
      const ct = scope.catalog.compositeType(e.typeName);
      if (ct !== undefined) return coerceStringToComposite(e.text, ct, scope.catalog);
      // A range type name (`i32range '[1,5)'`, `int4range '…'`) coerces the string via range_in
      // against the element type (spec/design/ranges.md §5) — the same primitive as the cast.
      const rdesc = rangeByName(e.typeName);
      if (rdesc !== undefined) return coerceStringToRangeExpr(e.text, rdesc);
      const [target] = resolveTypeAndTypmod(e.typeName, null);
      return coerceStringLiteral(e.text, target, null);
    }
    case "literal":
      switch (e.literal.kind) {
        case "null":
          return { node: { kind: "constNull" }, type: { kind: "null" } };
        case "bool":
          return {
            node: { kind: "constBool", value: e.literal.value },
            type: { kind: "bool" },
          };
        case "text": {
          // A string literal is text by default (collation C). It adapts to a BYTEA or a UUID
          // context (types.md §6/§13/§14): decode the hex input (bytea) or the PG-flexible uuid
          // input (uuid) — 22P02 on malformed; any other context — including none — keeps it text.
          // A string literal is text by default (collation C). It adapts to a BYTEA context
          // (decode the hex input, 22P02 on bad hex) or a TIMESTAMP/TIMESTAMPTZ context (parse
          // the datetime, 22007/22008 — spec/design/timestamp.md). Any other context keeps it text.
          if (ctx !== null && isBytea(ctx)) {
            return {
              node: {
                kind: "constBytea",
                value: decodeByteaLiteral(e.literal.text),
              },
              type: { kind: "bytea" },
            };
          }
          if (ctx !== null && isUuid(ctx)) {
            return {
              node: {
                kind: "constUuid",
                value: decodeUuidLiteral(e.literal.text),
              },
              type: { kind: "uuid" },
            };
          }
          if (ctx !== null && isTimestamp(ctx)) {
            return {
              node: {
                kind: "constTimestamp",
                value: parseTimestamp(e.literal.text),
              },
              type: { kind: "timestamp" },
            };
          }
          if (ctx !== null && isTimestamptz(ctx)) {
            return {
              node: {
                kind: "constTimestamptz",
                value: parseTimestamptz(e.literal.text),
              },
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
              node: {
                kind: "constInterval",
                value: parseInterval(e.literal.text),
              },
              type: { kind: "interval" },
            };
          }
          return {
            node: { kind: "constText", value: e.literal.text },
            type: { kind: "text" },
          };
        }
        case "decimal":
          // A decimal literal adapts to a FLOAT context (float.md §4): decimal → float at resolve
          // (the nearest binary64 to the exact decimal; Math.fround if the context is f32). The
          // exact-decimal string already round-trips IEEE conversion via Number(...). Otherwise it
          // is decimal — cap-checked here (an over-long coefficient/scale traps 22003 at resolve).
          if (ctx !== null && isFloat(ctx)) {
            return floatFromDecimalLiteral(e.literal.dec, ctx);
          }
          return {
            node: { kind: "constDecimal", value: e.literal.dec.checkCap() },
            type: { kind: "decimal" },
          };
        default: {
          // An integer literal adapts to an integer context or — like a decimal literal — a FLOAT
          // context (int → float at resolve; float.md §4). A non-numeric context (a text/decimal
          // column or assignment target) does not apply — it defaults to i64, and the surrounding
          // check then reports the family mismatch (42804) or widens it (int→decimal), never a wrong
          // range check.
          if (ctx !== null && isFloat(ctx)) {
            const n = roundToWidth(ctx, Number(e.literal.int));
            return {
              node: { kind: "constFloat", ty: ctx, value: n },
              type: { kind: "float", ty: ctx },
            };
          }
          const ty = ctx !== null && isInteger(ctx) ? ctx : "i64";
          if (!inRange(ty, e.literal.int)) throw overflow(ty);
          return {
            node: { kind: "constInt", value: e.literal.int },
            type: { kind: "int", ty },
          };
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
        node: {
          kind: "subquery",
          plan,
          subKind: "scalar",
          lhs: null,
          negated: false,
        },
        type: plan.columnTypes[0]!,
      };
    }
    case "exists": {
      // EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT EXISTS
      // parses as the unary NOT wrapping this, so negated here is always false.
      const plan = planSubquery(scope, e.query, params);
      return {
        node: {
          kind: "subquery",
          plan,
          subKind: "exists",
          lhs: null,
          negated: false,
        },
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
        node: {
          kind: "subquery",
          plan,
          subKind: "in",
          lhs,
          negated: e.negated,
        },
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
        node: {
          kind: "subquery",
          plan,
          subKind: "quantified",
          lhs,
          negated: false,
          op: e.op,
          all: e.all,
        },
        type: { kind: "bool" },
      };
    }
    case "collate": {
      // `expr COLLATE "name"` (spec/design/collation.md §1) — a postfix collation operator. Resolve
      // the inner expression, require a collatable (text) type (42804, PG-matching), and validate the
      // named collation exists ("C" or loaded, else 42704). A runtime PASSTHROUGH: a collation only
      // changes the ORDERING comparisons / ORDER BY, derived from the AST at those sites
      // (explicitCollation / OrderKey.collation), so resolving returns the inner node + type
      // unchanged. The hint flows through (COLLATE never changes the type).
      const r = resolve(scope, e.inner, ctx, ag, params);
      if (r.type.kind !== "text" && r.type.kind !== "null") {
        throw typeError(`collations are not supported by type ${rtName(r.type)}`);
      }
      resolveCollationName(scope.catalog, e.collation); // surfaces 42704 for an unknown name
      return r;
    }
    case "extract": {
      // EXTRACT(field FROM source) (timezones.md §9.2, grammar.md §50). The field is SYNTACTIC and
      // validated at RESOLVE (not per row): an unsupported field for the source type is 0A000, an
      // unrecognized field is 22023 — surfaced by probing the kernel with a zero value of the source's
      // family. The source must be a datetime type (else 42883); the result is numeric.
      const src = resolve(scope, e.source, null, ag, params);
      // A NULL source has no resolvable family; the value propagates to NULL at eval (the field is
      // not validated — a documented narrow edge vs. PG).
      if (src.type.kind !== "null") {
        let probe: ExtractSrc;
        switch (src.type.kind) {
          case "timestamp":
            probe = { kind: "ts", micros: 0n };
            break;
          case "timestamptz":
            probe = { kind: "tstz", instant: 0n, local: 0n, offsetSecs: 0n };
            break;
          case "date":
            probe = { kind: "date", days: 0n };
            break;
          case "interval":
            probe = { kind: "interval", iv: { months: 0, days: 0, micros: 0n } };
            break;
          default:
            throw engineError(
              "undefined_function",
              `function extract(text, ${rtName(src.type)}) does not exist`,
            );
        }
        extractField(e.field, probe); // validate field-for-type (0A000 / 22023); value discarded
      }
      return {
        node: { kind: "extract", field: e.field, value: src.node },
        type: { kind: "decimal" },
      };
    }
    case "cast": {
      // An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the string-literal
      // form `'{…}'::T[]` and a bare NULL; every other array cast (runtime text→array, array→text,
      // element-wise array→array) is a documented 0A000 narrowing.
      if (e.typeName.endsWith("[]")) {
        const base = e.typeName.slice(0, -2);
        if (e.typeMod !== null) {
          throw engineError(
            "feature_not_supported",
            "a type modifier on an array type is not supported yet",
          );
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
          return {
            node: valueToRExpr(val),
            type: { kind: "array", elem: elemRt },
          };
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "null") {
          return {
            node: { kind: "constNull" },
            type: { kind: "array", elem: elemRt },
          };
        }
        throw engineError(
          "feature_not_supported",
          "casting to an array type is only supported from a string literal this slice",
        );
      }
      // A range cast target (`'[1,5)'::i32range`, `…::int4range`). Like array, v1 supports the
      // string-literal form and a bare NULL; every other range cast is a 0A000 narrowing
      // (spec/design/ranges.md §1/§5).
      {
        const rdesc = rangeByName(e.typeName);
        if (rdesc !== undefined) {
          if (e.typeMod !== null) {
            throw engineError(
              "feature_not_supported",
              "a type modifier on a range type is not supported",
            );
          }
          const elemRt = resolvedTypeOf(elementScalar(rdesc));
          if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
            return coerceStringToRangeExpr(e.inner.literal.text, rdesc);
          }
          if (e.inner.kind === "literal" && e.inner.literal.kind === "null") {
            return {
              node: { kind: "constNull" },
              type: { kind: "range", elem: elemRt },
            };
          }
          throw engineError(
            "feature_not_supported",
            "casting to a range type is only supported from a string literal this slice",
          );
        }
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
          throw engineError(
            "feature_not_supported",
            "a type modifier is not supported on a composite type",
          );
        }
        if (e.inner.kind === "literal" && e.inner.literal.kind === "text") {
          return coerceStringToComposite(e.inner.literal.text, ct, scope.catalog);
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        if (inner.type.kind === "null") {
          return {
            node: inner.node,
            type: resolvedTypeOfCol({ kind: "composite", name: ct.name }, scope.catalog),
          };
        }
        // An identical named composite is the identity cast.
        if (
          inner.type.kind === "composite" &&
          inner.type.name?.toLowerCase() === ct.name.toLowerCase()
        ) {
          return inner;
        }
        throw engineError(
          "feature_not_supported",
          "casting to a composite type is only supported from a string literal",
        );
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
      // A boolean target (`CAST(x AS boolean)`, `x::boolean`) is the boolean cast slice
      // (spec/types/casts.toml, types.md §9). It needs the inner type to decide (only an i32 / NULL
      // / bool source is castable), so it is handled AFTER the inner is resolved, below.
      // bytea casts are likewise deferred (types.md §5/§13): casting TO bytea is 0A000.
      if (isBytea(target)) {
        throw engineError("feature_not_supported", "casting to bytea is not supported yet");
      }
      // uuid casts are likewise deferred (types.md §5/§14): casting TO uuid is 0A000.
      if (isUuid(target)) {
        throw engineError("feature_not_supported", "casting to uuid is not supported yet");
      }
      // Cross-family datetime casts (timezones.md §9.3): a timestamp/timestamptz/date TARGET from
      // another datetime family. A same-family cast is the identity; a cross-family cast becomes a
      // dateConvert node (the zone-crossing ones read the session zone at eval); any non-datetime
      // source is the deferred 0A000. A NULL operand adapts to the target. text↔datetime casts stay
      // deferred and fall through (a non-datetime source is rejected here).
      if (isTimestamp(target) || isTimestamptz(target) || isDate(target)) {
        if (e.inner.kind === "param") {
          const pinner = resolve(scope, e.inner, target, ag, params);
          return { node: pinner.node, type: resolvedTypeOf(target) };
        }
        const inner = resolve(scope, e.inner, null, ag, params);
        const toRt = resolvedTypeOf(target);
        const ik = inner.type.kind;
        if (ik === "null") return { node: inner.node, type: toRt };
        if (
          (ik === "timestamp" && isTimestamp(target)) ||
          (ik === "timestamptz" && isTimestamptz(target)) ||
          (ik === "date" && isDate(target))
        ) {
          return { node: inner.node, type: inner.type };
        }
        if (ik === "timestamp" || ik === "timestamptz" || ik === "date") {
          return { node: { kind: "dateConvert", inner: inner.node, to: target }, type: toRt };
        }
        throw engineError(
          "feature_not_supported",
          `cannot cast ${rtName(inner.type)} to ${canonicalName(target)}`,
        );
      }
      // interval casts are deferred (spec/design/interval.md): casting TO interval is 0A000.
      if (isInterval(target)) {
        throw engineError(
          "feature_not_supported",
          "casting to an interval type is not supported yet",
        );
      }
      // A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int` (and
      // `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
      // (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
      // context (its value is range-checked / coerced against target at eval), so changing the
      // context only for a parameter leaves all existing CAST behavior untouched.
      // A boolean target accepts only an i32 source (the boolean cast slice): an untyped integer
      // literal operand adapts to i32 (CAST(5 AS boolean) / 5::boolean), matching PG. A column/
      // expression keeps its own type; a literal beyond i32 range then traps 22003 (PG 42846 — a
      // documented divergence).
      const innerCtx = e.inner.kind === "param" ? target : isBool(target) ? "i32" : null;
      const inner = resolve(scope, e.inner, innerCtx, ag, params);
      // The boolean cast slice (spec/types/casts.toml, types.md §9): PG ties boolean↔integer to i32
      // ONLY and makes both directions explicit. A boolean TARGET takes an i32 / NULL / bool source
      // (the eval maps 0→false, nonzero→true); a boolean SOURCE produces an i32 (true→1, false→0).
      // Handled here, ahead of the generic numeric cast below — resultType assumes an int/decimal/
      // float target, so a boolean target must not fall through. A bool⇄i16 / bool⇄i64 pair is a
      // forbidden 42804 (jed's datatype-mismatch convention; PG reports 42846, casts.toml).
      if (isBool(target)) {
        if (
          (inner.type.kind === "int" && inner.type.ty === "i32") ||
          inner.type.kind === "null" ||
          inner.type.kind === "bool"
        ) {
          return {
            node: { kind: "cast", target, typmod, operand: inner.node },
            type: { kind: "bool" },
          };
        }
        throw typeError("cannot cast " + rtName(inner.type) + " to boolean");
      }
      if (inner.type.kind === "bool") {
        // boolean → i32 is the one boolean-source cast; any other target is forbidden (42804).
        if (target === "i32") {
          return {
            node: { kind: "cast", target, typmod, operand: inner.node },
            type: { kind: "int", ty: "i32" },
          };
        }
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
        throw engineError(
          "feature_not_supported",
          "casting from a timestamp type is not supported yet",
        );
      }
      // Casting FROM an interval is likewise deferred (0A000).
      if (inner.type.kind === "interval") {
        throw engineError(
          "feature_not_supported",
          "casting from an interval type is not supported yet",
        );
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
      return {
        node: { kind: "cast", target, typmod, operand: inner.node },
        type: resultType,
      };
    }
    case "unary":
      if (e.op === "neg") {
        const { node, type } = resolve(scope, e.operand, ctx, ag, params);
        if (type.kind === "decimal") {
          return {
            node: { kind: "neg", result: "decimal", operand: node },
            type: { kind: "decimal" },
          };
        }
        if (type.kind === "float") {
          // Unary minus on a float flips the sign bit (no overflow); a NaN/Inf operand passes
          // through per IEEE (-NaN is NaN, -Inf is -Inf) — float.md §5. result keeps the width.
          return {
            node: { kind: "neg", result: type.ty, operand: node },
            type: { kind: "float", ty: type.ty },
          };
        }
        if (type.kind === "interval") {
          // -interval (spec/design/interval.md §5).
          return {
            node: { kind: "neg", result: "interval", operand: node },
            type: { kind: "interval" },
          };
        }
        let result: ScalarType;
        if (type.kind === "int") result = type.ty;
        else if (type.kind === "null")
          result = "i64"; // -NULL = NULL
        else throw typeError("unary minus requires a numeric operand");
        return {
          node: { kind: "neg", result, operand: node },
          type: { kind: "int", ty: result },
        };
      }
      {
        const { node, type } = resolve(scope, e.operand, null, ag, params);
        requireBool(type, "NOT requires a boolean operand");
        return { node: { kind: "not", operand: node }, type: { kind: "bool" } };
      }
    case "isNull": {
      const { node } = resolve(scope, e.operand, null, ag, params);
      return {
        node: { kind: "isNull", operand: node, negated: e.negated },
        type: { kind: "bool" },
      };
    }
    case "isDistinct": {
      // NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a literal
      // adapts to its sibling; a text literal stays text), then require the operands be
      // comparable (both integer-ish or both text-ish; a mixed pair is 42804). The result
      // is always a definite boolean (functions.md §3).
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      classifyComparable(p.lt, p.rt);
      return {
        node: { kind: "distinct", lhs: p.rl, rhs: p.rr, negated: e.negated },
        type: { kind: "bool" },
      };
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
        return {
          node: { kind: "constBool", value: e.negated },
          type: { kind: "bool" },
        };
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
      // LIKE / ILIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
      // stays text), then require BOTH operands be text (or a bare NULL); a non-text operand is
      // 42804. We do NOT use classifyComparable here — it would wrongly accept bytea×bytea.
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      requireTextOrNull(p.lt);
      requireTextOrNull(p.rt);
      return {
        node: {
          kind: "like",
          lhs: p.rl,
          rhs: p.rr,
          negated: e.negated,
          insensitive: e.insensitive,
        },
        type: { kind: "bool" },
      };
    }
    case "regex": {
      // ~ / ~* / !~ / !~* — text×text → boolean (grammar.md §22b, regex.md). Same operand typing as
      // LIKE: resolve the pair, require both text (or a bare NULL); a non-text operand is 42804.
      const p = resolveOperandPair(scope, e.lhs, e.rhs, ag, params);
      requireTextOrNull(p.lt);
      requireTextOrNull(p.rt);
      // Precompile a CONSTANT pattern ONCE (regex.md §5); a non-constant pattern compiles per row at
      // eval. For ~* the constant is case-folded before compiling (the ILIKE mechanism). A malformed
      // pattern surfaces 2201B (and an oversized one 54001) here, at resolve, for the constant case.
      let program: RegexProgram | null = null;
      if (p.rr.kind === "constText") {
        const pat = e.insensitive ? foldLowerSimple(p.rr.value, loadedProperty()) : p.rr.value;
        program = compileRegex(pat);
      }
      return {
        node: {
          kind: "regex",
          lhs: p.rl,
          rhs: p.rr,
          negated: e.negated,
          insensitive: e.insensitive,
          program,
          compileCharged: false,
        },
        type: { kind: "bool" },
      };
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
          const eq: Expr = {
            kind: "binary",
            op: "eq",
            lhs: e.operand,
            rhs: w.cond,
          };
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
        node: {
          kind: "case",
          arms,
          els,
          coerceDecimal: unified.kind === "decimal",
        },
        type: unified,
      };
    }
  }
}

// resolveCollationName resolves a collation NAME to its table (spec/design/collation.md §1). C is the
// built-in byte / code-point order → null (the unchanged fast path); any other name resolves through
// the reference-only read path (the database's resolved set, then the binary's vendored set), else
// 42704.
function resolveCollationName(catalog: Database, name: string): Collation | null {
  if (name === "C") return null;
  const c = catalog.resolveCollationByName(name);
  if (c === undefined) {
    throw engineError("undefined_object", `collation "${name}" does not exist`);
  }
  return c;
}

// A text expression's collation derivation (spec/design/collation.md §1, PG's rules). "none" = no
// collation (a non-text expr or a bare literal); "implicit" = a column's frozen collation (C counts
// as a distinct implicit collation); "explicit" = an explicit COLLATE; "indeterminate" = two
// different implicit collations met — 42P22 when consumed.
type Deriv =
  | { kind: "none" }
  | { kind: "implicit"; name: string }
  | { kind: "explicit"; name: string }
  | { kind: "indeterminate" };

// deriveCollation derives the collation + derivation level of a (text) expression subtree. A COLLATE
// is explicit; a column reference is implicit (its frozen collation, C if none); || combines its
// operands. Every other shape resets to none (takes a neighbour's) — a documented narrowing (§14).
function deriveCollation(scope: Scope, e: Expr): Deriv {
  if (e.kind === "collate") return { kind: "explicit", name: e.collation };
  if (e.kind === "column") return columnDeriv(scope, () => scope.resolveBare(e.name));
  if (e.kind === "qualifiedColumn") {
    return columnDeriv(scope, () => scope.resolveQualified(e.qualifier, e.name));
  }
  if (e.kind === "binary" && e.op === "concat") {
    return combineDeriv(deriveCollation(scope, e.lhs), deriveCollation(scope, e.rhs));
  }
  return { kind: "none" };
}

// columnDeriv is the implicit derivation of a resolved column reference: a text column carries its
// frozen collation (C → "C", a distinct implicit collation); a non-text or unresolvable reference
// is "none".
function columnDeriv(scope: Scope, resolve: () => Resolved): Deriv {
  let col: Column;
  try {
    col = scope.columnOf(resolve());
  } catch {
    return { kind: "none" };
  }
  if (!typeIsText(col.type)) return { kind: "none" };
  return { kind: "implicit", name: col.collation ?? "C" };
}

// combineDeriv combines two operands' derivations (spec/design/collation.md §1/§7, PG's rules).
// Explicit dominates; two DIFFERENT explicit collations conflict eagerly (42P21); two different
// implicit collations yield "indeterminate" (deferred to 42P22 on use); explicit resolves it.
function combineDeriv(a: Deriv, b: Deriv): Deriv {
  if (a.kind === "explicit" && b.kind === "explicit") {
    if (a.name !== b.name) {
      throw engineError(
        "collation_mismatch",
        `collation mismatch between explicit collations "${a.name}" and "${b.name}"`,
      );
    }
    return a;
  }
  if (a.kind === "explicit") return a;
  if (b.kind === "explicit") return b;
  if (a.kind === "indeterminate" || b.kind === "indeterminate") {
    return { kind: "indeterminate" };
  }
  if (a.kind === "implicit" && b.kind === "implicit") {
    return a.name === b.name ? a : { kind: "indeterminate" };
  }
  if (a.kind === "implicit") return a;
  return b;
}

// resolveDeriv resolves a derivation to the concrete collation a comparison / ORDER BY uses. "none"
// and C → null (byte order, the fast path); a loaded name → its table (42704 if it vanished);
// "indeterminate" → 42P22 (the collation is required but ambiguous).
function resolveDeriv(catalog: Database, d: Deriv): Collation | null {
  if (d.kind === "indeterminate") {
    throw engineError(
      "indeterminate_collation",
      "could not determine which collation to use for string comparison",
    );
  }
  if (d.kind === "implicit" || d.kind === "explicit") {
    return resolveCollationName(catalog, d.name);
  }
  return null;
}

// collatedCmp compares two non-NULL text values under a loaded collation (spec/design/collation.md
// §6/§7): order by the UCA sort keys, whose memcmp order IS the collation order. The caller charges
// the collate cost and handles NULLs. Returns <0, 0, >0.
function collatedCmp(coll: Collation, a: string, b: string): number {
  return cmpBytes(collationSortKey(coll, a), collationSortKey(coll, b));
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
      // Range set operators (RF4, spec/design/range-functions.md §4): `+` union, `-` difference, `*`
      // intersection over two ranges. A range operand in any of these three is the set-op axis — both
      // operands must be ranges of a common element type, else 42883 (matching PG's "operator does not
      // exist"); the numeric/temporal arithmetic below never sees a range. `/` and `%` have no range
      // meaning and fall straight through.
      if (
        (op === "add" || op === "sub" || op === "mul") &&
        (p.lt.kind === "range" || p.rt.kind === "range")
      ) {
        return resolveRangeSetOp(op, p.rl, p.lt, p.rr, p.rt);
      }
      // interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Checked
      // before the ±-only temporal rule below.
      const scaled = intervalScaleResult(op, p.lt.kind, p.rt.kind);
      if (scaled !== undefined) {
        return {
          node: { kind: "arith", op, result: scaled, lhs: p.rl, rhs: p.rr },
          type: resolvedTypeOf(scaled),
        };
      }
      // Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz] ±
      // interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval. The
      // eval dispatches on the value kinds; here we settle the result type. A temporal operand in
      // any other combination is a 42804.
      const temporal = temporalArithResult(op, p.lt.kind, p.rt.kind);
      if (temporal !== undefined) {
        return {
          node: { kind: "arith", op, result: temporal, lhs: p.rl, rhs: p.rr },
          type: resolvedTypeOf(temporal),
        };
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
        return {
          node: { kind: "arith", op, result, lhs: lhsW, rhs: rhsW },
          type: { kind: "float", ty: result },
        };
      }
      requireNumericOperand(p.lt);
      requireNumericOperand(p.rt);
      if (p.lt.kind === "decimal" || p.rt.kind === "decimal") {
        return {
          node: { kind: "arith", op, result: "decimal", lhs: p.rl, rhs: p.rr },
          type: { kind: "decimal" },
        };
      }
      const result = promote(p.lt, p.rt);
      return {
        node: { kind: "arith", op, result, lhs: p.rl, rhs: p.rr },
        type: { kind: "int", ty: result },
      };
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
      // Derive the comparison's collation (spec/design/collation.md §1/§7). Only a text×text
      // comparison is collatable; a COLLATE on a non-text operand was already rejected 42804 at the
      // collate node. Each operand's derivation (explicit COLLATE / implicit column collation / none)
      // is combined per PG's rules: two different EXPLICIT collations conflict (42P21); two different
      // IMPLICIT collations are indeterminate (42P22 when consumed here). Derived for ALL comparison
      // ops incl =/<> (PG raises regardless), even though =/<> ignore the collation at eval (byte
      // equality, §7).
      let collation: Collation | null = null;
      if (p.lt.kind === "text" && p.rt.kind === "text") {
        const d = combineDeriv(deriveCollation(scope, lhs), deriveCollation(scope, rhs));
        collation = resolveDeriv(scope.catalog, d);
      }
      return {
        node: { kind: "compare", op, lhs: cl, rhs: cr, collation },
        type: { kind: "bool" },
      };
    }
    case "concat":
      return resolveConcat(scope, lhs, rhs, ag, params);
    // The containment/overlap operators (@>/<@/&&, shared by arrays and ranges) and the five
    // range-only positional/adjacency operators (<</>>/&</&>/-|-) all dispatch here: the operand
    // type chooses the array axis (array-functions.md §10) or the range axis (range-functions.md §3).
    case "contains":
    case "containedBy":
    case "overlaps":
    case "strictlyLeft":
    case "strictlyRight":
    case "notExtendRight":
    case "notExtendLeft":
    case "adjacent":
      return resolveSetOp(scope, op, lhs, rhs, ag, params);
    default: {
      // "and" | "or"
      const l = resolve(scope, lhs, null, ag, params);
      const r = resolve(scope, rhs, null, ag, params);
      requireBool(l.type, "AND/OR requires boolean operands");
      requireBool(r.type, "AND/OR requires boolean operands");
      return {
        node: { kind: op === "and" ? "and" : "or", lhs: l.node, rhs: r.node },
        type: { kind: "bool" },
      };
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
  if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyarray")
    func = "array_cat";
  else if (chosen.argFamilies[0] === "anyarray" && chosen.argFamilies[1] === "anyelement")
    func = "array_append";
  else func = "array_prepend";
  return { node: { kind: "arrayFunc", func, args: [rl.node, rr.node] }, type };
}

// noSetOpOverload is the "operator does not exist" error (42883) for a containment/positional
// operator whose operands are neither arrays of a common element type nor ranges of a common element
// type (matches PG).
function noSetOpOverload(): EngineError {
  return engineError(
    "undefined_function",
    "operator does not exist: the operands are not arrays or ranges of a common element type",
  );
}

// resolveSetOp resolves a containment / overlap / positional operator (`@>` `<@` `&&` `<<` `>>` `&<`
// `&>` `-|-`), choosing the axis by operand type: an array operand → the array containment surface
// (array-functions.md §10, only `@>`/`<@`/`&&`); a range operand → the range boolean surface
// (range-functions.md §3). The result is always boolean (strict — a NULL operand short-circuits to
// NULL at eval). A non-array / non-range pair, or a positional operator on arrays, is 42883.
function resolveSetOp(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  // Pass 1: resolve both operands with no hint.
  let rl = resolve(scope, lhs, null, ag, params);
  let rr = resolve(scope, rhs, null, ag, params);
  // RANGE axis if either operand is a range. (The five positional operators are range-only; on a
  // non-range pair they fall through to the array branch below, which rejects them as 42883.)
  if (rl.type.kind === "range" || rr.type.kind === "range") {
    return resolveRangeOp(scope, op, lhs, rhs, rl, rr, ag, params);
  }

  // ARRAY axis: only @>/<@/&& have an array overload (array-functions.md §10).
  let func: ArrayFuncName;
  if (op === "contains") func = "contains";
  else if (op === "containedBy") func = "contained_by";
  else if (op === "overlaps") func = "overlaps";
  // A positional/adjacency operator on non-range operands — no array overload exists.
  else throw noSetOpOverload();

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
  if (!matchPoly(["anyarray", "anyarray"], tys).matched) throw noSetOpOverload();
  return {
    node: { kind: "arrayFunc", func, args: [rl.node, rr.node] },
    type: { kind: "bool" },
  };
}

// rangeOpFor maps a containment/positional BinaryOp to its range-against-range kernel (RangeOpName).
function rangeOpFor(op: BinaryOp): RangeOpName {
  switch (op) {
    case "contains":
      return "contains";
    case "containedBy":
      return "containedBy";
    case "overlaps":
      return "overlaps";
    case "strictlyLeft":
      return "before";
    case "strictlyRight":
      return "after";
    case "notExtendRight":
      return "overleft";
    case "notExtendLeft":
      return "overright";
    case "adjacent":
      return "adjacent";
    default:
      throw new Error("rangeOpFor is only called for the eight set/positional operators");
  }
}

// resolveRangeOp resolves the RANGE axis of a containment/positional operator (range-functions.md §3),
// with both operands already resolved (pass 1, to avoid double aggregate collection — only the element
// operand re-resolves with the element hint). The overload is chosen by the operand types: range×range
// (the elements must match, else 42883) for every operator; the bare element overloads `range @>
// element` and `element <@ range` re-resolve the element operand with the range's element type as the
// hint and type-check assignability. A bare untyped NULL on one side is treated as a NULL range (the
// range×range overload; eval yields NULL). Anything else is 42883.
function resolveRangeOp(
  scope: Scope,
  op: BinaryOp,
  lhs: Expr,
  rhs: Expr,
  rl: { node: RExpr; type: ResolvedType },
  rr: { node: RExpr; type: ResolvedType },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  const lt = rl.type;
  const rt = rr.type;
  // range × range: the elements must match.
  if (lt.kind === "range" && rt.kind === "range") {
    const le = resolvedRangeElementScalar(lt.elem);
    const re = resolvedRangeElementScalar(rt.elem);
    if (le === undefined || re === undefined || le !== re) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: le,
      },
      type: { kind: "bool" },
    };
  }
  // range × NULL (a bare NULL is taken as a NULL range; eval yields NULL).
  if (lt.kind === "range" && rt.kind === "null") {
    const le = resolvedRangeElementScalar(lt.elem);
    if (le === undefined) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: le,
      },
      type: { kind: "bool" },
    };
  }
  if (lt.kind === "null" && rt.kind === "range") {
    const re = resolvedRangeElementScalar(rt.elem);
    if (re === undefined) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: rangeOpFor(op),
        args: [rl.node, rr.node],
        elem: re,
      },
      type: { kind: "bool" },
    };
  }
  // `range @> element` — the element overload of `@>` (the only operator with one). Re-resolve the
  // right operand with the range's element as the hint, then check it is assignable.
  if (lt.kind === "range" && op === "contains") {
    const elem = resolvedRangeElementScalar(lt.elem);
    if (elem === undefined) throw noSetOpOverload();
    const re = resolve(scope, rhs, elem, ag, params);
    if (!rangeBoundAssignable(re.type, elem)) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: "containsElem",
        args: [rl.node, re.node],
        elem,
      },
      type: { kind: "bool" },
    };
  }
  // `element <@ range` — the element overload of `<@`.
  if (rt.kind === "range" && op === "containedBy") {
    const elem = resolvedRangeElementScalar(rt.elem);
    if (elem === undefined) throw noSetOpOverload();
    const le = resolve(scope, lhs, elem, ag, params);
    if (!rangeBoundAssignable(le.type, elem)) throw noSetOpOverload();
    return {
      node: {
        kind: "rangeOp",
        op: "elemContainedBy",
        args: [le.node, rr.node],
        elem,
      },
      type: { kind: "bool" },
    };
  }
  throw noSetOpOverload();
}

// resolveRangeSetOp resolves a range SET operator (`+` union, `-` difference, `*` intersection —
// range-functions.md §4), reached from resolveBinary when a `+`/`-`/`*` has a range operand (the
// operands are already resolved). Both must be ranges over the SAME element type — a range × non-range,
// or a cross-element pair, is 42883 (PG's "operator does not exist"); a bare untyped NULL beside a range
// is taken as a NULL range (the range×range overload; eval → NULL, strict). The result is a range over
// that element type. range_merge does NOT come through here (it is a function call — see
// resolveRangeFunc); it shares the "rangeSetOp" node with op = "merge".
function resolveRangeSetOp(
  op: BinaryOp,
  rl: RExpr,
  lt: ResolvedType,
  rr: RExpr,
  rt: ResolvedType,
): { node: RExpr; type: ResolvedType } {
  let elem: ScalarType;
  if (lt.kind === "range" && rt.kind === "range") {
    const le = resolvedRangeElementScalar(lt.elem);
    const re = resolvedRangeElementScalar(rt.elem);
    if (le === undefined || re === undefined || le !== re) throw noSetOpOverload();
    elem = le;
  } else if (lt.kind === "range" && rt.kind === "null") {
    const le = resolvedRangeElementScalar(lt.elem);
    if (le === undefined) throw noSetOpOverload();
    elem = le;
  } else if (lt.kind === "null" && rt.kind === "range") {
    const re = resolvedRangeElementScalar(rt.elem);
    if (re === undefined) throw noSetOpOverload();
    elem = re;
  } else {
    // A range paired with a non-range (or any other combination) — no such operator.
    throw noSetOpOverload();
  }
  let setop: RangeSetOpName;
  if (op === "add") setop = "union";
  else if (op === "sub") setop = "difference";
  else if (op === "mul") setop = "intersect";
  else throw new Error("resolveRangeSetOp is only called for +, -, *");
  return {
    node: { kind: "rangeSetOp", op: setop, args: [rl, rr] },
    type: { kind: "range", elem: resolvedTypeOf(elem) },
  };
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
  return {
    node: { kind: "quantified", op, all, lhs: rl.node, array: ra.node },
    type: { kind: "bool" },
  };
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
    case "strictlyLeft":
      return "<<";
    case "strictlyRight":
      return ">>";
    case "notExtendRight":
      return "&<";
    case "notExtendLeft":
      return "&>";
    case "adjacent":
      return "-|-";
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
  // Range comparison is the PG range_cmp total order (spec/design/ranges.md §6). Two ranges are
  // comparable iff they are over the SAME element type — i32range × i32range only, never
  // i32range × i64range or i32range × i32 (no implicit cross-element range comparison this slice;
  // stricter than the int↔bigint scalar case, so the element types must be EQUAL, not merely
  // comparable). A bare NULL is always comparable (the comparison is unknown). Checked FIRST so a
  // range × array/composite pair reports the range message (matching the Rust arm order).
  const rangeL = lt.kind === "range";
  const rangeR = rt.kind === "range";
  if (rangeL && rangeR) {
    if (!resolvedTypeEqual(lt.elem, rt.elem)) {
      throw typeError("cannot compare ranges of different element types");
    }
    return;
  }
  if ((rangeL || rangeR) && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a range value with a value of a different type");
  }
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
  return (
    e.kind === "literal" &&
    (e.literal.kind === "int" || e.literal.kind === "decimal" || e.literal.kind === "text")
  );
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
    throw engineError(
      "invalid_text_representation",
      "invalid input syntax for type bytea: " + r.error,
    );
  }
  return r.bytes;
}

// decodeUuidLiteral decodes a single-quoted literal's content as a uuid value via the
// PG-flexible input (parseUuid), mapping malformed input to a 22P02. Used when a string literal
// adapts to a uuid context (types.md §6/§14); deterministic, fires at resolve before any scan.
function decodeUuidLiteral(str: string): Uint8Array {
  const r = parseUuid(str);
  if ("error" in r) {
    throw engineError(
      "invalid_text_representation",
      "invalid input syntax for type uuid: " + r.error,
    );
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
  return {
    node: { kind: "constFloat", ty, value: n },
    type: { kind: "float", ty },
  };
}

// coerceStringToRangeExpr coerces a range text literal to a constant range expression
// ('[1,5)'::i32range / i32range '[1,5)'): parse, coerce each bound to the element type, then
// canonicalize (spec/design/ranges.md §4/§5). Folds to a constRange. 22P02 malformed / 22000
// lower>upper / 22003 canonicalize overflow.
function coerceStringToRangeExpr(
  text: string,
  desc: RangeDesc,
): { node: RExpr; type: ResolvedType } {
  const val = coerceStringToRange(text, desc);
  const elemRt = resolvedTypeOf(elementScalar(desc));
  return {
    node: { kind: "constRange", value: val },
    type: { kind: "range", elem: elemRt },
  };
}

function coerceStringToRange(text: string, desc: RangeDesc): Value {
  const parsed = parseRangeText(text);
  if (parsed.empty) return emptyRangeValue();
  const elem = elementScalar(desc);
  const coerceBound = (b: string | null): Value | null => {
    if (b === null) return null;
    const { node } = coerceStringLiteral(b, elem, null);
    return rexprConstToValue(node);
  };
  const lower = coerceBound(parsed.lower);
  const upper = coerceBound(parsed.upper);
  return finalizeRange(desc, lower, upper, parsed.lowerInc, parsed.upperInc);
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
      return {
        node: { kind: "constBytea", value: decodeByteaLiteral(s) },
        type: { kind: "bytea" },
      };
    case "uuid":
      return {
        node: { kind: "constUuid", value: decodeUuidLiteral(s) },
        type: { kind: "uuid" },
      };
    case "timestamp":
      return {
        node: { kind: "constTimestamp", value: parseTimestamp(s) },
        type: { kind: "timestamp" },
      };
    case "timestamptz":
      return {
        node: { kind: "constTimestamptz", value: parseTimestamptz(s) },
        type: { kind: "timestamptz" },
      };
    case "date":
      return {
        node: { kind: "constDate", value: parseDate(s) },
        type: { kind: "date" },
      };
    case "interval":
      return {
        node: { kind: "constInterval", value: parseInterval(s) },
        type: { kind: "interval" },
      };
    case "text":
      // text 'x' is identity — the string IS the value.
      return { node: { kind: "constText", value: s }, type: { kind: "text" } };
    case "boolean":
      return {
        node: { kind: "constBool", value: parseBoolLiteral(s) },
        type: { kind: "bool" },
      };
    case "f32":
    case "f64": {
      const n = parseFloatLiteral(s, target);
      return {
        node: { kind: "constFloat", ty: target, value: n },
        type: { kind: "float", ty: target },
      };
    }
    case "decimal": {
      let d = parseDecimalLiteral(s);
      d = typmod !== null ? d.coerceToTypmod(typmod.precision, typmod.scale) : d.checkCap();
      return {
        node: { kind: "constDecimal", value: d },
        type: { kind: "decimal" },
      };
    }
    default: {
      // i16 / i32 / i64
      const n = parseIntLiteral(s, target);
      return {
        node: { kind: "constInt", value: n },
        type: { kind: "int", ty: target },
      };
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
    engineError(
      "invalid_text_representation",
      `malformed record literal: "${text}" for type ${ct.name}`,
    );
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
      if (nested === undefined)
        throw new Error("nested composite type resolved at CREATE TYPE / load");
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
    } else if (f.type.kind === "range") {
      // A range field cannot occur: CREATE TYPE rejects a range field (range columns are not
      // storable yet — R2).
      throw new Error("a composite range field is rejected at CREATE TYPE (R2)");
    } else {
      const { node, type } = coerceStringLiteral(tok, f.type.scalar, f.decimal);
      nodes.push(node);
      fieldTypes.push({ name: f.name, type });
    }
  }
  return {
    node: { kind: "row", fields: nodes },
    type: { kind: "composite", name: ct.name, fields: fieldTypes },
  };
}

// parseIntLiteral parses a string literal's content as a signed integer of type ty — the
// text→integer coercion for INTEGER '42' / CAST('42' AS int) (grammar.md §36). jed's OWN
// integer-literal grammar: trimmed ASCII whitespace, optional +/-, then ASCII decimal digits
// (NO hex/octal/binary or underscores — 22P02, a documented PG divergence). Out of range → 22003.
function parseIntLiteral(s: string, ty: ScalarType): bigint {
  const invalid = (): Error =>
    engineError(
      "invalid_text_representation",
      `invalid input syntax for type ${canonicalName(ty)}: "${s}"`,
    );
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
      throw engineError(
        "invalid_text_representation",
        `invalid input syntax for type boolean: "${s}"`,
      );
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
    engineError(
      "invalid_text_representation",
      `invalid input syntax for type ${canonicalName(ty)}: "${s}"`,
    );
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
    // A range/composite/array operand is non-numeric (range arithmetic + * - lands in RF4).
    t.kind === "range" ||
    t.kind === "composite" ||
    t.kind === "array" ||
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
  if (
    op === "add" &&
    ((l === "timestamp" && r === "interval") || (l === "interval" && r === "timestamp"))
  )
    return "timestamp";
  if (op === "sub" && l === "timestamp" && r === "interval") return "timestamp";
  if (
    op === "add" &&
    ((l === "timestamptz" && r === "interval") || (l === "interval" && r === "timestamptz"))
  )
    return "timestamptz";
  if (op === "sub" && l === "timestamptz" && r === "interval") return "timestamptz";
  if (
    op === "sub" &&
    ((l === "timestamp" && r === "timestamp") || (l === "timestamptz" && r === "timestamptz"))
  )
    return "interval";
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
    t.kind === "date" ||
    t.kind === "range"
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

// evalRangeFunc evaluates a range accessor (spec/design/range-functions.md §1, RF1). STRICT: a NULL
// range → NULL. lower/upper yield the bound value (NULL when empty or unbounded on that side); the
// _inc/_inf readers + isempty yield boolean. For the empty range every reader but isempty is
// false/NULL; for an infinite bound the _inf reader is true and the _inc reader false. The resolver
// guarantees the operand is a range or NULL.
function evalRangeFunc(func: RangeFuncName, vals: Value[]): Value {
  const rv = vals[0]!;
  if (rv.kind === "null") return nullValue();
  if (rv.kind !== "range") throw new Error("range accessor: range operand");
  switch (func) {
    case "lower":
      return !rv.empty && rv.lower !== null ? rv.lower : nullValue();
    case "upper":
      return !rv.empty && rv.upper !== null ? rv.upper : nullValue();
    case "isempty":
      return boolValue(rv.empty);
    // For the empty range both inclusivity flags are false by the canonical invariant, so reading
    // them directly already yields PG's false; an infinite bound likewise stores _inc = false.
    case "lower_inc":
      return boolValue(rv.lowerInc);
    case "upper_inc":
      return boolValue(rv.upperInc);
    // The empty range is NOT infinite on either side (PG): guard before reading the bound.
    case "lower_inf":
      return boolValue(!rv.empty && rv.lower === null);
    case "upper_inf":
      return boolValue(!rv.empty && rv.upper === null);
  }
}

// evalRangeCtor evaluates a range constructor (spec/design/range-functions.md §2, RF2). `vals` is
// [lo, hi] or [lo, hi, bounds]. Each bound is coerced to the element `elem` assignment-style (a NULL
// bound → an infinite bound; an integer range-checks 22003; an int→decimal / text→temporal adapts),
// the bounds flags are read (default `[)`; a NULL 3-arg flags → 22000; an invalid flags string →
// 42601), and finalizeRange produces the canonical value (order-check 22000, canonicalize,
// empty-normalize).
function evalRangeCtor(elem: ScalarType, vals: Value[]): Value {
  const desc = rangeForElement(elem);
  if (desc === undefined) throw new Error("a range constructor's elem has a range");
  const lower = coerceRangeBound(vals[0]!, elem);
  const upper = coerceRangeBound(vals[1]!, elem);
  let lowerInc: boolean;
  let upperInc: boolean;
  const flags = vals[2];
  if (flags === undefined) {
    // 2-arg form defaults to `[)`.
    lowerInc = true;
    upperInc = false;
  } else if (flags.kind === "null") {
    throw engineError("data_exception", "range constructor flags argument must not be null");
  } else if (flags.kind === "text") {
    [lowerInc, upperInc] = parseBoundFlags(flags.text);
  } else {
    throw new Error("resolver restricts the range bounds flags to text");
  }
  return finalizeRange(desc, lower, upper, lowerInc, upperInc);
}

// coerceRangeBound coerces one constructor bound value to the range element `elem`, returning null
// for a NULL bound (an infinite bound). Reuses storeValue (the INSERT/UPDATE assignment coercion):
// an integer range-checks into the element (22003), an int→decimal widens, a text→temporal parses,
// and a non-assignable value is 42804 (the resolver already screened the common 42883 cases).
function coerceRangeBound(v: Value, elem: ScalarType): Value | null {
  const stored = storeValue(v, elem, null, false, "range bound");
  return stored.kind === "null" ? null : stored;
}

// expectRange extracts the range value the resolver guaranteed is a (non-NULL) range operand.
function expectRange(v: Value): Value & { kind: "range" } {
  if (v.kind !== "range")
    throw new Error("the range-operator resolver guarantees a range operand here");
  return v;
}

// evalRangeOp evaluates a range boolean operator (range-functions.md §3, RF3) over two
// already-evaluated operand values. STRICT: a NULL operand → NULL. For the range-against-range
// operators both operands are ranges; for the element overloads (containsElem/elemContainedBy) the
// non-range operand is coerced to the range's element type `elem` (assignment-style, matching the
// resolver's hint). The boolean kernels live in range.ts.
function evalRangeOp(op: RangeOpName, l: Value, r: Value, elem: ScalarType): Value {
  if (l.kind === "null" || r.kind === "null") return nullValue();
  let result: boolean;
  switch (op) {
    // `range @> element`: l is the range, r the element (coerced to the range's element type).
    case "containsElem": {
      const e = storeValue(r, elem, null, false, "range element");
      result = rangeContainsElem(expectRange(l), e);
      break;
    }
    // `element <@ range`: l is the element, r the range.
    case "elemContainedBy": {
      const e = storeValue(l, elem, null, false, "range element");
      result = rangeContainsElem(expectRange(r), e);
      break;
    }
    case "contains":
      result = rangeContains(expectRange(l), expectRange(r));
      break;
    case "containedBy":
      result = rangeContains(expectRange(r), expectRange(l));
      break;
    case "overlaps":
      result = rangeOverlaps(expectRange(l), expectRange(r));
      break;
    case "before":
      result = rangeBefore(expectRange(l), expectRange(r));
      break;
    case "after":
      result = rangeAfter(expectRange(l), expectRange(r));
      break;
    case "overleft":
      result = rangeOverleft(expectRange(l), expectRange(r));
      break;
    case "overright":
      result = rangeOverright(expectRange(l), expectRange(r));
      break;
    case "adjacent":
      result = rangeAdjacent(expectRange(l), expectRange(r));
      break;
  }
  return boolValue(result);
}

// evalRangeSetOp evaluates a range SET operator (range-functions.md §4, RF4) over two already-evaluated
// operand values. STRICT: a NULL operand → NULL. "union"/"difference" raise 22000 on a non-contiguous
// result; "intersect"/"merge" never error. The kernels live in range.ts.
function evalRangeSetOp(op: RangeSetOpName, l: Value, r: Value): Value {
  if (l.kind === "null" || r.kind === "null") return nullValue();
  const a = expectRange(l);
  const b = expectRange(r);
  switch (op) {
    case "union":
      return rangeUnion(a, b, true);
    case "merge":
      return rangeUnion(a, b, false);
    case "intersect":
      return rangeIntersect(a, b);
    case "difference":
      return rangeMinus(a, b);
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
    throw engineError(
      "feature_not_supported",
      "removing elements from multidimensional arrays is not supported",
    );
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
  return {
    kind: "array",
    dims: [...arr.dims],
    lbounds: [...arr.lbounds],
    elements,
  };
}

// arrayPositionValue is array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the
// array's lower-bound space) of the first element NOT DISTINCT FROM e, NULL if absent. 1-D/empty only
// (a multidimensional array is 0A000); the optional start is a subscript to begin at, and a NULL
// start is 22004.
function arrayPositionValue(arr: Value, elem: Value, start: Value | null): Value {
  if (arr.kind !== "array") return nullValue();
  if (arr.dims.length > 1) {
    throw engineError(
      "feature_not_supported",
      "searching for elements in multidimensional arrays is not supported",
    );
  }
  const lb = arr.lbounds.length > 0 ? arr.lbounds[0]! : 1;
  let begin = 0;
  if (start !== null) {
    if (start.kind === "null")
      throw engineError("null_value_not_allowed", "initial position must not be null");
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
    throw engineError(
      "feature_not_supported",
      "searching for elements in multidimensional arrays is not supported",
    );
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
  return {
    kind: "array",
    dims: [arr.dims[0]! + 1],
    lbounds: [...arr.lbounds],
    elements,
  };
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
  const mismatch = () =>
    engineError("array_subscript_error", "cannot concatenate incompatible arrays");
  const eqInts = (x: number[], y: number[]): boolean =>
    x.length === y.length && x.every((v, i) => v === y[i]);
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
  const eqNum = (a: number[], b: number[]): boolean =>
    a.length === b.length && a.every((x, i) => x === b[i]);
  const dims0 = arrs[0]!.dims;
  const lbounds0 = arrs[0]!.lbounds;
  for (const a of arrs.slice(1)) {
    if (!eqNum(a.dims, dims0) || !eqNum(a.lbounds, lbounds0)) throw arraySubscriptErr(mismatch);
  }
  if (dims0.length === 0) return emptyArray(); // all sub-arrays empty → empty array
  const elements: Value[] = [];
  for (const a of arrs) elements.push(...a.elements);
  return {
    kind: "array",
    dims: [arrs.length, ...dims0],
    lbounds: [1, ...lbounds0],
    elements,
  };
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
function arrayGetElement(
  a: { dims: number[]; lbounds: number[]; elements: Value[] },
  idxs: bigint[],
): Value {
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
  return {
    kind: "array",
    dims: newDims,
    lbounds: new Array(ndim).fill(1),
    elements,
  };
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
  throw engineError(
    "datatype_mismatch",
    `VALUES types ${rtName(a)} and ${rtName(b)} cannot be matched`,
  );
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
  if (a.kind === "float" && b.kind === "float")
    return { kind: "float", ty: promoteFloat(a.ty, b.ty) };
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
  else if (isFloat(colTy))
    ok = (t.kind === "float" && promoteFloat(t.ty, colTy) === colTy) || t.kind === "null";
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
function resolveTypeAndTypmod(
  name: string,
  typeMod: TypeMod | null,
): [ScalarType, DecimalTypmod | null] {
  const ty = scalarTypeFromName(name);
  if (ty === undefined) {
    throw engineError("undefined_object", "type does not exist: " + name);
  }
  if (typeMod === null) return [ty, null];
  if (!isDecimal(ty)) {
    throw engineError(
      "feature_not_supported",
      "a type modifier is not supported for type " + canonicalName(ty),
    );
  }
  return [ty, validateDecimalTypmod(typeMod)];
}

// validateDecimalTypmod validates a decimal numeric(p[,s]) type modifier: 1 <= p <= 1000,
// 0 <= s <= p; else trap 22023 (spec/design/decimal.md §2). numeric(p) means scale 0.
function validateDecimalTypmod(tm: TypeMod): DecimalTypmod {
  const p = tm.precision;
  if (p < 1n || p > BigInt(MAX_PRECISION)) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC precision ${p} must be between 1 and ${MAX_PRECISION}`,
    );
  }
  const s = tm.scale ?? 0n;
  if (s > p || s > BigInt(MAX_SCALE)) {
    throw engineError(
      "invalid_parameter_value",
      `NUMERIC scale ${s} must be between 0 and precision ${p}`,
    );
  }
  return { precision: Number(p), scale: Number(s) };
}

// storeValue coerces a value into a column for storage (shared by INSERT and UPDATE). NULL
// honours NOT NULL (23502); an integer into an integer column is range-checked (22003); an
// integer into a decimal column widens (int→decimal) then coerces to the typmod; a decimal into
// a decimal column coerces to the typmod (rounds, precision-checks → 22003); a boolean into a
// boolean column is accepted as-is; a cross-family value (decimal→int, text→int, etc.) is 42804.
function storeValue(
  v: Value,
  colTy: ScalarType,
  typmod: DecimalTypmod | null,
  notNull: boolean,
  colName: string,
): Value {
  switch (v.kind) {
    case "null":
      if (notNull) {
        throw engineError(
          "not_null_violation",
          "null value in column " + colName + " violates not-null constraint",
        );
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
      throw typeError(
        "cannot store an integer value in " + canonicalName(colTy) + " column " + colName,
      );
    case "decimal":
      if (isDecimal(colTy)) return decimalValue(coerceDecimal(v.dec, typmod));
      // A decimal LITERAL adapts to a float column (float.md §4): nearest binary, fround for f32.
      if (isFloat(colTy)) {
        const d = Number(v.dec.render());
        if (!Number.isFinite(d)) throw overflow(colTy);
        return makeFloat(colTy, d);
      }
      throw typeError(
        "cannot store a decimal value in " + canonicalName(colTy) + " column " + colName,
      );
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
      throw typeError(
        "cannot store a text value in " + canonicalName(colTy) + " column " + colName,
      );
    case "bytea":
      if (isBytea(colTy)) return v;
      throw typeError(
        "cannot store a bytea value in " + canonicalName(colTy) + " column " + colName,
      );
    case "uuid":
      if (isUuid(colTy)) return v;
      throw typeError(
        "cannot store a uuid value in " + canonicalName(colTy) + " column " + colName,
      );
    case "timestamp":
      if (isTimestamp(colTy)) return v;
      throw typeError(
        "cannot store a timestamp value in " + canonicalName(colTy) + " column " + colName,
      );
    case "timestamptz":
      if (isTimestamptz(colTy)) return v;
      throw typeError(
        "cannot store a timestamptz value in " + canonicalName(colTy) + " column " + colName,
      );
    case "date":
      if (isDate(colTy)) return v;
      throw typeError(
        "cannot store a date value in " + canonicalName(colTy) + " column " + colName,
      );
    case "interval":
      if (isInterval(colTy)) return v;
      throw typeError(
        "cannot store an interval value in " + canonicalName(colTy) + " column " + colName,
      );
    default: // bool
      if (isBool(colTy)) return v;
      throw typeError(
        "cannot store a boolean value in " + canonicalName(colTy) + " column " + colName,
      );
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
function coerceForStore(
  v: Value,
  ty: ColType,
  typmod: DecimalTypmod | null,
  notNull: boolean,
  colName: string,
): Value {
  if (ty.kind === "scalar") return storeValue(v, ty.scalar, typmod, notNull, colName);
  if (ty.kind === "array") return storeArray(v, ty.elem, notNull, colName);
  if (ty.kind === "range") return storeRange(v, ty.elem, notNull, colName);
  return storeComposite(v, ty.name, ty.fields, notNull, colName);
}

// storeRange coerces a value into a RANGE column (spec/design/ranges.md §4): NULL honours NOT NULL
// (23502); a range value is already canonical + element-typed by the resolver (the literal/cast
// path canonicalized it), so each present finite bound is re-coerced to the element type as a
// belt-and-suspenders identity (an unconstrained scalar coercion — no typmod, NULL-tolerant) and
// the value passes through; any other value is a 42804. An infinite bound is null and skipped;
// bounds are never NULL here (a null bound is infinite, not NULL), so the element store is never
// NOT NULL.
function storeRange(v: Value, elem: ColType, notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw engineError(
        "not_null_violation",
        "null value in column " + colName + " violates not-null constraint",
      );
    }
    return nullValue();
  }
  if (v.kind !== "range") {
    throw typeError("cannot store a non-range value in range column " + colName);
  }
  if (v.empty) return v;
  const coerce = (b: Value | null): Value | null =>
    b === null ? null : coerceForStore(b, elem, null, false, colName);
  return rangeValue(coerce(v.lower), coerce(v.upper), v.lowerInc, v.upperInc);
}

// storeArray coerces a value into an ARRAY column (spec/design/array.md §4): NULL honours NOT NULL
// (23502); an array value coerces each element to the declared element type via coerceForStore (a
// NULL element is allowed — array elements are nullable). Any other value is a 42804.
function storeArray(v: Value, elem: ColType, notNull: boolean, colName: string): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw engineError(
        "not_null_violation",
        "null value in column " + colName + " violates not-null constraint",
      );
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
function storeComposite(
  v: Value,
  typeName: string,
  fields: ColField[],
  notNull: boolean,
  colName: string,
): Value {
  if (v.kind === "null") {
    if (notNull) {
      throw engineError(
        "not_null_violation",
        "null value in column " + colName + " violates not-null constraint",
      );
    }
    return nullValue();
  }
  if (v.kind !== "composite") {
    throw typeError(
      "cannot store a non-record value in composite column " + colName + " (type " + typeName + ")",
    );
  }
  if (v.fields.length !== fields.length) {
    throw typeError(
      "row has " +
        v.fields.length +
        " fields but composite type " +
        typeName +
        " has " +
        fields.length,
    );
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
  if (ty.kind === "range") {
    // A range column's element is always a scalar; the descriptor (for canonicalization) is
    // re-derived from it (spec/design/ranges.md §3/§4).
    if (ty.elem.kind !== "scalar")
      throw new Error("a range element is always a scalar (ranges.md §2)");
    const desc = rangeForElement(ty.elem.scalar);
    if (desc === undefined) throw new Error("a range column's element always has a range type");
    switch (iv.kind) {
      case "lit":
        // A bare string literal adapts to the range context via range_in (the same
        // string-adapts-to-context rule array/bytea/uuid use — spec/design/ranges.md §5).
        if (iv.lit.kind === "text") return coerceStringToRange(iv.lit.text, desc);
        if (iv.lit.kind === "null") return nullValue();
        throw typeError("cannot assign a scalar value to a range column");
      case "param":
        return bound[iv.index - 1]!;
      case "array":
        throw typeError("cannot assign an array value to a range column");
      case "row":
        throw typeError("cannot assign a record value to a range column");
      default: // default
        throw engineError("syntax_error", "DEFAULT is not allowed inside ROW(...)");
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
        throw typeError(
          "ROW has " +
            iv.fields.length +
            " fields but composite type " +
            ty.name +
            " has " +
            ty.fields.length,
        );
      }
      const vals: Value[] = new Array(ty.fields.length);
      for (let i = 0; i < ty.fields.length; i++)
        vals[i] = materializeInsertValue(iv.fields[i]!, ty.fields[i]!.type, bound);
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
    if (parsed.err === "boundflip")
      throw arraySubscriptErr("upper bound cannot be less than lower bound");
    throw engineError("invalid_text_representation", "malformed array literal");
  }
  const vals = parsed.value.tokens.map((tok) => {
    if (tok === null) return nullValue();
    // Coerce the token to the element type (a scalar via the string-literal coercion, a composite
    // via record_in — array-of-composite, spec/design/array.md §12 AC1 / §7).
    return coerceArrayElementText(tok, elem);
  });
  return {
    kind: "array",
    dims: parsed.value.dims,
    lbounds: parsed.value.lbounds,
    elements: vals,
  };
}

// coerceArrayElementText coerces one array-element token to a Value against the element ColType (the
// array_in per-element step, spec/design/array.md §7): a scalar via the string-literal coercion, a
// composite via record_in (recursive — the array-of-composite quoting nests, §12 AC1). Self-contained
// over the resolved ColType (no catalog re-walk). A nested-array element would recurse, but
// array-of-array is not a jed type, so it is unreachable in v1.
function coerceArrayElementText(tok: string, elem: ColType): Value {
  if (elem.kind === "composite") return coerceRecordTextToValue(tok, elem);
  if (elem.kind === "array") return coerceStringToArray(tok, elem.elem);
  // A range element is unreachable: array-of-range is not a storable jed type (R2), so an array
  // element ColType is never a range.
  if (elem.kind === "range")
    throw new Error("array-of-range is not a storable type (ranges.md §2)");
  const { node } = coerceStringLiteral(tok, elem.scalar, null);
  return rexprConstToValue(node);
}

// coerceRecordTextToValue is record_in over a self-contained composite ColType (the inverse of
// record_out): the token is the composite's own `(f1,f2,…)` text, tokenized by the shared
// parseRecordTokens and recursively coerced per field (a scalar field respects its decimal typmod).
// Mirrors coerceStringToComposite but produces a Value directly and walks ColType (no Database). A
// bad shape / field count is 22P02.
function coerceRecordTextToValue(
  text: string,
  ct: { kind: "composite"; name: string; fields: ColField[] },
): Value {
  const tokens = parseRecordTokens(text);
  if (tokens === null || tokens.length !== ct.fields.length) {
    throw engineError(
      "invalid_text_representation",
      `malformed record literal: "${text}" for type ${ct.name}`,
    );
  }
  const vals = tokens.map((tok, i) => {
    if (tok === null) return nullValue();
    const f = ct.fields[i]!;
    if (f.type.kind === "composite") return coerceRecordTextToValue(tok, f.type);
    if (f.type.kind === "array") return coerceStringToArray(tok, f.type.elem);
    // A composite range field is unreachable: CREATE TYPE rejects a range field (R2).
    if (f.type.kind === "range")
      throw new Error("a composite range field is rejected at CREATE TYPE (R2)");
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
  return engineError(
    "numeric_value_out_of_range",
    "value out of range for type " + canonicalName(ty),
  );
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

// evalDateConvert evaluates a cross-family datetime cast (timezones.md §9.3) of the non-NULL value v
// to `to` (timestamp/timestamptz/date). The casts crossing the timestamptz boundary consult the
// session zone (charging timezone); the others are zone-free. ±infinity maps to the target's own
// sentinel. The (source family, to) pair is guaranteed cross-family by the resolver.
function evalDateConvert(v: Value, to: ScalarType, env: EvalEnv, m: Meter): Value {
  const MICROS_PER_DAY = 86_400n * 1_000_000n;
  const microsToDate = (mc: bigint): Value => {
    if (mc === POS_INFINITY) return dateValue(DATE_POS_INFINITY);
    if (mc === NEG_INFINITY) return dateValue(DATE_NEG_INFINITY);
    const days = mc >= 0n ? mc / MICROS_PER_DAY : -((-mc + (MICROS_PER_DAY - 1n)) / MICROS_PER_DAY);
    return dateValue(days);
  };
  const dateToMicros = (d: bigint): bigint => {
    if (d === DATE_POS_INFINITY) return POS_INFINITY;
    if (d === DATE_NEG_INFINITY) return NEG_INFINITY;
    return d * MICROS_PER_DAY;
  };
  const isInf = (mc: bigint): boolean => mc === POS_INFINITY || mc === NEG_INFINITY;
  const zoneCharge = (): ZoneRef => {
    const zr = env.exec.session.timeZone;
    m.charge(COSTS.timezone);
    m.guard();
    return zr;
  };
  if (v.kind === "timestamp" && to === "date") return microsToDate(v.micros);
  if (v.kind === "date" && to === "timestamp") return timestampValue(dateToMicros(v.days));
  if (v.kind === "timestamptz" && to === "timestamp") {
    if (isInf(v.micros)) return timestampValue(v.micros);
    return timestampValue(instantToLocalMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "timestamp" && to === "timestamptz") {
    if (isInf(v.micros)) return timestamptzValue(v.micros);
    return timestamptzValue(localToInstantMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "timestamptz" && to === "date") {
    if (isInf(v.micros)) return microsToDate(v.micros);
    return microsToDate(instantToLocalMicros(zoneCharge(), v.micros));
  }
  if (v.kind === "date" && to === "timestamptz") {
    const mid = dateToMicros(v.days);
    if (isInf(mid)) return timestamptzValue(mid);
    return timestamptzValue(localToInstantMicros(zoneCharge(), mid));
  }
  throw new Error("unreachable: resolver restricts dateConvert to cross-family datetime casts");
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
    case "constRange":
      // A folded range constant (already canonical) — return it directly.
      return e.value;
    case "field": {
      // Field selection — one operator_eval, then pull the resolved field ordinal out of the
      // evaluated composite. A whole-value-NULL composite yields NULL (PG); the index is in range
      // by construction (resolve fixed it against the static field list).
      m.charge(COSTS.operatorEval);
      const base = evalExpr(e.base, row, env, m);
      if (base.kind === "null") return nullValue();
      if (base.kind !== "composite")
        throw typeError("internal: field access on a non-composite value");
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
        return decimalValue(
          (v.kind === "int" ? Decimal.fromBigInt(v.int) : (v as { dec: Decimal }).dec).negate(),
        );
      }
      if (isFloat(e.result)) {
        // Negation flips the sign (no overflow); -NaN is NaN, -Inf is -Inf per IEEE. f32 stays
        // binary32 (negation never changes the width's representability, but fround keeps the path
        // uniform). float.md §5.
        if (v.kind !== "f32" && v.kind !== "f64")
          throw typeError("internal: non-float unary minus");
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
      // A collated ORDERING comparison (< <= > >=) over two non-NULL text values orders by the
      // collation's UCA sort key (spec/design/collation.md §7), charging the collate unit per code
      // point of each operand (cost.md "collate"). =/<> are byte-equality even under a deterministic
      // collation (§7), so they take the plain path and charge no collate. A NULL operand ⇒ Unknown
      // (no sort key). [...s] counts code points (NOT s.length — the UTF-16 trap, §8).
      if (
        e.collation !== null &&
        (e.op === "lt" || e.op === "gt" || e.op === "le" || e.op === "ge")
      ) {
        if (a.kind === "text" && b.kind === "text") {
          m.charge(COSTS.collate * BigInt([...a.text].length + [...b.text].length));
          m.guard();
          const c = collatedCmp(e.collation, a.text, b.text);
          let res: boolean;
          switch (e.op) {
            case "lt":
              res = c < 0;
              break;
            case "gt":
              res = c > 0;
              break;
            case "le":
              res = c <= 0;
              break;
            default: // "ge"
              res = c >= 0;
          }
          return { kind: "bool", value: res };
        }
        // Either operand NULL ⇒ Unknown (text comparison is three-valued).
        return { kind: "null" };
      }
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
      let sub = subject.text;
      let pat = pattern.text;
      // ILIKE: simple-lowercase both sides under the engine casing regime (collation.md §16) before
      // matching — 1:1 folding so the matcher's _/length semantics survive.
      if (e.insensitive) {
        const prop = loadedProperty();
        sub = foldLowerSimple(sub, prop);
        pat = foldLowerSimple(pat, prop);
      }
      // negated carries NOT LIKE/ILIKE: matched !== negated flips for the NOT form.
      return { kind: "bool", value: likeMatch(sub, pat) !== e.negated };
    }
    case "regex": {
      m.charge(COSTS.operatorEval);
      const subject = evalExpr(e.lhs, row, env, m);
      const pattern = evalExpr(e.rhs, row, env, m);
      // NULL propagates BEFORE the matcher runs (regex.md §1) — a malformed pattern against a NULL
      // operand is still NULL, never 2201B.
      if (subject.kind === "null" || pattern.kind === "null") return nullValue();
      if (subject.kind !== "text" || pattern.kind !== "text") {
        throw new Error("unreachable: resolver requires text regex operands");
      }
      // ~* (insensitive): simple-lowercase the subject under the engine casing regime (collation.md
      // §16). The constant pattern was folded at resolve; a non-constant pattern is folded below.
      const prop = e.insensitive ? loadedProperty() : undefined;
      const sub = e.insensitive ? foldLowerSimple(subject.text, prop) : subject.text;
      const subjCps = Array.from(sub, (c) => c.codePointAt(0) as number);
      let matched: boolean;
      if (e.program !== null) {
        // Constant precompiled pattern: charge its regex_compile cost ONCE per statement execution
        // (on first eval), not per row (regex.md §5).
        if (!e.compileCharged) {
          e.compileCharged = true;
          m.charge(COSTS.regexCompile * BigInt(regexNinst(e.program)));
          m.guard();
        }
        matched = regexIsMatch(e.program, subjCps, m);
      } else {
        // Non-constant pattern: compile now (charging regex_compile) and run.
        const pat = e.insensitive ? foldLowerSimple(pattern.text, prop) : pattern.text;
        const prog = compileRegex(pat);
        m.charge(COSTS.regexCompile * BigInt(regexNinst(prog)));
        m.guard();
        matched = regexIsMatch(prog, subjCps, m);
      }
      // negated carries !~ / !~*: matched !== negated flips for the negated form.
      return { kind: "bool", value: matched !== e.negated };
    }
    case "regexFunc": {
      m.charge(COSTS.operatorEval);
      // STRICT: evaluate the args; any NULL short-circuits to NULL (regex.md §8).
      const vals: Value[] = [];
      for (const a of e.args) {
        const v = evalExpr(a, row, env, m);
        if (v.kind === "null") return nullValue();
        vals.push(v);
      }
      const text = (v: Value): string => {
        if (v.kind !== "text")
          throw new Error("unreachable: resolver requires text regexp_* operands");
        return v.text;
      };
      const source = text(vals[0]);
      const pattern = text(vals[1]);
      const replacement = e.func === "replace" ? text(vals[2]) : "";
      const flags =
        e.func === "replace" ? (vals[3] ? text(vals[3]) : "") : vals[2] ? text(vals[2]) : "";
      // Validate flags: `i` (both), `g` (replace only); anything else is 2201B.
      for (const c of flags) {
        if (!(c === "i" || (c === "g" && e.func === "replace"))) {
          throw engineError(
            "invalid_regular_expression",
            `invalid regular expression: invalid option "${c}"`,
          );
        }
      }
      const insensitive = flags.includes("i");
      const global = flags.includes("g");
      // The original-case subject (for output/captures) and the matched subject (folded when
      // case-insensitive — same length, so offsets carry over, regex.md §8).
      const origCps = Array.from(source, (ch) => ch.codePointAt(0) as number);
      const prop = insensitive ? loadedProperty() : undefined;
      const matchCps = insensitive
        ? Array.from(foldLowerSimple(source, prop), (ch) => ch.codePointAt(0) as number)
        : origCps;
      let prog: RegexProgram;
      if (e.program !== null) {
        if (!e.compileCharged) {
          e.compileCharged = true;
          m.charge(COSTS.regexCompile * BigInt(regexNinst(e.program)));
          m.guard();
        }
        prog = e.program;
      } else {
        const pat = insensitive ? foldLowerSimple(pattern, prop) : pattern;
        prog = compileRegex(pat);
        m.charge(COSTS.regexCompile * BigInt(regexNinst(prog)));
        m.guard();
      }
      if (e.func === "replace") {
        const repl = Array.from(replacement, (ch) => ch.codePointAt(0) as number);
        return { kind: "text", text: regexpReplace(prog, matchCps, origCps, repl, global, m) };
      }
      const groups = regexpMatch(prog, matchCps, origCps, m);
      if (groups === null) return nullValue();
      return arrayValue(groups.map((g) => (g === null ? nullValue() : { kind: "text", text: g })));
    }
    case "casing": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.arg, row, env, m);
      if (v.kind === "null") return nullValue();
      if (v.kind !== "text") {
        throw new Error("unreachable: resolver requires text upper/lower operand");
      }
      return textValue(foldCase(v.text, e.upper, loadedProperty()));
    }
    case "atTimeZone": {
      m.charge(COSTS.operatorEval);
      const zv = evalExpr(e.zone, row, env, m);
      const vv = evalExpr(e.value, row, env, m);
      if (zv.kind === "null" || vv.kind === "null") return nullValue();
      if (zv.kind !== "text") throw new Error("unreachable: resolver requires a text zone");
      if (vv.kind !== "timestamp" && vv.kind !== "timestamptz") {
        throw new Error("unreachable: resolver requires a timestamp/timestamptz value");
      }
      m.charge(COSTS.timezone);
      m.guard();
      const micros = vv.micros;
      // ±infinity passes through unchanged (PG): no zone offset applies, zone not validated.
      if (micros === POS_INFINITY || micros === NEG_INFINITY) {
        return e.toTimestamptz ? timestamptzValue(micros) : timestampValue(micros);
      }
      const zr = resolveZone(zv.text);
      if (zr === undefined) {
        throw engineError("invalid_parameter_value", `time zone "${zv.text}" not recognized`);
      }
      return e.toTimestamptz
        ? timestamptzValue(localToInstantMicros(zr, micros))
        : timestampValue(instantToLocalMicros(zr, micros));
    }
    case "dateTrunc": {
      m.charge(COSTS.operatorEval);
      const uv = evalExpr(e.unit, row, env, m);
      const vv = evalExpr(e.value, row, env, m);
      const zv = e.zone !== null ? evalExpr(e.zone, row, env, m) : null;
      if (uv.kind === "null" || vv.kind === "null" || (zv !== null && zv.kind === "null")) {
        return nullValue();
      }
      if (uv.kind !== "text") throw new Error("unreachable: resolver requires a text unit");
      const unitS = uv.text;
      if (vv.kind === "timestamp") return timestampValue(dateTruncMicros(unitS, vv.micros));
      if (vv.kind === "interval") return intervalValue(dateTruncInterval(unitS, vv.iv));
      if (vv.kind === "timestamptz") {
        const mc = vv.micros;
        if (mc === POS_INFINITY || mc === NEG_INFINITY) {
          dateTruncMicros(unitS, mc); // still validate the unit
          return timestamptzValue(mc);
        }
        let zr: ZoneRef;
        if (zv !== null) {
          if (zv.kind !== "text") throw new Error("unreachable: resolver requires a text zone");
          const r = resolveZone(zv.text);
          if (r === undefined) {
            throw engineError("invalid_parameter_value", `time zone "${zv.text}" not recognized`);
          }
          zr = r;
        } else {
          zr = env.exec.session.timeZone;
        }
        m.charge(COSTS.timezone);
        m.guard();
        const local = instantToLocalMicros(zr, mc);
        const trunc = dateTruncMicros(unitS, local);
        return timestamptzValue(localToInstantMicros(zr, trunc));
      }
      throw new Error("unreachable: resolver restricts date_trunc to ts/tstz/interval");
    }
    case "extract": {
      m.charge(COSTS.operatorEval);
      const vv = evalExpr(e.value, row, env, m);
      if (vv.kind === "null") return nullValue();
      let src: ExtractSrc;
      if (vv.kind === "timestamp") src = { kind: "ts", micros: vv.micros };
      else if (vv.kind === "date") src = { kind: "date", days: vv.days };
      else if (vv.kind === "interval") src = { kind: "interval", iv: vv.iv };
      else if (vv.kind === "timestamptz") {
        const mc = vv.micros;
        // `epoch` is zone-independent (the instant); every other field decomposes in the session zone
        // — so only the zone-consulting fields charge `timezone`.
        if (e.field === "epoch" || mc === POS_INFINITY || mc === NEG_INFINITY) {
          src = { kind: "tstz", instant: mc, local: mc, offsetSecs: 0n };
        } else {
          const zr = env.exec.session.timeZone;
          m.charge(COSTS.timezone);
          m.guard();
          const local = instantToLocalMicros(zr, mc);
          const secs = mc >= 0n ? mc / 1_000_000n : -((-mc + 999_999n) / 1_000_000n);
          const off = BigInt(offsetAtRef(zr, secs).utoff);
          src = { kind: "tstz", instant: mc, local, offsetSecs: off };
        }
      } else {
        throw new Error("unreachable: resolver restricts EXTRACT to ts/tstz/date/interval");
      }
      return decimalValue(extractField(e.field, src));
    }
    case "dateConvert": {
      m.charge(COSTS.operatorEval);
      const v = evalExpr(e.inner, row, env, m);
      if (v.kind === "null") return nullValue();
      return evalDateConvert(v, e.to, env, m);
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
        const shifted =
          vals.length === 1 ? tsShift(clock, (vals[0] as { iv: Interval }).iv, false) : clock;
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
          env.exec.seqSetval(
            (vals[0] as { text: string }).text,
            (vals[1] as { int: bigint }).int,
            isCalled,
          ),
        );
      }
      if (e.func === "lastval") {
        return intValue(env.exec.seqLastval());
      }
      // current_setting (spec/design/session.md §6.1): read the named session variable from the
      // session's variable map. The blanket NULL propagation above already returned NULL for a NULL
      // name / missing_ok argument, so both are non-NULL here. An unset name is 42704 UNLESS the
      // two-arg overload's missing_ok is true (→ NULL).
      if (e.func === "current_setting") {
        const name = (vals[0] as { text: string }).text;
        const missingOk = vals.length > 1 && (vals[1] as { value: boolean }).value;
        const got = env.exec.session.vars.get(name.toLowerCase());
        if (got !== undefined) {
          return textValue(got);
        }
        if (missingOk) {
          return nullValue();
        }
        throw engineError("undefined_object", "unrecognized configuration parameter: " + name);
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
    case "rangeFunc": {
      // A polymorphic range accessor (spec/design/range-functions.md §1, RF1). One operator_eval per
      // call; arguments charge their own. STRICT (a NULL range → NULL), handled in the kernel.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalRangeFunc(e.func, vals);
    }
    case "rangeCtor": {
      // A range CONSTRUCTOR call (spec/design/range-functions.md §2, RF2). One operator_eval (like
      // the range accessors); arguments charge their own evaluation. Non-strict — the kernel turns a
      // NULL bound into an infinite bound, so there is no blanket NULL short-circuit.
      m.charge(COSTS.operatorEval);
      const vals: Value[] = [];
      for (const a of e.args) vals.push(evalExpr(a, row, env, m));
      return evalRangeCtor(e.elem, vals);
    }
    case "rangeOp": {
      // A range BOOLEAN operator (spec/design/range-functions.md §3, RF3). One operator_eval; the
      // operands charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in
      // evalRangeOp.
      m.charge(COSTS.operatorEval);
      const l = evalExpr(e.args[0]!, row, env, m);
      const r = evalExpr(e.args[1]!, row, env, m);
      return evalRangeOp(e.op, l, r, e.elem);
    }
    case "rangeSetOp": {
      // A range SET operator (spec/design/range-functions.md §4). One operator_eval; the operands
      // charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in evalRangeSetOp.
      m.charge(COSTS.operatorEval);
      const l = evalExpr(e.args[0]!, row, env, m);
      const r = evalExpr(e.args[1]!, row, env, m);
      return evalRangeSetOp(e.op, l, r);
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
        if (v.kind !== "array")
          throw new Error("resolver restricts a VARIADIC operand to an array");
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
        throw engineError(
          "invalid_escape_sequence",
          "LIKE pattern must not end with escape character",
        );
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
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take square root of a negative number",
        );
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
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take logarithm of a negative number",
        );
      return out(Math.log(x));
    case "log10":
      if (x === 0) throw engineError("numeric_value_out_of_range", "cannot take logarithm of zero");
      if (x < 0)
        throw engineError(
          "numeric_value_out_of_range",
          "cannot take logarithm of a negative number",
        );
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
  const r = x ** y;
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
  if (v.kind === "bool") {
    // boolean → boolean is the identity cast (`x::boolean` on a boolean). boolean → i32 (the
    // boolean cast slice, casts.toml): true → 1, false → 0. The resolver guarantees the only
    // non-bool target is i32.
    if (isBool(target)) return v;
    return intValue(v.value ? 1n : 0n);
  }
  if (v.kind === "int") {
    // i32 → boolean (the boolean cast slice, casts.toml): 0 → false, any nonzero (incl. negative)
    // → true. The resolver guarantees the source is i32, so v.int is already in i32 range.
    if (isBool(target)) return boolValue(v.int !== 0n);
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
function or3(
  a: "true" | "false" | "unknown",
  b: "true" | "false" | "unknown",
): "true" | "false" | "unknown" {
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

// applyWindowStage is the WINDOW stage (spec/design/window.md §5.2): for each window function,
// partition the rows, sort each partition by the window ORDER BY (stable → PK tie-break, as `rows`
// arrives in PK scan order), compute the per-row result, and APPEND it to every row (so window
// result i lands at flat slot inputWidth + i, where the projection reads it). The partition + sort
// are unmetered (like ORDER BY / GROUP BY); each computed result charges windowResult and guards
// the ceiling. S0: row_number() only; partitions bucket value-canonically via an insertion-ordered
// list keyed by the value-canonical distinctRowKey (the aggregate-grouping discipline), so no
// hash-map iteration order leaks (CLAUDE.md §8/§10).
function applyWindowStage(rows: Row[], specs: WindowSpec[], meter: Meter): void {
  const n = rows.length;
  if (n === 0) return;
  // Copy each input row to a fresh array BEFORE appending: the scan yields references to the stored
  // table rows (the page store's own arrays), so appending in place would corrupt them across
  // statements. The window stage owns its row buffer (Rust holds owned Rows; the TS scan shares
  // them), so detach here, then push the per-row results onto these private copies.
  for (let i = 0; i < n; i++) rows[i] = rows[i]!.slice();
  for (const spec of specs) {
    // Partition the row indices by the partition-key values. The Map is an index only (never
    // iterated); output comes from the insertion-ordered `partitions` (no hash-order leak).
    const index = new Map<string, number>();
    const partitions: number[][] = [];
    for (let i = 0; i < n; i++) {
      const keyVals = spec.partition.map((p) => rows[i]![p]!);
      const k = distinctRowKey(keyVals);
      let pi = index.get(k);
      if (pi === undefined) {
        pi = partitions.length;
        index.set(k, pi);
        partitions.push([]);
      }
      partitions[pi]!.push(i);
    }
    // Compute each row's result into a per-row slot, then append in input order.
    const results: Value[] = new Array(n).fill(nullValue());
    for (const part of partitions) {
      // Sort the partition's row indices by the window ORDER BY. Array#sort is stable, so a full
      // tie keeps ascending original index = PK scan order (the §3 PK tie-break).
      const ordered = part.slice();
      if (spec.order.length > 0) {
        ordered.sort((a, b) => cmpRowsByOrder(rows[a]!, rows[b]!, spec.order));
      }
      switch (spec.plan) {
        case "rowNumber":
          for (let pos = 0; pos < ordered.length; pos++) {
            meter.guard(); // enforce the cost ceiling per result (CLAUDE.md §13)
            meter.charge(COSTS.windowResult);
            results[ordered[pos]!] = intValue(BigInt(pos + 1));
          }
          break;
      }
    }
    for (let i = 0; i < n; i++) rows[i]!.push(results[i]!);
  }
}

// sortRows sorts rows by the ORDER BY keys (spec/design/grammar.md §10). The all-C fast path is a
// stable sort over the value comparator; if ANY key carries a collation, the collation-aware
// sortRowsCollated decorate sorter runs instead (it can throw — an unmapped code point is 0A000).
// (Array.prototype.sort is stable in modern engines — the runtime jed targets, spill.md §6.)
function sortRows(rows: Row[], order: OrderSlot[]): void {
  if (order.some((k) => k.collation !== null)) {
    sortRowsCollated(rows, order);
    return;
  }
  rows.sort((a, b) => cmpRowsByOrder(a, b, order));
}

// cmpRowsByOrder compares two rows by the (all-C) ORDER BY keys — the first non-equal key decides; a
// full tie is 0 (the stable sort then keeps input order). Only used when no key is collated.
function cmpRowsByOrder(a: Row, b: Row, order: OrderSlot[]): number {
  for (const k of order) {
    const c = keyCmp(a[k.idx]!, b[k.idx]!, k.descending, k.nullsFirst);
    if (c !== 0) return c;
  }
  return 0;
}

// sortRowsCollated sorts rows when at least one ORDER BY key is collated (spec/design/collation.md
// §6/§8). Decorate-sort-undecorate: each collated key's UCA sort key is built ONCE per row up front
// (propagating a sortKey failure — e.g. 0A000 for an unmapped code point — at this deterministic
// per-row point, not inside the comparator), then the rows are sorted by the precomputed key bytes
// for collated slots and the value comparator for the rest. The sort is UNMETERED like every sort
// (cost.md §3); the collate cost is charged at the comparison evaluator (collation.md §11). A
// collated ORDER BY is in-memory only this slice, so this never spills (collated keys are slice 1e).
function sortRowsCollated(rows: Row[], order: OrderSlot[]): void {
  // (keys[i], row) per row; a keys entry is null for a NULL value, the sort-key bytes otherwise.
  const deco: { keys: (Uint8Array | null)[]; row: Row }[] = rows.map((row) => {
    const keys: (Uint8Array | null)[] = [];
    for (const k of order) {
      if (k.collation === null) continue;
      const v = row[k.idx]!;
      keys.push(v.kind === "text" ? collationSortKey(k.collation, v.text) : null);
    }
    return { keys, row };
  });
  deco.sort((a, b) => cmpDecorated(a.keys, a.row, b.keys, b.row, order));
  for (let i = 0; i < deco.length; i++) rows[i] = deco[i]!.row;
}

// cmpDecorated compares two decorated rows (precomputed collated-key bytes + the row) by the ORDER BY
// keys. A collated slot compares its precomputed sort-key bytes (NULL placement + the descending flip
// applied here, mirroring keyCmp); a non-collated slot compares the row values via keyCmp.
function cmpDecorated(
  akeys: (Uint8Array | null)[],
  arow: Row,
  bkeys: (Uint8Array | null)[],
  brow: Row,
  order: OrderSlot[],
): number {
  let ci = 0; // advances once per collated slot (keys stored in slot order)
  for (const k of order) {
    let c: number;
    if (k.collation !== null) {
      const ak = akeys[ci] ?? null;
      const bk = bkeys[ci] ?? null;
      ci++;
      if (ak === null && bk === null) c = 0;
      else if (ak === null) c = k.nullsFirst ? -1 : 1;
      else if (bk === null) c = k.nullsFirst ? 1 : -1;
      else {
        c = cmpBytes(ak, bk);
        if (k.descending) c = -c;
      }
    } else {
      c = keyCmp(arow[k.idx]!, brow[k.idx]!, k.descending, k.nullsFirst);
    }
    if (c !== 0) return c;
  }
  return 0;
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
    if (a.elements.length !== b.elements.length)
      return a.elements.length < b.elements.length ? -1 : 1;
    if (a.dims.length !== b.dims.length) return a.dims.length < b.dims.length ? -1 : 1;
    for (let d = 0; d < a.dims.length; d++) {
      if (a.dims[d] !== b.dims[d]) return a.dims[d]! < b.dims[d]! ? -1 : 1;
      if (a.lbounds[d] !== b.lbounds[d]) return a.lbounds[d]! < b.lbounds[d]! ? -1 : 1;
    }
    return 0;
  }
  // A range sorts by the PG range_cmp total order (spec/design/ranges.md §6): `empty` below every
  // non-empty, then lower bound, then upper bound (accounting for infinity/inclusivity). Kept
  // identical to value's lt3/gt3 range arm so `<` and ORDER BY never disagree.
  if (a.kind === "range" && b.kind === "range") return rangeTotalCmp(a, b);
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
