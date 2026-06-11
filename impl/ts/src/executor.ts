// Statement executor (CLAUDE.md §10). Mirrors executor.go / executor.rs: dispatch a
// parsed statement; analyze (resolve types, predicates, projections against the
// catalog); run. Errors throw EngineError. Results are produced deterministically
// (CLAUDE.md §10): scan in primary-key order, three-valued WHERE (only TRUE keeps a
// row), stable ORDER BY with NULLs last (the PostgreSQL model).

import type {
  BinaryOp,
  CreateIndex,
  CreateTable,
  Delete,
  DropIndex,
  DropTable,
  Expr,
  Insert,
  JoinKind,
  Literal,
  OrderKey,
  QueryExpr,
  Select,
  SelectItems,
  SetOp,
  SetOpKind,
  Statement,
  TypeMod,
  Update,
} from "./ast.ts";
import {
  type CheckConstraint,
  type Column,
  type IndexDef,
  type Table,
  columnIndex,
  pkIndices,
  primaryKeyIndex,
} from "./catalog.ts";
import { Meter } from "./cost.ts";
import { COSTS } from "./costs.ts";
import { Decimal, MAX_PRECISION, MAX_SCALE, workDiv, workLinear, workMod, workMul } from "./decimal.ts";
import { encodeInt } from "./encoding.ts";
import { type EngineError, engineError } from "./errors.ts";
import type { SharedPaging } from "./paging.ts";
import { type KeyBound, compareBytes, unboundedBound } from "./pmap.ts";
import { type Entry, type Row, TableStore } from "./storage.ts";
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
  isTimestamp,
  isTimestamptz,
  rank,
  scalarTypeFromName,
  widthBytes,
} from "./types.ts";
import { parseTimestamp, parseTimestamptz } from "./timestamp.ts";
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
  timestampValue,
  timestamptzValue,
} from "./value.ts";

// Outcome is the result of executing one statement: a bare statement (CREATE, INSERT,
// UPDATE, DELETE) or a query result set. cost is the deterministic execution cost accrued
// while running it (CLAUDE.md §13) — a DML statement accrues its scan + filter cost even
// though it returns no rows. It is a bigint for int64 parity across cores (§8).
export type Outcome =
  | { kind: "statement"; cost: bigint }
  | { kind: "query"; columnNames: string[]; rows: Value[][]; cost: bigint };

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
  stores: Map<string, TableStore>;
  // indexStores holds each secondary index's B-tree (spec/design/indexes.md §3): a
  // TableStore with ZERO value columns (entry keys only — the on-disk empty-payload
  // record), keyed by the lowercased index name (index names live in the relation
  // namespace, globally unique). Which table owns an index is recorded in that table's
  // indexes list.
  indexStores: Map<string, TableStore>;

  constructor(
    txid: bigint = 0n,
    tables: Map<string, Table> = new Map(),
    stores: Map<string, TableStore> = new Map(),
    indexStores: Map<string, TableStore> = new Map(),
  ) {
    this.txid = txid;
    this.tables = tables;
    this.stores = stores;
    this.indexStores = indexStores;
  }

  // clone returns an independent copy: the catalog map is shallow (Table objects are never mutated
  // in place — only added/removed) and each store is an O(1) persistent-map clone (pmap.ts).
  clone(): Snapshot {
    return new Snapshot(
      this.txid,
      new Map(this.tables),
      cloneStores(this.stores),
      cloneStores(this.indexStores),
    );
  }

  // table looks up a table definition by name (case-insensitive).
  table(name: string): Table | undefined {
    return this.tables.get(name.toLowerCase());
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
    const colTypes = t.columns.map((c) => c.type);
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
  // database, set by the file host (file.ts create/open) to the synchronous whole-image write. It
  // is called by commitTx with the working snapshot being published (transactions.md §4.1/§9).
  // Injecting it here keeps the executor free of a file-module dependency (no executor→file cycle).
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
  }

  // setMaxCost sets the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
  // spec/design/api.md §8). A positive limit bounds every subsequent statement: it aborts with 54P01
  // the instant accrued cost reaches limit (spec/design/cost.md §6). limit <= 0n (the default) is
  // unlimited. The primary guard for safely evaluating untrusted, user-supplied queries; a handle
  // setting, not stored in the file.
  setMaxCost(limit: bigint): void {
    this.maxCost = limit;
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
        return this.dispatchStmt(stmt, params);
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
    this.tx = { writable: true, failed: false, working: this.committed.clone() };
    let outcome: Outcome;
    try {
      outcome = this.dispatchStmt(stmt, params);
    } catch (e) {
      this.tx = null;
      throw e;
    }
    this.commitTx();
    return outcome;
  }

  // beginTx opens an explicit transaction (spec/design/transactions.md §4.2). A nested BEGIN (a
  // block is already open) is 25001. The committed snapshot is captured as the transaction's
  // working snapshot — a writable tx mutates it in place; a read-only tx reads it unchanged
  // (read-your-snapshot, §4.3). Cheap: the persistent stores clone O(1) (pmap.ts) and the catalog is
  // shallow. committed is untouched until commit.
  beginTx(writable: boolean): Outcome {
    if (this.tx !== null) {
      throw engineError("active_sql_transaction", "there is already a transaction in progress");
    }
    this.tx = { writable, failed: false, working: this.committed.clone() };
    return { kind: "statement", cost: 0n };
  }

  // commitTx commits the current transaction (spec/design/transactions.md §4.2). With no open block
  // it is a lenient no-op success. A failed block, or any read-only tx, publishes nothing — the
  // working snapshot is dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ WRITE block
  // publishes its working snapshot: bump its txid (file-backed only — an in-memory database stays at
  // txid 0), make it durable (the persistHook, §9), then swap it in as committed. A durable-write
  // failure leaves committed untouched and rethrows. Returns to autocommit.
  commitTx(): Outcome {
    const tx = this.tx;
    if (tx === null) return { kind: "statement", cost: 0n };
    this.tx = null;
    if (tx.failed || !tx.writable) return { kind: "statement", cost: 0n };
    const working = tx.working;
    if (this.path !== null) working.txid = this.committed.txid + 1n;
    // persistHook (if any) throws on an I/O failure before committed is swapped, so committed is
    // left untouched (the commit failed; the working snapshot is discarded).
    if (this.persistHook !== null) this.persistHook(this, working);
    this.committed = working;
    return { kind: "statement", cost: 0n };
  }

  // rollbackTx rolls back the current transaction (spec/design/transactions.md §4.2). With no open
  // block it is a no-op success. Otherwise the working snapshot is dropped — every staged
  // INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
  // committed was never mutated, so there is nothing to restore. Returns to autocommit.
  rollbackTx(): Outcome {
    this.tx = null;
    return { kind: "statement", cost: 0n };
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
      case "insert":
        return this.executeInsert(stmt, params);
      case "select":
        return this.executeSelect(stmt, params);
      case "setOp":
        return this.executeSetOp(stmt, params);
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
      const [ty, decimal] = resolveTypeAndTypmod(def.typeName, def.typeMod);
      if (def.primaryKey) {
        // Integers and uuid may be a key. uuid is the FIRST non-integer key type — its fixed
        // uuid-raw16 encoding (spec/design/encoding.md §2.7) is exercised. The other non-integer
        // types' order-preserving key encodings (text §2.4, decimal §2.5, bytea §2.6, boolean's
        // bool-byte) are authored but unexercised, so a text/decimal/bytea/boolean PRIMARY KEY is
        // a documented 0A000 narrowing (types.md §9/§11/§12/§13), relaxable in a later in-key slice.
        // timestamp / timestamptz are also allowed — they share the int64 int-be-signflip key
        // encoding (exercised + byte-pinned, spec/design/timestamp.md §6).
        if (!isInteger(ty) && !isUuid(ty) && !isTimestamp(ty) && !isTimestamptz(ty)) {
          throw engineError(
            "feature_not_supported",
            "a " + canonicalName(ty) + " primary key is not supported yet",
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
        if (!isInteger(ty) && !isUuid(ty) && !isTimestamp(ty) && !isTimestamptz(ty)) {
          throw engineError(
            "feature_not_supported",
            "a " + canonicalName(ty) + " primary key is not supported yet",
          );
        }
        columns[i]!.primaryKey = true;
        columns[i]!.notNull = true; // PRIMARY KEY ⇒ NOT NULL, per member
      }
      pk = indices;
    }

    // CHECK constraints (constraints.md §4). All validation runs first, in textual
    // definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
    // oracle-probed); naming follows in a second pass, so a 42703 in a later check fires
    // before a 42710 between earlier ones. Resolution needs a catalog Table, so build it
    // now (checks attach below, before putTable).
    const table: Table = { name: ct.name, columns, pk, checks: [], indexes: [] };
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
          throw engineError("duplicate_object", "check constraint " + def.name + " already exists");
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

    this.putTable(table);
    // DDL touches no rows and evaluates no expressions: zero cost.
    return { kind: "statement", cost: 0n };
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
    this.working().removeTable(dt.name.toLowerCase());
    return { kind: "statement", cost: 0n };
  }

  // findIndex finds the table owning the named index in the visible snapshot
  // (case-insensitive).
  private findIndex(name: string): [string, IndexDef] | null {
    return this.readSnap().findIndex(name);
  }

  // relationExists reports whether name is taken in the shared relation namespace (a
  // table OR an index — spec/design/indexes.md §2), case-insensitively.
  private relationExists(name: string): boolean {
    return this.table(name) !== undefined || this.findIndex(name) !== null;
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
    const cols: number[] = [];
    for (const name of ci.columns) {
      const idx = columnIndex(table, name);
      if (idx < 0) {
        throw engineError("undefined_column", "column does not exist: " + name);
      }
      const ty = columns[idx]!.type;
      if (!isInteger(ty) && !isUuid(ty) && !isTimestamp(ty) && !isTimestamptz(ty)) {
        throw engineError(
          "feature_not_supported",
          "a " + canonicalName(ty) + " index column is not supported yet",
        );
      }
      // A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
      cols.push(idx);
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
    const def: IndexDef = { name, columns: cols };
    const store = this.readSnap().store(ci.table);
    const { pages: nodes, slabs } = store.scanUnits(mask);
    meter.charge(COSTS.pageRead * BigInt(nodes) + COSTS.valueDecompress * BigInt(slabs));
    const entries: Uint8Array[] = [];
    for (const e of store.entriesInKeyOrder()) {
      meter.guard(); // enforce the cost ceiling per scanned row (CLAUDE.md §13)
      meter.charge(COSTS.storageRowRead);
      entries.push(indexEntryKey(columns, def, e.key, e.row));
    }
    meter.guard();

    const nameKey = def.name.toLowerCase();
    this.working().putIndex(tableKey, def, this.pageSize);
    const istore = this.working().indexStore(nameKey);
    for (const ek of entries) {
      if (!istore.insert(ek, [])) {
        throw new Error("index entry keys are unique (storage-key suffix)");
      }
    }
    return { kind: "statement", cost: meter.accrued };
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
    return { kind: "statement", cost: 0n };
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
    const entries = istore.rangeEntries(b);
    let pages = istore.overlapNodeCount(b);
    const store = this.readSnap().store(tableName);
    let slabs = 0;
    const rows: Row[] = [];
    for (const e of entries) {
      // Skip the remaining key components (each self-delimiting — indexes.md §5); the
      // suffix after them is the row's storage key (indexes.md §3).
      let at = prefix.length;
      for (const ty of ib.tailTypes) {
        at += e.key[at] === 0x01 ? 1 : 1 + widthBytes(ty);
      }
      const rowKey = e.key.slice(at);
      const point: KeyBound = { lo: rowKey, loInc: true, hi: rowKey, hiInc: true };
      const u = store.overlapScanUnits(point, mask);
      pages += u.pages;
      slabs += u.slabs;
      const row = store.get(rowKey);
      if (row === undefined) throw new Error("an index entry references a stored row");
      rows.push(row);
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
      // SELECT source (§24). Run the source query first; it returns OWNED rows, so a self-insert
      // (INSERT INTO t SELECT ... FROM t) reads the pre-insert snapshot, then writes. Params bind
      // through the SELECT's own resolver.
      const q = this.runSelect(ins.source.select, params);
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
        if (p >= 0 && !assignableTo(q.columnTypes[p]!, table.columns[i]!.type)) {
          const col = table.columns[i]!;
          throw typeError(
            `column ${col.name} is of type ${canonicalName(col.type)} but expression is of type ${rtName(q.columnTypes[p]!)}`,
          );
        }
      }
      // Cost = the embedded SELECT's accrued cost (§24) plus the disposition plan's
      // compression attempts for over-RECORD_MAX rows (value_compress, cost.md §3); storing
      // the rows themselves stays unmetered. Seeding the meter with the SELECT's cost keeps
      // one ceiling over the whole statement.
      const meter = new Meter(this.maxCost);
      meter.charge(q.cost);
      this.insertRows(table, store, pk, checks, provided, q.rows, meter);
      return { kind: "statement", cost: meter.accrued };
    }

    // VALUES source. A $N in a VALUES slot is typed as its TARGET COLUMN's type. Collect those
    // types across every row (a $N reused under two columns unifies; spec/design/api.md §5), then
    // bind the supplied values up front so a bad bind fails before any row is stored.
    const rowsIn = ins.source.rows;
    const ptypes = new ParamTypes();
    for (const values of rowsIn) {
      for (let i = 0; i < n; i++) {
        const p = provided[i]!;
        if (p >= 0 && p < values.length) {
          const iv = values[p]!;
          if (iv.kind === "param") ptypes.note(iv.index - 1, table.columns[i]!.type);
        }
      }
    }
    const bound = bindParams(params, ptypes.finalize());

    // Materialize each row into its value-position-indexed candidates (length arity), checking
    // arity (42601) and resolving each slot: a literal, a bound $N, or a DEFAULT keyword → that
    // column's default else NULL. The shared insertRows then builds the declaration-order row.
    const rows: Value[][] = [];
    for (const values of rowsIn) {
      if (values.length !== arity) {
        const which = ins.columns !== null ? "target columns are" : "columns are";
        throw engineError(
          "syntax_error",
          `INSERT row has ${values.length} values but ${arity} ${which} expected for table ${table.name}`,
        );
      }
      const rv: Value[] = new Array(arity);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        if (p >= 0) {
          const iv = values[p]!;
          if (iv.kind === "default") rv[p] = col.default ?? nullValue();
          else if (iv.kind === "param") rv[p] = bound[iv.index - 1]!;
          else rv[p] = literalToValue(iv.lit);
        }
      }
      rows.push(rv);
    }
    // INSERT ... VALUES reads no rows and evaluates no expression tree — its values are literals
    // and pre-evaluated constant defaults (folded at CREATE TABLE), i.e. leaves. The only
    // metered work is the disposition plan's compression attempts for over-RECORD_MAX rows
    // (value_compress, cost.md §3); a fully-inline row still costs zero.
    const meter = new Meter(this.maxCost);
    this.insertRows(table, store, pk, checks, provided, rows, meter);
    return { kind: "statement", cost: meter.accrued };
  }

  // insertRows runs phase 1 + phase 2 of an INSERT, shared by the VALUES and SELECT sources. Each
  // element of rows is one row's candidate values indexed by VALUE POSITION p (length arity); the
  // declaration-order stored row is built via provided (an omitted column takes its default else
  // NULL) and each value is type-coerced + range-checked by storeValue (23502 / 22003 / 22P02 /
  // 42804). The storage key is computed and checked for a duplicate (23505 — within this batch via
  // seenKeys AND against the store) BEFORE any row is written; only once every row validates are
  // they all inserted (phase 2), allocating a fresh monotonic rowid in row order for a no-PK
  // table. All-or-nothing: a failure leaves the store untouched and burns no rowids.
  private insertRows(
    table: Table,
    store: TableStore,
    pk: number[],
    checks: NamedCheck[],
    provided: number[],
    rows: Value[][],
    meter: Meter,
  ): void {
    const n = table.columns.length;
    const prepared: { key: Uint8Array | null; row: Row }[] = [];
    const seenKeys = new Set<string>();
    let cunits = 0n;
    for (const values of rows) {
      const row: Row = new Array(n);
      for (let i = 0; i < n; i++) {
        const col = table.columns[i]!;
        const p = provided[i]!;
        const candidate: Value = p >= 0 ? values[p]! : (col.default ?? nullValue());
        row[i] = storeValue(candidate, col.type, col.decimal, col.notNull, col.name);
      }

      // CHECK constraints, in name order, on the fully-coerced candidate row — after NOT
      // NULL (storeValue above), before the key/duplicate check (PG's per-row order,
      // constraints.md §4.4). TRUE and NULL pass; the first FALSE aborts the whole
      // statement (two-phase — nothing has been written). Evaluation is metered
      // expression work (operator_eval), so guard the ceiling per checked row.
      if (checks.length > 0) {
        meter.guard();
        const env: EvalEnv = {
          params: [],
          outer: [],
          runSubquery: (p, o) => this.execQueryPlan(p, o, []),
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
          } else if (pkv.kind === "int") {
            parts.push(encodeInt(table.columns[i]!.type, pkv.int));
          } else if (pkv.kind === "timestamp" || pkv.kind === "timestamptz") {
            // A timestamp / timestamptz PK encodes its int64 instant (spec/design/timestamp.md §6).
            parts.push(encodeInt(table.columns[i]!.type, pkv.micros));
          } else {
            throw engineError("data_corrupted", "a primary key must be an integer, uuid, or timestamp value");
          }
        }
        const total = parts.reduce((acc, b) => acc + b.length, 0);
        key = new Uint8Array(total);
        let off = 0;
        for (const b of parts) {
          key.set(b, off);
          off += b.length;
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
      // Meter the row's disposition-plan compression attempts (value_compress, cost.md §3).
      // For a no-PK table the synthetic rowid is allocated in phase 2; only the key LENGTH
      // feeds the plan, so an 8-byte placeholder stands in deterministically.
      cunits += BigInt(store.writeCompressUnits(key ?? new Uint8Array(8), row));
      prepared.push({ key, row });
    }
    // Charge + enforce the ceiling BEFORE phase 2 writes anything (all-or-nothing).
    meter.charge(COSTS.valueCompress * cunits);
    meter.guard();

    // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
    // rowid is allocated here, in row order, so a failed validation pass burns none
    // (spec/fileformat/format.md, grammar.md §12). Each stored row's secondary-index
    // entries are computed against its final key (the rowid included) and written after
    // the rows (indexes.md §4 — an index write cannot fail, so all-or-nothing is
    // unchanged).
    const indexInserts: Uint8Array[][] = table.indexes.map(() => []);
    for (const pr of prepared) {
      const key = pr.key ?? encodeInt("int64", store.allocRowid());
      for (let k = 0; k < table.indexes.length; k++) {
        indexInserts[k]!.push(indexEntryKey(table.columns, table.indexes[k]!, key, pr.row));
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
    const scope = Scope.single(this, table);
    const ptypes = new ParamTypes();
    let filter = del.filter ? resolveBooleanFilter(scope, del.filter, ptypes) : null;
    const bound = bindParams(params, ptypes.finalize());

    // Fold globally-uncorrelated WHERE subqueries once (their cost is added a single time —
    // spec/design/grammar.md §26, cost.md §3); a correlated one stays and re-runs per row via the
    // per-row outer environment below (it pushes the current row, so `target.col` reads it). The
    // uncorrelated execution reads the pre-DELETE snapshot (keys are collected before mutating).
    // Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13; cost.md §3).
    const meter = new Meter(this.maxCost);
    if (filter !== null) {
      const cost = { value: 0n };
      filter = this.foldUncorrelatedInRExpr(filter, bound, cost);
      meter.charge(cost.value);
    }
    const env: EvalEnv = { params: bound, outer: [], runSubquery: (p, o) => this.execQueryPlan(p, o, bound) };
    const store = this.working().store(del.table);
    // matched collects (key, row) pairs before mutating; the rows feed phase 2's
    // index-entry removal (indexed columns are fixed-width and always resident).
    const matched: { key: Uint8Array; row: Row }[] = [];
    // DELETE's touched set (cost.md §3): only the filter's columns — dropping a row never reads
    // its chains, so a bare DELETE charges no chain/decompress units at all.
    const mask: boolean[] = new Array(table.columns.length).fill(false);
    if (filter !== null) collectTouched(filter, 0, mask);
    // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
    // scan"); an empty bound deletes nothing. The whole WHERE stays the residual filter below.
    // page_read per visited node (block, before the rows), then storageRowRead per scanned row.
    const { entries, overlap, slabs } = scanEntries(store, mutationPkBound(table, filter), bound, mask);
    if (entries === null) return { kind: "statement", cost: meter.accrued }; // empty bound
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
    // Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
    // unmetered write work; an index removal cannot fail).
    for (const m of matched) store.remove(m.key);
    for (const def of table.indexes) {
      const istore = this.working().indexStore(def.name.toLowerCase());
      for (const m of matched) {
        istore.remove(indexEntryKey(table.columns, def, m.key, m.row));
      }
    }
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
      // The RHS is a general expression evaluated against the OLD row; a literal operand
      // adapts to the target column's type. The result must be assignable to the column's
      // family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
      const { node, type } = resolve(scope, a.value, col.type, { collecting: false, groupKeys: [], specs: [] }, ptypes);
      requireAssignable(type, col.type, a.column);
      plans.push({ idx, name: col.name, target: col.type, decimal: col.decimal, notNull: col.notNull, source: node });
    }

    let filter = upd.filter ? resolveBooleanFilter(scope, upd.filter, ptypes) : null;
    // The CHECK constraints, resolved once per statement in evaluation (name) order;
    // phase 1 evaluates them on each post-assignment row (constraints.md §4.4).
    const checks = this.resolveChecks(table);
    // All assignment RHSs + the WHERE are resolved: finalize + bind before any scan.
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
    for (const p of plans) p.source = this.foldUncorrelatedInRExpr(p.source, bound, foldCost);
    if (filter !== null) filter = this.foldUncorrelatedInRExpr(filter, bound, foldCost);
    meter.charge(foldCost.value);
    const env: EvalEnv = { params: bound, outer: [], runSubquery: (p, o) => this.execQueryPlan(p, o, bound) };
    const store = this.working().store(upd.table);
    // Each entry is (key, new row, OLD row) — the old row feeds the index maintenance.
    const updates: { key: Uint8Array; row: Row; oldRow: Row }[] = [];
    // UPDATE's touched set (cost.md §3): the filter's columns plus every assignment SOURCE's —
    // the rewrite re-stores an untouched spilled value without logically re-reading it
    // (large-values.md §14).
    const mask: boolean[] = new Array(table.columns.length).fill(false);
    if (filter !== null) collectTouched(filter, 0, mask);
    for (const p of plans) collectTouched(p.source, 0, mask);
    // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
    // scan"); an empty bound updates nothing. The whole WHERE stays the residual filter below.
    // page_read per visited node (block, before the rows), then storageRowRead per scanned row.
    const { entries, overlap, slabs } = scanEntries(store, mutationPkBound(table, filter), bound, mask);
    if (entries === null) return { kind: "statement", cost: meter.accrued }; // empty bound
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

    // Each rewritten row's disposition plan may attempt compression (a record over RECORD_MAX)
    // — meter the attempts (value_compress, cost.md §3) and enforce the ceiling BEFORE phase 2
    // writes anything, preserving all-or-nothing.
    let cunits = 0n;
    for (const u of updates) cunits += BigInt(store.writeCompressUnits(u.key, u.row));
    meter.charge(COSTS.valueCompress * cunits);
    meter.guard();

    // Index maintenance (indexes.md §4): an entry moves only when its key CHANGED — equal
    // old/new keys leave the index tree untouched (part of the contract: it keeps the
    // copy-on-write dirty set, and so the commit's written pages, byte-identical across
    // cores). The storage key cannot change (PK assignment is rejected), so the suffix is
    // stable.
    const indexMoves: { oldKey: Uint8Array; newKey: Uint8Array }[][] = table.indexes.map(() => []);
    for (const u of updates) {
      for (let k = 0; k < table.indexes.length; k++) {
        const def = table.indexes[k]!;
        const oldEk = indexEntryKey(table.columns, def, u.key, u.oldRow);
        const newEk = indexEntryKey(table.columns, def, u.key, u.row);
        if (!bytesEq(oldEk, newEk)) indexMoves[k]!.push({ oldKey: oldEk, newKey: newEk });
      }
    }

    // Phase 2: apply (keys unchanged — a PK column can't be assigned), then move the
    // changed index entries (unmetered write work; cannot fail).
    for (const u of updates) store.replace(u.key, u.row);
    for (let k = 0; k < table.indexes.length; k++) {
      const istore = this.working().indexStore(table.indexes[k]!.name.toLowerCase());
      for (const mv of indexMoves[k]!) {
        istore.remove(mv.oldKey);
        if (!istore.insert(mv.newKey, [])) {
          throw new Error("index entry keys are unique (storage-key suffix)");
        }
      }
    }
    return { kind: "statement", cost: meter.accrued };
  }

  // executeSelect runs a SELECT as a top-level statement: runSelect, then wrap as a query
  // Outcome (the projection types are internal — only INSERT ... SELECT consumes them).
  private executeSelect(sel: Select, params: Value[]): Outcome {
    const r = this.runSelect(sel, params);
    return { kind: "query", columnNames: r.columnNames, rows: r.rows, cost: r.cost };
  }

  // executeSetOp runs a set operation as a top-level statement: runSetOp, then wrap as a query
  // Outcome. Cost is lhs.cost + rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
  private executeSetOp(so: SetOp, params: Value[]): Outcome {
    const r = this.runSetOp(so, params);
    return { kind: "query", columnNames: r.columnNames, rows: r.rows, cost: r.cost };
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
    const plan = this.planQuery(qe, null, ptypes);
    const bound = bindParams(params, ptypes.finalize());
    const subqueryCost = { value: 0n };
    this.foldUncorrelatedInPlan(plan, bound, subqueryCost);
    const r = this.execQueryPlan(plan, [], bound);
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

  // planQuery resolves a query expression into an owned QueryPlan against the scope chain (parent =
  // the enclosing query's scope, null at top level). A subquery is planned here, once (§26).
  // Not private: the free function planSubquery calls it through scope.catalog (an internal seam).
  planQuery(qe: QueryExpr, parent: Scope | null, ptypes: ParamTypes): QueryPlan {
    return qe.kind === "select"
      ? this.planSelect(qe, parent, ptypes)
      : this.planSetOp(qe, parent, ptypes);
  }

  // execQueryPlan executes a resolved plan against an outer-row environment (outer = the enclosing
  // rows, innermost last; empty at top level) and the bound parameters.
  private execQueryPlan(plan: QueryPlan, outer: Row[], params: Value[]): SelectResult {
    return plan.kind === "select"
      ? this.execSelectPlan(plan, outer, params)
      : this.execSetOpPlan(plan, outer, params);
  }

  // planSetOp plans a set operation (spec/design/grammar.md §25): plan both operands with the same
  // parent scope, check arity + unify column types up front (so the 42601/42804 fire even over
  // empty operands), and resolve the trailing ORDER BY by output column name.
  private planSetOp(so: SetOp, parent: Scope | null, ptypes: ParamTypes): SetOpPlan {
    const lhs = this.planQuery(so.lhs, parent, ptypes);
    const rhs = this.planQuery(so.rhs, parent, ptypes);

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
  private execSetOpPlan(plan: SetOpPlan, outer: Row[], params: Value[]): SelectResult {
    const left = this.execQueryPlan(plan.lhs, outer, params);
    const right = this.execQueryPlan(plan.rhs, outer, params);

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

  // runSelect resolves projected columns and the WHERE/ORDER BY columns against the catalog,
  // scans in primary-key order, filters (three-valued — only TRUE keeps a row), optionally
  // re-sorts by ORDER BY, then projects. Returns the rows with each output column's NAME and
  // resolved TYPE (the types let INSERT ... SELECT gate assignability up front — §24) and the
  // accrued cost.
  // planSelect resolves a SELECT into a SelectPlan against the scope chain (parent = the enclosing
  // query's scope, for correlated references — grammar.md §26). The resolve half of the old
  // runSelect: build the FROM scope, resolve every clause, infer $N types into ptypes. No row is
  // touched and no parameter is bound here (runQueryExpr binds once, after the whole tree is planned).
  private planSelect(sel: Select, parent: Scope | null, ptypes: ParamTypes): SelectPlan {
    // Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
    // relation's flat column offset in FROM order, and reject a duplicate label — a self-join
    // without distinct aliases is 42712 (spec/design/grammar.md §15). The scope links to `parent`
    // (correlation) + the catalog (so a subquery resolves its own FROM); allowSubquery is true.
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
    const scope = new Scope(rels, this, parent, true);

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
      const partial = new Scope(scope.rels.slice(0, k + 2), this, parent, true);
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
    const relBounds: (ScanBound | null)[] = rels.map((rel) =>
      filter === null ? null : detectScanBound(filter, rel),
    );

    // Assemble the owned plan (table NAMES + offsets/widths replace the scope's tables, so the
    // plan outlives the scope and a correlated subquery can re-execute it per row).
    const planRels: PlanRel[] = scope.rels.map((rel) => ({
      tableName: rel.table.name,
      offset: rel.offset,
      colCount: rel.table.columns.length,
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

  private execSelectPlan(plan: SelectPlan, outer: Row[], params: Value[]): SelectResult {
    const env: EvalEnv = {
      params,
      outer,
      runSubquery: (p, o) => this.execQueryPlan(p, o, params),
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
      plan.relBounds[0]?.kind !== "index"
    ) {
      return this.execStreamingLimit(plan, env, meter, params);
    }

    // Materialize each base table once, in primary-key order, by draining a scanSource (the
    // pageRead block + per-row storageRowRead accrue inside the generator — spec/design/cost.md §3
    // "page_read"/JOIN). Each base table's own primary-key bound (if any) seeks/ranges instead of
    // walking the whole B-tree; an empty bound (a NULL const or contradictory bounds) reads nothing.
    // The nested loop re-reads from these in-memory buffers, which are not stores and charge nothing.
    const materialized: Row[][] = plan.rels.map((rel, ri) => {
      const store = this.readSnap().store(rel.tableName);
      let rows: Row[];
      let nodeCount: number;
      let slabs = 0;
      const relBound = plan.relBounds[ri]!;
      if (relBound !== null && relBound.kind === "index") {
        // An index bound fetches via the index tree + per-row point lookups (cost.md §3
        // "index-bounded scan").
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
          rows = store.rangeRows(b);
          const u = store.overlapScanUnits(b, plan.relMasks[ri]!);
          nodeCount = u.pages;
          slabs = u.slabs;
        }
      } else {
        rows = store.iterInKeyOrder();
        const u = store.scanUnits(plan.relMasks[ri]!);
        nodeCount = u.pages;
        slabs = u.slabs;
      }
      // Materialize this relation's touched columns where the lazy load left unfetched
      // references (large-values.md §14) — exactly the static set the cost block charges, so
      // the physical chain reads/decompressions match the metered units.
      for (let i = 0; i < rows.length; i++) {
        rows[i] = store.resolveColumns(rows[i]!, plan.relMasks[ri]!);
      }
      // The decompress slabs join the same up-front block as the page_read the scanSource
      // charges on its first next() (cost.md §3 "the compression units").
      meter.charge(COSTS.valueDecompress * BigInt(slabs));
      const tableRows: Row[] = [];
      for (const row of scanSource(rows, nodeCount, meter)) {
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
    for (let k = 0; k < plan.joins.length; k++) {
      const rightRows = materialized[k + 1]!;
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
      const newAccs = (): Acc[] => plan.aggSpecs.map((s) => newAcc(s.plan));
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

  private foldUncorrelatedInPlan(plan: QueryPlan, bound: Value[], cost: { value: bigint }): void {
    if (plan.kind === "select") {
      this.foldUncorrelatedInSelect(plan, bound, cost);
      return;
    }
    this.foldUncorrelatedInPlan(plan.lhs, bound, cost);
    this.foldUncorrelatedInPlan(plan.rhs, bound, cost);
  }

  private foldUncorrelatedInSelect(sp: SelectPlan, bound: Value[], cost: { value: bigint }): void {
    for (const j of sp.joins) if (j.on !== null) j.on = this.foldUncorrelatedInRExpr(j.on, bound, cost);
    if (sp.filter !== null) sp.filter = this.foldUncorrelatedInRExpr(sp.filter, bound, cost);
    if (sp.having !== null) sp.having = this.foldUncorrelatedInRExpr(sp.having, bound, cost);
    for (const s of sp.aggSpecs) {
      if (s.operand !== null) s.operand = this.foldUncorrelatedInRExpr(s.operand, bound, cost);
    }
    sp.projections = sp.projections.map((p) => this.foldUncorrelatedInRExpr(p, bound, cost));
  }

  // foldUncorrelatedInRExpr folds this node if it is an uncorrelated "subquery", else recurses into
  // its children. It RETURNS the (possibly replaced) node; the caller reassigns the field.
  private foldUncorrelatedInRExpr(e: RExpr, bound: Value[], cost: { value: bigint }): RExpr {
    switch (e.kind) {
      case "subquery": {
        // Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
        // globally-uncorrelated subquery nested inside it is already a constant before we run it.
        if (e.lhs !== null) e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, cost);
        this.foldUncorrelatedInPlan(e.plan, bound, cost);
        if (queryPlanReferencesOuter(e.plan, 0)) return e; // correlated — re-run per outer row
        // Uncorrelated: execute ONCE and fold to a constant / inValues.
        const r = this.execQueryPlan(e.plan, [], bound);
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
        // in
        const list = r.rows.map((row) => row[0]!);
        return { kind: "inValues", lhs: e.lhs!, list, negated: e.negated };
      }
      case "cast":
      case "neg":
      case "not":
      case "isNull":
        e.operand = this.foldUncorrelatedInRExpr(e.operand, bound, cost);
        return e;
      case "arith":
      case "compare":
      case "and":
      case "or":
      case "distinct":
      case "like":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, cost);
        e.rhs = this.foldUncorrelatedInRExpr(e.rhs, bound, cost);
        return e;
      case "case":
        e.arms = e.arms.map((arm) => ({
          cond: this.foldUncorrelatedInRExpr(arm.cond, bound, cost),
          result: this.foldUncorrelatedInRExpr(arm.result, bound, cost),
        }));
        e.els = this.foldUncorrelatedInRExpr(e.els, bound, cost);
        return e;
      case "scalarFunc":
        e.args = e.args.map((a) => this.foldUncorrelatedInRExpr(a, bound, cost));
        return e;
      case "inValues":
        e.lhs = this.foldUncorrelatedInRExpr(e.lhs, bound, cost);
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
    const bp = detectPkBound(filter, rel.offset + pkLocal, rel.table.columns[pkLocal]!.type);
    if (bp !== null) return { kind: "pk", pk: bp };
  }
  for (const idx of rel.table.indexes) {
    const ci = idx.columns[0]!;
    const ty = rel.table.columns[ci]!.type;
    const bp = detectPkBound(filter, rel.offset + ci, ty);
    const eqs: RExpr[] = [];
    if (bp !== null) {
      for (const t of bp.terms) if (t.op === "eq") eqs.push(t.src);
    }
    if (eqs.length > 0) {
      const tailTypes = idx.columns.slice(1).map((c) => rel.table.columns[c]!.type);
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
      parts.push(Uint8Array.of(0x00), encodeInt(columns[ci]!.type, v.int));
    } else if (v.kind === "uuid") {
      parts.push(Uint8Array.of(0x00), v.bytes);
    } else if (v.kind === "timestamp" || v.kind === "timestamptz") {
      parts.push(Uint8Array.of(0x00), encodeInt(columns[ci]!.type, v.micros));
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

// bytesEq reports byte equality of two keys.
function bytesEq(a: Uint8Array, b: Uint8Array): boolean {
  return compareBytes(a, b) === 0;
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
    case "constUuid":
      return isUuid(pkType);
    case "constTimestamp":
      return isTimestamp(pkType);
    case "constTimestamptz":
      return isTimestamptz(pkType);
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
// encodeInt for integer/timestamp widths, the raw 16 bytes for uuid). param/outerColumn resolve to a
// runtime Value first (the param table / the enclosing outer row) and then encode through the shared path.
function encodeBoundKey(pkType: ScalarType, src: RExpr, params: Value[], outer: Row[]): BoundKey {
  switch (src.kind) {
    case "constNull":
      return { kind: "null" };
    case "constInt":
      return inRange(pkType, src.value) ? { kind: "key", key: encodeInt(pkType, src.value) } : { kind: "outOfRange" };
    case "constUuid":
      return { kind: "key", key: src.value.slice() };
    case "constTimestamp":
    case "constTimestamptz":
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
  if (v.kind === "uuid") return { kind: "key", key: v.bytes.slice() };
  if (v.kind === "int")
    return inRange(pkType, v.int) ? { kind: "key", key: encodeInt(pkType, v.int) } : { kind: "outOfRange" };
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
  return detectPkBound(filter, pkIdx, table.columns[pkIdx]!.type);
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
    const u = store.overlapScanUnits(b, mask);
    return { entries: store.rangeEntries(b), overlap: u.pages, slabs: u.slabs };
  }
  const u = store.scanUnits(mask);
  return { entries: store.entriesInKeyOrder(), overlap: u.pages, slabs: u.slabs };
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
  for (const j of plan.joins) if (j.on !== null && rexprReferencesOuter(j.on, depth)) return true;
  if (plan.filter !== null && rexprReferencesOuter(plan.filter, depth)) return true;
  if (plan.having !== null && rexprReferencesOuter(plan.having, depth)) return true;
  for (const s of plan.aggSpecs) if (s.operand !== null && rexprReferencesOuter(s.operand, depth)) return true;
  for (const p of plan.projections) if (rexprReferencesOuter(p, depth)) return true;
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
      return e.args.some((a) => rexprReferencesOuter(a, depth));
    default:
      return false; // leaves: column, param, const*
  }
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
      for (const a of e.args) collectTouched(a, depth, touched);
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
  | { kind: "timestamp" } // zoneless instant; does not compare/cast to timestamptz
  | { kind: "timestamptz" } // UTC instant; does not compare/cast to timestamp
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
  | { kind: "constTimestamp"; value: bigint }
  | { kind: "constTimestamptz"; value: bigint }
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
  | { kind: "case"; arms: { cond: RExpr; result: RExpr }[]; els: RExpr; coerceDecimal: boolean }
  // A scalar-function call (abs/round, spec/design/functions.md §9), evaluated per row in any
  // context. `result` is the static result type — for abs over an integer it is the operand's
  // integer type, so the magnitude is range-checked at that boundary; otherwise decimal.
  | { kind: "scalarFunc"; func: "abs" | "round"; args: RExpr[]; result: ScalarType }
  // A correlated column reference (spec/design/grammar.md §26): column `index` of the enclosing
  // row `level` hops out (1 = immediate parent). A leaf — reads from the outer-row environment.
  | { kind: "outerColumn"; level: number; index: number }
  // A CORRELATED subquery, re-executed once per outer row at eval (uncorrelated ones are folded to
  // a constant / inValues before exec). `lhs`/`negated` apply to the IN form.
  | { kind: "subquery"; plan: QueryPlan; subKind: SubqueryKind; lhs: RExpr | null; negated: boolean }
  // A folded uncorrelated `IN (subquery)`: the subquery ran once yielding `list`; per row it tests
  // `lhs` for three-valued membership (empty → negated; a NULL with no positive match → NULL).
  | { kind: "inValues"; lhs: RExpr; list: Value[]; negated: boolean };

// SubqueryKind selects which subquery form a "subquery" RExpr is (spec/design/grammar.md §26).
type SubqueryKind = "scalar" | "exists" | "in";

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). planQuery (the resolve half of the old runSelect)
// produces a QueryPlan; execQueryPlan (the execute half) consumes it against an outer-row
// environment. The split lets a subquery be resolved ONCE — so its structural/type errors fire
// even over an empty outer — yet executed many times (spec/design/grammar.md §26).
// ============================================================================

// PlanRel is one relation in a SELECT plan: the table name (looked up in the store at exec), the
// flat offset of its first column, and its column count (for NULL-padding).
type PlanRel = { tableName: string; offset: number; colCount: number };

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

// QueryPlan is a resolved query expression: a SELECT plan or a set-op plan (mirrors QueryExpr).
type QueryPlan = SelectPlan | SetOpPlan;

// EvalEnv is the environment threaded into the per-row evaluator (spec/design/grammar.md §26): the
// bound parameters, the stack of enclosing rows (innermost LAST) a correlated reference reads, and
// a runSubquery callback (a correlated subquery re-runs its inner plan against the pushed stack).
// outer is empty at the top level; an outerColumn at frame `level` reads outer[outer.length-level].
type EvalEnv = {
  params: Value[];
  outer: Row[];
  runSubquery(plan: QueryPlan, outer: Row[]): SelectResult;
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
        if (!inRange("int64", s)) throw overflow("int64");
        a.sumInt = s;
        a.seen = true;
      }
      break;
    case "sumDecimal":
      if (v.kind !== "null") {
        const inc = toDecimal(v);
        m.charge(COSTS.decimalWork * BigInt(workLinear(a.sumDec, inc) - 1));
        m.guard();
        a.sumDec = a.sumDec.add(inc);
        a.seen = true;
      }
      break;
    case "avg":
      if (v.kind !== "null") {
        const inc = toDecimal(v);
        m.charge(COSTS.decimalWork * BigInt(workLinear(a.sumDec, inc) - 1));
        m.guard();
        a.sumDec = a.sumDec.add(inc);
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
  return items.items.some((it) => exprHasAggregate(it.expr));
}

// isAggregateName reports whether name (case-insensitive) is one of the five aggregates.
function isAggregateName(name: string): boolean {
  switch (name.toLowerCase()) {
    case "count":
    case "sum":
    case "min":
    case "max":
    case "avg":
      return true;
    default:
      return false;
  }
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
    default: // column, qualifiedColumn, literal
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
  // Each aggregate takes exactly one argument; a different count matches no aggregate overload
  // and is 42883 (PG).
  const arg = (): Expr => {
    if (e.args.length !== 1) {
      throw engineError("undefined_function", "no aggregate function matches the given argument count");
    }
    return e.args[0];
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

// noFuncOverload is 42883 — a scalar function over argument types it has no overload for.
function noFuncOverload(fn: string): EngineError {
  return engineError("undefined_function", "no " + fn + " function for those argument types");
}

// resolveFuncCall resolves a function call: an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar
// function (abs/round, spec/design/functions.md §9), or 42883 for any other name. Aggregates and
// scalar functions share the call syntax (grammar.md §17); they are distinguished here.
function resolveFuncCall(
  scope: Scope,
  e: { name: string; args: Expr[]; star: boolean },
  ag: AggCtx,
  params: ParamTypes,
): { node: RExpr; type: ResolvedType } {
  switch (e.name.toLowerCase()) {
    case "count":
    case "sum":
    case "min":
    case "max":
    case "avg":
      return resolveAggregate(scope, e, ag, params);
    case "abs":
    case "round":
      return resolveScalarFunc(scope, e, ag, params);
    default:
      throw engineError("undefined_function", "function does not exist: " + e.name);
  }
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
  const name = e.name.toLowerCase() as "abs" | "round";
  const rargs: RExpr[] = [];
  const tys: ResolvedType[] = [];
  for (const a of e.args) {
    const r = resolve(scope, a, null, ag, params);
    rargs.push(r.node);
    tys.push(r.type);
  }
  const numeric = (t: ResolvedType): boolean => t.kind === "int" || t.kind === "decimal";
  let result: ScalarType;
  if (name === "abs" && tys.length === 1 && tys[0].kind === "int") {
    // abs: result is the operand's own type (range-checked at its boundary for integers).
    result = tys[0].ty;
  } else if (name === "abs" && tys.length === 1 && tys[0].kind === "decimal") {
    result = "decimal";
  } else if (
    // round: always decimal; integer overloads return numeric (PG round(5)).
    name === "round" &&
    ((tys.length === 1 && numeric(tys[0])) || (tys.length === 2 && numeric(tys[0]) && tys[1].kind === "int"))
  ) {
    result = "decimal";
  } else {
    throw noFuncOverload(name);
  }
  return { node: { kind: "scalarFunc", func: name, args: rargs, result }, type: resolvedTypeOf(result) };
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
  constructor(rels: ScopeRel[], catalog: Database, parent: Scope | null, allowSubquery: boolean) {
    this.rels = rels;
    this.catalog = catalog;
    this.parent = parent;
    this.allowSubquery = allowSubquery;
  }

  // single builds a one-relation scope with no parent (the single-table UPDATE / DELETE case).
  // Subqueries ARE allowed: a correlated reference resolves to the target row via the per-row
  // outer environment (the subquery's parent is this scope), an uncorrelated one folds once
  // (spec/design/grammar.md §26). SELECT builds its own scope in planSelect.
  static single(catalog: Database, t: Table): Scope {
    return new Scope([{ label: t.name.toLowerCase(), table: t, offset: 0 }], catalog, null, true);
  }

  // resolveBare resolves a bare column name against THIS scope, then OUTWARD through the parent
  // chain. Within one scope: two+ relations have it → 42702 ambiguous; exactly one → local; none
  // → fall through to the parent. A name found only in an ancestor is an outer reference (nearest
  // scope wins). 42703 only if no scope in the chain has it.
  resolveBare(name: string): Resolved {
    let found = -1;
    for (const r of this.rels) {
      const local = columnIndex(r.table, name);
      if (local >= 0) {
        if (found >= 0) throw ambiguousColumn(name);
        found = r.offset + local;
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
  if (isBytea(ty)) return { kind: "bytea" };
  if (isUuid(ty)) return { kind: "uuid" };
  if (isTimestamp(ty)) return { kind: "timestamp" };
  if (isTimestamptz(ty)) return { kind: "timestamptz" };
  return { kind: "int", ty };
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
    case "int":
      return isInteger(colTy) || isDecimal(colTy);
    case "decimal":
      return isDecimal(colTy);
    case "bool":
      return isBool(colTy);
    case "text":
      return isText(colTy) || isUuid(colTy) || isBytea(colTy) || isTimestamp(colTy) || isTimestamptz(colTy);
    case "bytea":
      return isBytea(colTy);
    case "uuid":
      return isUuid(colTy);
    case "timestamp":
      return isTimestamp(colTy);
    case "timestamptz":
      return isTimestamptz(colTy);
  }
}

// rtName is `t`'s type name, for a 42804 assignability message (the integer width is exact).
function rtName(t: ResolvedType): string {
  switch (t.kind) {
    case "int":
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
    case "null":
      return "unknown";
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
// §4.1).
export function stmtIsWrite(stmt: Statement): boolean {
  return (
    stmt.kind === "createTable" ||
    stmt.kind === "dropTable" ||
    stmt.kind === "createIndex" ||
    stmt.kind === "dropIndex" ||
    stmt.kind === "insert" ||
    stmt.kind === "update" ||
    stmt.kind === "delete"
  );
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
    const nodes: RExpr[] = [];
    const names: string[] = [];
    const types: ResolvedType[] = [];
    for (const r of scope.rels) {
      r.table.columns.forEach((c, i) => {
        nodes.push({ kind: "column", index: r.offset + i });
        names.push(c.name);
        types.push(resolvedTypeOf(c.type));
      });
    }
    return { nodes, names, types };
  }
  const nodes: RExpr[] = [];
  const names: string[] = [];
  const types: ResolvedType[] = [];
  for (const it of items.items) {
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
  // relation or (correlated) an enclosing one — columnOf handles both.
  if (e.kind === "column") return scope.columnOf(scope.resolveBare(e.name)).name;
  if (e.kind === "qualifiedColumn") return scope.columnOf(scope.resolveQualified(e.qualifier, e.name)).name;
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
  return { node: { kind: "outerColumn", level: r.level, index: r.index }, type: resolvedTypeOf(scope.columnOf(r).type) };
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
  return scope.catalog.planQuery(inner, scope, params);
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
      // Resolve against the scope CHAIN (§26). A Local match obeys the grouping rule; an Outer
      // (correlated) match is a per-outer-row constant exempt from it (resolveColumnRef).
      return resolveColumnRef(scope, ag, scope.resolveBare(e.name), e.name);
    }
    case "qualifiedColumn": {
      return resolveColumnRef(scope, ag, scope.resolveQualified(e.qualifier, e.name), e.name);
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
      // timestamp casts are deferred (spec/design/timestamp.md §6): casting TO a datetime is 0A000.
      if (isTimestamp(target) || isTimestamptz(target)) {
        throw engineError("feature_not_supported", "casting to a timestamp type is not supported yet");
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
      // Casting FROM a timestamp is likewise deferred (0A000).
      if (inner.type.kind === "timestamp" || inner.type.kind === "timestamptz") {
        throw engineError("feature_not_supported", "casting from a timestamp type is not supported yet");
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
  // timestamp / timestamptz compare only within their own family (or with NULL). A mixed
  // timestamp × timestamptz pair, or a datetime vs any other family, would need a zone, so it
  // is a 42804 type error (spec/design/timestamp.md §5).
  const tsL = lt.kind === "timestamp" || lt.kind === "timestamptz";
  const tsR = rt.kind === "timestamp" || rt.kind === "timestamptz";
  if ((tsL || tsR) && lt.kind !== rt.kind && lt.kind !== "null" && rt.kind !== "null") {
    throw typeError("cannot compare a timestamp value with a value of a different type");
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
  if (t.kind === "timestamp") return "timestamp";
  if (t.kind === "timestamptz") return "timestamptz";
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
  if (
    t.kind === "bool" ||
    t.kind === "text" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz"
  ) {
    throw typeError("arithmetic operators require numeric operands");
  }
}

function requireBool(t: ResolvedType, msg: string): void {
  if (
    t.kind === "int" ||
    t.kind === "text" ||
    t.kind === "decimal" ||
    t.kind === "bytea" ||
    t.kind === "uuid" ||
    t.kind === "timestamp" ||
    t.kind === "timestamptz"
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
function unifySetopColumn(a: ResolvedType, b: ResolvedType, op: SetOpKind): ResolvedType {
  if (a.kind === "null" && b.kind === "null") return { kind: "null" };
  if (a.kind === "null") return b;
  if (b.kind === "null") return a;
  if (a.kind === "int" && b.kind === "int") return { kind: "int", ty: promote(a, b) };
  if ((a.kind === "int" || a.kind === "decimal") && (b.kind === "int" || b.kind === "decimal")) {
    // at least one decimal (both-int handled above) -> decimal
    return { kind: "decimal" };
  }
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
  else if (isBool(colTy)) ok = t.kind === "bool" || t.kind === "null";
  else if (isBytea(colTy)) ok = t.kind === "bytea" || t.kind === "null";
  else if (isUuid(colTy)) ok = t.kind === "uuid" || t.kind === "null";
  else if (isTimestamp(colTy)) ok = t.kind === "timestamp" || t.kind === "null";
  else if (isTimestamptz(colTy)) ok = t.kind === "timestamptz" || t.kind === "null";
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
      // ... or to a timestamp column (spec/design/timestamp.md); bad input traps 22007/22008.
      if (isTimestamp(colTy)) return timestampValue(parseTimestamp(v.text));
      if (isTimestamptz(colTy)) return timestamptzValue(parseTimestamptz(v.text));
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
    case "constNull":
      return nullValue();
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
      return boolNot(evalExpr(e.operand, row, env, m));
    }
    case "arith": {
      m.charge(COSTS.operatorEval);
      const a = evalExpr(e.lhs, row, env, m);
      const b = evalExpr(e.rhs, row, env, m);
      if (a.kind === "null" || b.kind === "null") return nullValue();
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
      const isNull = evalExpr(e.operand, row, env, m).kind === "null";
      return { kind: "bool", value: isNull !== e.negated };
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
      const v0 = vals[0];
      if (e.func === "abs") {
        if (v0.kind === "int") {
          // abs over an integer: |x| then range-check at the result type's boundary
          // (abs(int16 -32768) → 22003), exactly like neg.
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
  // Timestamps order by the int64 instant (-infinity < finite < infinity).
  if (a.kind === "timestamp" && b.kind === "timestamp") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
  }
  if (a.kind === "timestamptz" && b.kind === "timestamptz") {
    return a.micros < b.micros ? -1 : a.micros > b.micros ? 1 : 0;
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
    case "uuid":
      return 6;
    case "timestamp":
      return 7;
    case "timestamptz":
      return 8;
    default:
      return 9;
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
