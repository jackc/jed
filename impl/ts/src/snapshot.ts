// Snapshot is an immutable committed (or in-progress working) database state — the catalog + each
// table's store + the commit counter (spec/design/transactions.md §2). The committed state is one
// of these; a write transaction builds a new one from it (the persistent stores clone O(1) —
// pmap.ts / §3). A reader holds a Snapshot and is thereby stable for its life: a later commit
// produces a new Snapshot and never mutates this one. (JavaScript has no shared-memory threads for
// live objects, so P5.3b gives snapshot ISOLATION across async interleavings, not CPU parallelism.)
import type {
  ColType,
  CompositeType,
  DefaultExpr,
  IndexDef,
  SequenceDef,
  Table,
} from "./catalog.ts";
import { indexColumnOrdinals } from "./catalog.ts";
import { TableStore } from "./storage.ts";
import type { Collation } from "./collation.ts";
import type { GistOpclass, GistTree } from "./gist.ts";
import type { SharedPaging } from "./paging.ts";
import { cloneStores } from "./scope.ts";
import {
  MAX_COMPOSITE_DEPTH,
  encodePkKey,
  gistOpclassFor,
  indexEntryKeysColumns,
} from "./executor.ts";
import { buildGistFromLeafKeys } from "./gist.ts";
import { loadedCollation, versionSkew } from "./collation.ts";
import { engineError } from "./errors.ts";
import type { Entry, Row } from "./storage.ts";
import { compareBytes } from "./pmap.ts";
import { pagePayload } from "./format.ts";
import { compositeRefName } from "./types.ts";
import type { FkDependent } from "./executor.ts";
import type { Type } from "./types.ts";
import { resolveColType } from "./catalog.ts";
import type { PrivilegeSet } from "./privileges.ts";
import type { ColumnStatistics } from "./statistics.ts";
export class Snapshot {
  // txid is the snapshot's version — the commit counter (transactions.md §8; the watermark unit).
  txid: bigint;
  // catGen is the catalog generation — a monotonic counter bumped by every schema mutation
  // (CREATE/DROP/ALTER of a table/type/index), carried forward across clone(). Unlike txid it does
  // NOT move on data writes and is defined for in-memory databases too, so a prepared statement's
  // plan cache keys its committed-plan validity on it: a cached plan is reusable iff the read
  // prepared statement's relation signature includes it alongside database identity, table name,
  // and estimator revision (spec/design/api.md §2.4). NOT bumped by sequence nextval (a data write on
  // the nextval path), only by sequence DDL — a SELECT plan binds no sequence.
  catGen: bigint = 0n;
  // Exact, opaque prepared-cache identity/revision tokens (estimator.md §6). They are never
  // serialized or rendered. A clone shares them until a relevant table mutation replaces its
  // revision; a fresh create/open/attachment starts with a fresh database identity.
  estimatorIdentity: object = {};
  private estimatorBaseRevision: object = {};
  private estimatorRevisions: Map<string, object> = new Map();
  statistics: Map<string, Map<number, ColumnStatistics>> = new Map();
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
  // gistTrees holds each GiST index's resident R-tree (spec/design/gist.md §4.1), keyed by lowercased
  // index name. The leaf-key store (indexStores) stays the maintained source of truth; this tree is
  // the acceleration structure the planner descends. Rebuilt CANONICALLY (buildGistFromLeafKeys —
  // content-deterministic, gist.md §3) at every mutating statement and on load. Never mutated in
  // place (replaced wholesale on rebuild), so clone shallow-copies it.
  gistTrees: Map<string, GistTree> = new Map();
  // storePaging is this snapshot's domain paging context — the pager a store created IN-SESSION
  // (putTableResolved / putIndexStore / putIndex) binds at creation, so it joins the post-commit
  // residency flip (demoteCleanLeaves) instead of staying a fully-resident decoded tree forever.
  // Every domain sets it: the main file/in-memory snapshot binds the storage identity's paging at
  // load/create (format.ts / file.ts / opfs.ts), a session-local temp snapshot its per-domain
  // MemoryBlockStore pager (spec/design/temp-tables.md §6), an attachment its own storage's pager.
  // null only on a bare scratch engine that never persists. Stores loaded FROM a file attach the
  // same pager individually at load; binding at creation is what covers the stores load never sees.
  // Carried through clone() so a tx's working snapshot creates stores against the same domain page
  // space. NEVER serialized.
  storePaging: SharedPaging | null = null;

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
    const c = new Snapshot(
      this.txid,
      new Map(this.tables),
      cloneStores(this.stores),
      cloneStores(this.indexStores),
      new Map(this.types),
      new Map(this.sequences),
      new Map(this.collations),
      this.defaultCollation,
    );
    // GiST trees are never mutated in place — only replaced wholesale — so a shallow Map copy is safe.
    c.gistTrees = new Map(this.gistTrees);
    c.catGen = this.catGen;
    c.estimatorIdentity = this.estimatorIdentity;
    c.estimatorBaseRevision = this.estimatorBaseRevision;
    c.estimatorRevisions = new Map(this.estimatorRevisions);
    c.statistics = new Map(
      [...this.statistics].map(([table, columns]) => [
        table,
        new Map(
          [...columns].map(([column, statistics]) => [
            column,
            {
              ...statistics,
              mcv: statistics.mcv.slice(),
              histogram: statistics.histogram.slice(),
            },
          ]),
        ),
      ]),
    );
    // The temp domain's paging is shared by reference (one pool per domain), like a store's paging.
    c.storePaging = this.storePaging;
    return c;
  }

  // bumpCatGen advances the catalog generation — called by every schema mutator (see catGen). A
  // SELECT plan cached against a prior generation is thereby invalidated on the next execute.
  bumpCatGen(): void {
    this.catGen += 1n;
  }

  estimatorRevisionFor(name: string): object {
    return this.estimatorRevisionForKey(name.toLowerCase());
  }

  estimatorRevisionForKey(key: string): object {
    return this.estimatorRevisions.get(key) ?? this.estimatorBaseRevision;
  }

  bumpEstimatorRevision(name: string): void {
    this.estimatorRevisions.set(name.toLowerCase(), {});
  }

  columnStatistics(table: string, column: number): ColumnStatistics | undefined {
    return this.statistics.get(table.toLowerCase())?.get(column);
  }

  putColumnStatistics(table: string, column: number, statistics: ColumnStatistics): void {
    const key = table.toLowerCase();
    let columns = this.statistics.get(key);
    if (columns === undefined) {
      columns = new Map();
      this.statistics.set(key, columns);
    }
    columns.set(column, statistics);
  }

  markStatisticsStale(table: string): void {
    const columns = this.statistics.get(table.toLowerCase());
    if (columns === undefined) return;
    for (const statistics of columns.values()) statistics.stale = true;
  }

  clearStatistics(table: string): void {
    this.statistics.delete(table.toLowerCase());
  }

  clearColumnStatistics(table: string, column: number): void {
    const key = table.toLowerCase();
    const columns = this.statistics.get(key);
    if (columns === undefined) return;
    columns.delete(column);
    if (columns.size === 0) this.statistics.delete(key);
  }

  // gistTreeFor returns the resident GiST R-tree of the named index (lowercased key), or undefined if
  // the index is not GiST / not present (spec/design/gist.md §4.1).
  gistTreeFor(nameKey: string): GistTree | undefined {
    return this.gistTrees.get(nameKey);
  }

  // rebuildGistTrees rebuilds EVERY GiST index's resident R-tree from its leaf-key store
  // (spec/design/gist.md §3/§4.1). Called after any statement that may have changed a GiST index's
  // leaf set (the mutating-statement hook) and on load, so the working snapshot always carries a fresh
  // tree a subsequent read descends. Each tree is built CANONICALLY (buildGistFromLeafKeys), making it
  // a pure function of the leaf SET — content-deterministic, cross-core identical, and identical to the
  // on-disk persisted R-tree. Trees whose index has been dropped are removed.
  rebuildGistTrees(): void {
    const specs: { nameKey: string; ops: GistOpclass[] }[] = [];
    for (const t of this.tables.values()) {
      for (const idx of t.indexes) {
        if (idx.kind !== "gist") continue;
        // One opclass per indexed column (gist.md §7): single for a GX1/GX2 index, one per WITH
        // column for an EXCLUDE backing index.
        specs.push({
          nameKey: idx.name.toLowerCase(),
          ops: indexColumnOrdinals(idx)!.map((ci) => gistOpclassFor(t.columns[ci]!.type)),
        });
      }
    }
    const live = new Set(specs.map((s) => s.nameKey));
    for (const k of [...this.gistTrees.keys()]) if (!live.has(k)) this.gistTrees.delete(k);
    for (const sp of specs) {
      const store = this.indexStores.get(sp.nameKey);
      const keys = store ? store.entriesInKeyOrder().map((e) => e.key) : [];
      this.gistTrees.set(sp.nameKey, buildGistFromLeafKeys(sp.ops, keys));
    }
  }

  // demoteCleanLeaves demotes every store's clean, persisted resident leaves to OnDisk references —
  // the post-commit residency flip over the whole snapshot (bplus-reshape.md B4), run after a
  // successful persist so the published committed tree is the skeletal `interiors + OnDisk leaves`
  // shape on every host. Table stores and btree/GIN index stores flip; a GiST leaf-key store's
  // nodes are never persisted (its on-disk form is the R-tree) and a store with no paging context
  // (a table created in-session, a temp store) has nothing to fault from — both no-op inside
  // TableStore.demoteCleanLeaves. Map iteration order is irrelevant (each store flips
  // independently), so no order leak (CLAUDE.md §8).
  demoteCleanLeaves(): void {
    for (const store of this.stores.values()) store.demoteCleanLeaves();
    for (const store of this.indexStores.values()) store.demoteCleanLeaves();
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

  tableByKey(key: string): Table | undefined {
    return this.tables.get(key);
  }

  // compositeType looks up a composite type definition by name (case-insensitive).
  compositeType(name: string): CompositeType | undefined {
    return this.types.get(name.toLowerCase());
  }

  // putType registers a composite type (CREATE TYPE). Lower-cased name is the key. The caller has
  // already resolved field types and checked for a duplicate.
  putType(ty: CompositeType): void {
    this.bumpCatGen();
    this.types.set(ty.name.toLowerCase(), ty);
  }

  // removeType removes a composite type (DROP TYPE). The caller has checked there are no dependents.
  removeType(key: string): void {
    this.bumpCatGen();
    this.types.delete(key);
  }

  // tablesSorted is all tables in ascending lowercased-name order — a deterministic order with no
  // map-iteration leak (CLAUDE.md §8); the jed_tables / jed_columns generation order
  // (spec/design/introspection.md §5).
  tablesSorted(): Table[] {
    return [...this.tables.keys()].sort().map((k) => this.tables.get(k)!);
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
      const indexes = table.indexes.filter((idx) => {
        if (pkSkewed) return true;
        const cols = indexColumnOrdinals(idx);
        return cols?.some((c) => isSkewed(table.columns[c]!.collation));
      });
      for (let column = 0; column < table.columns.length; column++)
        if (isSkewed(table.columns[column]!.collation)) this.clearColumnStatistics(key, column);
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
      // Rebuild each affected index store from the (re-keyed) rows. The realign runs on a Snapshot
      // with no Engine to evaluate an expression key; an expression index is C-collated so its keys
      // never change on a collation upgrade, but a pkSkewed re-key moves its suffix — that (rare)
      // rebuild is unsupported here (0A000; drop the expression index, upgrade, recreate —
      // indexes.md §7). Uses the column-only entry builder.
      for (const def of indexes) {
        if (indexColumnOrdinals(def) === null) {
          throw engineError(
            "feature_not_supported",
            "collation upgrade of a table with an expression index is not supported yet",
          );
        }
        // A PARTIAL index likewise needs the Engine to evaluate its predicate per row, so the realign
        // bails the same way (indexes.md §9).
        if (def.predicate !== undefined) {
          throw engineError(
            "feature_not_supported",
            "collation upgrade of a table with a partial index is not supported yet",
          );
        }
        const ekeys: Uint8Array[] = [];
        for (const e of entries)
          ekeys.push(...indexEntryKeysColumns(table.columns, colls, def, e.key, e.row));
        ekeys.sort(compareBytes);
        const fresh = new TableStore(pagePayload(pageSize), []);
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

  // fkDependentsExcluding returns every FK on a table NOT in `dropping` (a set of lowercased table
  // keys) that references a table that IS in `dropping` — the dependency scan for a multi-table
  // DROP TABLE (spec/design/grammar.md §13, constraints.md §6.10). A dependent whose referencing
  // table is itself being dropped does not count (the drop-set exclusion), so a FK between two
  // tables both named in the same statement never blocks. Referencing tables are scanned in
  // ascending lowercased key order (each table's fks is already name-ordered) for determinism (§8).
  // RESTRICT raises 2BP01 on the first entry; CASCADE removes every entry's FK.
  fkDependentsExcluding(dropping: Set<string>): FkDependent[] {
    const out: FkDependent[] = [];
    const tkeys = [...this.tables.keys()].sort();
    for (const tk of tkeys) {
      if (dropping.has(tk)) continue; // the referencing table is itself being dropped — no dependency
      const t = this.tables.get(tk)!;
      for (const fk of t.fks) {
        const refKey = fk.refTable.toLowerCase();
        if (dropping.has(refKey)) {
          const droppedName = this.tables.get(refKey)?.name ?? fk.refTable;
          out.push({
            refTableKey: tk,
            fkName: fk.name,
            refTableName: t.name,
            droppedName,
          });
        }
      }
    }
    return out;
  }

  // removeForeignKey removes the named FK constraint from `tableKey` in place — re-allocating the
  // Table + its fks array (catalog Tables are never mutated in place, snapshots share them) so the
  // committed snapshot is untouched, preserving the table's store and rows. DROP TABLE … CASCADE's
  // removal of a dependent FK on a table that survives the drop (spec/design/grammar.md §13). An FK
  // owns no B-tree (constraints.md §6), so only the catalog list changes.
  removeForeignKey(tableKey: string, fkName: string): void {
    const old = this.tables.get(tableKey);
    if (old === undefined) return;
    this.bumpCatGen();
    const fks = old.fks.filter((fk) => fk.name.toLowerCase() !== fkName.toLowerCase());
    this.tables.set(tableKey, { ...old, fks });
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
  // page_size − 16) and the column types so the page-backed B-tree can weigh records for its
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
    this.bumpCatGen();
    const key = t.name.toLowerCase();
    const st = new TableStore(pagePayload(pageSize), colTypes);
    // Bind the domain's pager (Snapshot.storePaging) so the new store demand-pages like a loaded one:
    // its committed leaves demote at each commit (demoteCleanLeaves) and fault back through the pool,
    // instead of staying fully-resident decoded for the handle's lifetime. null only on a bare scratch
    // engine that never persists.
    if (this.storePaging !== null) st.attachPaging(this.storePaging);
    this.stores.set(key, st);
    this.tables.set(key, t);
    this.estimatorRevisions.delete(key);
    this.statistics.delete(key);
  }

  // removeTable removes a table's definition, its store, and its indexes' stores (DROP
  // TABLE — the indexes have no independent life, spec/design/indexes.md §2).
  removeTable(key: string): void {
    this.bumpCatGen();
    const t = this.tables.get(key);
    if (t) for (const idx of t.indexes) this.indexStores.delete(idx.name.toLowerCase());
    this.tables.delete(key);
    this.stores.delete(key);
    this.estimatorRevisions.delete(key);
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
    this.bumpCatGen();
    const nameKey = def.name.toLowerCase();
    const fresh = new TableStore(pagePayload(pageSize), []);
    // Bind the domain pager, like putTableResolved / putIndexStore.
    if (this.storePaging !== null) fresh.attachPaging(this.storePaging);
    this.indexStores.set(nameKey, fresh);
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
    this.bumpCatGen();
    const columns = old.columns.map((c, i) => (i === column ? { ...c, defaultExpr } : c));
    this.tables.set(tableKey, { ...old, columns });
  }

  // Publish one validated ALTER TABLE catalog entry without touching row bytes.
  alterTableCatalog(
    oldKey: string,
    table: Table,
    renameTable: boolean,
    indexRename?: { oldName: string; newName: string },
  ): void {
    this.bumpCatGen();
    const newKey = renameTable ? table.name.toLowerCase() : oldKey;
    if (renameTable) {
      this.tables.delete(oldKey);
      const store = this.stores.get(oldKey);
      if (store) {
        this.stores.delete(oldKey);
        this.stores.set(newKey, store);
      }
      const statistics = this.statistics.get(oldKey);
      if (statistics !== undefined) {
        this.statistics.delete(oldKey);
        this.statistics.set(newKey, statistics);
      }
    }
    this.tables.set(newKey, table);
    if (indexRename) {
      const old = indexRename.oldName.toLowerCase();
      const next = indexRename.newName.toLowerCase();
      const store = this.indexStores.get(old);
      if (store) {
        this.indexStores.delete(old);
        this.indexStores.set(next, store);
      }
      const tree = this.gistTrees.get(old);
      if (tree) {
        this.gistTrees.delete(old);
        this.gistTrees.set(next, tree);
      }
    }
    if (!renameTable) return;
    for (const [key, old] of [...this.tables]) {
      if (!old.fks.some((fk) => fk.refTable.toLowerCase() === oldKey)) continue;
      this.tables.set(key, {
        ...old,
        fks: old.fks.map((fk) =>
          fk.refTable.toLowerCase() === oldKey ? { ...fk, refTable: table.name } : fk,
        ),
      });
    }
    for (const [key, seq] of [...this.sequences]) {
      if (seq.ownedBy?.table.toLowerCase() === oldKey) {
        this.sequences.set(key, { ...seq, ownedBy: { ...seq.ownedBy, table: table.name } });
      }
    }
  }

  alterTableRewrite(
    table: Table,
    colTypes: ColType[],
    entries: Entry[],
    nextRowid: bigint,
    pageSize: number,
  ): void {
    const key = table.name.toLowerCase();
    const statistics = this.statistics.get(key);
    this.putTableResolved(table, colTypes, pageSize);
    if (statistics !== undefined) this.statistics.set(key, statistics);
    const store = this.store(table.name);
    store.bumpRowidTo(nextRowid);
    for (const entry of entries)
      if (!store.insert(entry.key, entry.row))
        throw new Error("ADD COLUMN retains distinct existing storage keys");
  }

  syncAlterConstraintIndexes(
    old: Table,
    next: Table,
    entries: Map<string, Uint8Array[]>,
    pageSize: number,
  ): void {
    const live = new Set(next.indexes.map((i) => i.name.toLowerCase()));
    for (const i of old.indexes)
      if (!live.has(i.name.toLowerCase())) {
        this.indexStores.delete(i.name.toLowerCase());
        this.gistTrees.delete(i.name.toLowerCase());
      }
    const prior = new Set(old.indexes.map((i) => i.name.toLowerCase()));
    for (const i of next.indexes) {
      const k = i.name.toLowerCase();
      const rebuild = entries.has(k);
      if (prior.has(k) && !rebuild) continue;
      if (rebuild) {
        this.indexStores.delete(k);
        this.gistTrees.delete(k);
      }
      const s = new TableStore(pagePayload(pageSize), []);
      if (this.storePaging !== null) s.attachPaging(this.storePaging);
      for (const e of entries.get(k) ?? []) s.insert(e, []);
      this.indexStores.set(k, s);
    }
  }

  rebuildAlterIndexes(
    old: Table,
    next: Table,
    entries: Map<string, Uint8Array[]>,
    pageSize: number,
  ): void {
    for (const index of old.indexes) {
      const key = index.name.toLowerCase();
      this.indexStores.delete(key);
      this.gistTrees.delete(key);
    }
    for (const index of next.indexes) {
      const key = index.name.toLowerCase();
      const store = new TableStore(pagePayload(pageSize), []);
      if (this.storePaging !== null) store.attachPaging(this.storePaging);
      for (const entry of entries.get(key) ?? []) store.insert(entry, []);
      this.indexStores.set(key, store);
    }
  }

  // Repair incoming FK ordinals and owned-sequence links after DROP COLUMN compacts a table's
  // dense ordinals. -1 means CASCADE removed the referenced column.
  remapAlterColumnDependents(
    parent: string,
    originalToNew: number[],
    pendingSequences: Set<string>,
  ): void {
    for (const [key, child] of [...this.tables]) {
      if (child.name.toLowerCase() === parent.toLowerCase()) continue;
      let changed = false;
      const fks = child.fks.flatMap((fk) => {
        if (fk.refTable.toLowerCase() !== parent.toLowerCase()) return [fk];
        const mapped = fk.refColumns.map((old) => originalToNew[old] ?? -1);
        if (mapped.some((column) => column < 0)) {
          changed = true;
          return [];
        }
        if (mapped.some((column, i) => column !== fk.refColumns[i])) changed = true;
        return [{ ...fk, refColumns: mapped }];
      });
      if (changed) this.tables.set(key, { ...child, fks });
    }
    for (const [key, seq] of [...this.sequences]) {
      if (
        pendingSequences.has(key) ||
        seq.ownedBy === undefined ||
        seq.ownedBy.table.toLowerCase() !== parent.toLowerCase()
      )
        continue;
      const column = originalToNew[seq.ownedBy.column] ?? -1;
      if (column < 0) this.sequences.delete(key);
      else this.sequences.set(key, { ...seq, ownedBy: { ...seq.ownedBy, column } });
    }
  }

  // putIndexStore registers a loaded index store under its (lowercased) name — the file
  // loader's hook (format.ts): the owning table's indexes list came from its catalog
  // entry, so only the store is registered here.
  putIndexStore(nameKey: string, store: TableStore): void {
    // An index store created in-session binds the domain's pager like a table store (putTableResolved)
    // so it joins the post-commit residency flip; a store loaded from a file already attached it.
    if (this.storePaging !== null && !store.isFileBacked()) store.attachPaging(this.storePaging);
    this.indexStores.set(nameKey, store);
  }

  // removeIndex removes one secondary index (DROP INDEX): its definition from the owning
  // table and its store.
  removeIndex(tableKey: string, nameKey: string): void {
    this.bumpCatGen();
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
export type ActiveTx = {
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
  // mainDirty is whether this transaction mutated the MAIN (persistent) snapshot — set by working().
  // Drives the commit's persist decision so a transaction that touched ONLY session-local temp tables
  // makes zero file writes (temp-tables.md §2). tempDirty mirrors it for the temp snapshot.
  mainDirty: boolean;
  tempDirty: boolean;
  // attachWorking is the transaction's working copy of a host-attached database's snapshot
  // (spec/design/attached-databases.md §5), keyed by lowercased attachment name — the attachment
  // analogue of tempWorking. Cloned lazily from Engine.attachedCommitted[name] on the first write to
  // that attachment (attachWriteSnap), so a read-only cross-attachment query allocates nothing here.
  // Adopted into attachedCommitted + persisted+published on a successful COMMIT, discarded on ROLLBACK.
  // undefined until an attachment is written.
  attachWorking?: Map<string, Snapshot>;
  // attachDirty records which attachments this transaction mutated (lowercased names), the
  // per-attachment analogue of mainDirty/tempDirty — the set the commit persists + publishes.
  attachDirty?: Set<string>;
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
  lockTimeoutMs?: number;
  // The work-memory budget in bytes before a blocking operator spills (spill.md §3). 0 (or absent) ⇒
  // the default (256 MiB), same as unset — use setWorkMem(0) for the 0 ⇒ unlimited (never-spill) form.
  // Unlike maxCost/lifetimeMaxCost, whose default genuinely is 0 ⇒ unlimited (api.md §2.1).
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

export function txStatusOf(tx: ActiveTx | null): TxStatus {
  if (tx === null) return "Idle";
  return tx.failed ? "Failed" : "Open";
}

// SessionState is the per-connection SESSION envelope (spec/design/session.md §2.1/§2.4): the
// configured, stateful context a host runs statements through, un-fused from the committed storage on
// Engine. It owns the open transaction (the Idle/Open/Failed machine), the relocated handle settings,
// the entropy/clock seam, and the currval/lastval session state. An Engine holds one as its default
// session; the host-facing Session (shared.ts) wraps an Engine and exposes this envelope, delegating
// its setters/getters here. (Pre-§2.4 this class was the exported `Session`; the convergence renamed
// it and made the per-caller handle the public `Session`.)
// requireCustomVarName validates + canonicalizes a session-variable name (spec/design/session.md
// §6.1). A variable must be namespaced like a PostgreSQL custom GUC — a dotted name (myapp.tenant); a
// non-dotted name would be a built-in setting, and v1 exposes none through this map (the time_zone
// built-in is a separate slice), so it is 42704. Returns the case-folded (lowercase, PG GUC names are
// case-insensitive) map key.
export function requireCustomVarName(name: string): string {
  if (name.includes(".")) {
    return name.toLowerCase();
  }
  throw engineError("undefined_object", "unrecognized configuration parameter: " + name);
}
