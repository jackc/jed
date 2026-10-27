// The integrity check (spec/design/integrity-check.md): a host-API walk of one database's stored
// generation that reports structured findings instead of failing on the first problem. It reads every
// page the generation reaches — catalog, table and index trees, overflow chains, the free list —
// through the handle's paging context, checks the invariants between them, and, at level `full`,
// probes every index once per entry its table derives, deriving entries through the write path.
//
// The report is a cross-core contract (§4): the same findings in the same order, the same skips and
// counters, as spec/fileformat/integrity.rb. Finding order is the walk order: generation, then each
// table in name byte order (its tree, then each index's structure and agreement), then, under
// `constraints`, each table's column pass and named constraints, then space accounting.
//
// Memory is bounded independent of rows (§5.4, CLAUDE.md §9): one reachability bitmap of page_count
// bits, one root-to-leaf path per tree being walked, and one row at a time. Index agreement re-walks
// the table and probes the index for each derived entry; the constraint pass re-walks each table (and
// each unique index, keeping only the previous entry's key prefix) and probes the referenced table for
// each foreign key; nothing materializes a table or an index.
//
// It is unmetered, like compaction: no session cost or memory account is charged.

import { NO_TAG, capFirstBody, type Cap } from "./cap.ts";
import type {
  CheckConstraint,
  ColType,
  Column,
  ForeignKey,
  HostFuncDep,
  IndexDef,
  Table,
} from "./catalog.ts";
import { indexColumnOrdinals, indexKeyOrder, resolveColType } from "./catalog.ts";
import type { Collation } from "./collation.ts";
import {
  type CommitMeta,
  crc64Update,
  manifestOverflowCapacity,
  metaChecksum,
  pageChecksum,
  parseCommitMeta,
} from "./commit_manifest.ts";
import { cancellationCheckpoint, throwIfAborted } from "./cancel.ts";
import { EngineError, engineError } from "./errors.ts";
import { COOPERATE, shouldCooperate, type Steps } from "./execution.ts";
import { Meter } from "./cost.ts";
import { type KeyOrder, keyOrderNullTag, keyOrderPresentTag } from "./encoding.ts";
import { evalExprSteps } from "./eval.ts";
import {
  encodePkKey,
  type Engine,
  fkMatchFullViolated,
  fkProbeSteps,
  gistOpclassFor,
  indexEntryKeysSteps,
  keyBodyLen,
  keySlotForScalar,
  type ResolvedIndex,
  type RExpr,
  sameSet,
  sortedUnique,
  uniqueProbeBound,
} from "./executor.ts";
import {
  catalogImagePages,
  chainPayload,
  decodeLeafValueSteps,
  encodeValue,
  isExternalTag,
  type LeafView,
  leafValueTag,
  PAGE_FREELIST,
  PAGE_TYPE_CATALOG,
  PAGE_TYPE_INTERIOR,
  PAGE_TYPE_LEAF,
  type PageFetch,
  parseInterior,
  parseLeafView,
  parsePage,
  type TreeRef,
} from "./format.ts";
import { ginDecodeRun, ginRunAtOrAfterSteps, ginRunPostings } from "./gin_runs.ts";
import {
  type GistBound,
  type GistOpclass,
  gistBoundContains,
  gistDecodeBound,
  gistSplitLeafKey,
} from "./gist.ts";
import { verifyIndexTimezoneDeps } from "./index_dependencies.ts";
import type { IntegrityKindName, IntegritySkipReasonName } from "./integrity_kinds.ts";
import type { SharedPaging } from "./paging.ts";
import { compareBytes } from "./pmap.ts";
import type { Snapshot } from "./snapshot.ts";
import type { Row, TableStore } from "./storage.ts";
import { indexUnparsed } from "./unparsed.ts";
import {
  adjustIntervalTypmod,
  adjustTimestampTypmod,
  coerceDecimal,
  coerceVarcharStore,
} from "./store.ts";
import { compareTextC, decimalValue, intervalValue, textValue, type Value } from "./value.ts";

export {
  IntegrityKind,
  type IntegrityKindName,
  IntegritySkipReason,
  type IntegritySkipReasonName,
} from "./integrity_kinds.ts";

/** The checks to run (spec/design/integrity-check.md §3): `quick` omits index agreement. */
export type IntegrityLevel = "quick" | "full";

/** Options of `integrityCheck` (spec/design/integrity-check.md §2). */
export type IntegrityCheckOptions = {
  /** `quick` or `full` (the default). */
  level?: IntegrityLevel;
  /**
   * Also re-check declared constraints against the stored rows (default false): NOT NULL and column
   * typmods, CHECK, uniqueness of every unique B-tree index, and FOREIGN KEY (§3.4). Independent of
   * `level`; a CHECK may need host functions or loaded data the handle lacks, which it then skips.
   */
  constraints?: boolean;
  /** Stop after this many findings (default 100); 0 means no limit. */
  maxFindings?: number;
  /** Cancels the check with 57014; a canceled check returns no report. */
  signal?: AbortSignal;
};

/** One thing wrong with the database (spec/design/integrity-check.md §2, §4). */
export type IntegrityFinding = {
  kind: IntegrityKindName;
  database: string;
  object: string | null;
  page: number | null;
  key: Uint8Array | null;
  /** Human-readable; not part of the contract. */
  detail: string;
};

/** A requested check that could not run on an object, and why (§3.5). */
export type IntegritySkip = { object: string; reason: IntegritySkipReasonName };

/**
 * The result of `integrityCheck`. `pages` and `rows` are numbers: both are bounded by the u32 page
 * space (a page holds at most a few thousand records), far below 2^53.
 */
export type IntegrityReport = {
  findings: IntegrityFinding[];
  truncated: boolean;
  skipped: IntegritySkip[];
  /** Distinct pages the walk reached, plus the free-list pages it read. */
  pages: number;
  /** Records visited in table leaves. */
  rows: number;
  /** No findings and not truncated. */
  ok: boolean;
};

// IntegrityTarget is what both callers (Database.integrityCheck, the simulation's image checker) have:
// the pinned snapshot and its domain's storage. durable selects where the generation's facts live: a
// durable store (file, OPFS, page client) reads them from the meta slot and the persisted free list; a
// never-reopened in-memory store writes a meta without a live count, manifest or free list, and keeps
// its free list in RAM (persist.ts commitInMemory).
export type IntegrityTarget = {
  database: string;
  snap: Snapshot;
  storage: Engine;
  durable: boolean;
};

// Truncated stops the walk when max_findings is reached (§4).
class Truncated {}

// Reach is thrown by the overflow fetch when a chain names a page the walk must not read.
class Reach {
  readonly kind: "page_out_of_range" | "page_reached_twice";
  readonly page: number;
  constructor(kind: "page_out_of_range" | "page_reached_twice", page: number) {
    this.kind = kind;
    this.page = page;
  }
}

// fatal reports whether an error ends the check instead of becoming a finding: a cancellation, a
// device or connection error, a poisoned handle (§2), and any error that is not the codec's report of
// bad stored bytes (a plain JS error is a bug, never a finding). XX001 and the codec's class-22
// decode errors are findings.
function fatal(e: unknown): boolean {
  if (!(e instanceof EngineError)) return true;
  return e.code() !== "XX001" && !e.code().startsWith("22");
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// The generation the check examines: its meta facts and the pages its manifest depends on.
type Generation = {
  rootPage: number;
  rootTag: Uint8Array;
  pageCount: number;
  freeHead: number;
  freeTag: Uint8Array;
  livePages: number | null; // null: not recorded (an in-memory store)
  protectedPages: Set<number>;
  memoryFree: number[] | null; // an in-memory store's RAM free list
};

// One B+tree record handed to a table walk's visitor.
type Visit = (page: number, leaf: LeafView, i: number) => Steps<void> | undefined;

class Checker {
  private readonly findings: IntegrityFinding[] = [];
  private readonly skipped: IntegritySkip[] = [];
  private truncated = false;
  private rows = 0;
  private freeChainPages = 0;
  private damaged = false;
  private reached: Uint8Array = new Uint8Array(0);
  private reachedCount = 0;
  // ginPostings counts a GIN tree's postings during its walk (§3.2).
  private ginPostings = 0;
  private first = 2;
  private pageCount = 0;
  private readonly tagLen: number;
  private readonly cap: Cap;
  private readonly paging: SharedPaging;
  private readonly target: IntegrityTarget;
  private readonly level: IntegrityLevel;
  private readonly constraints: boolean;
  private readonly max: number;
  private readonly signal: AbortSignal | undefined;
  // Whether each table's tree was readable, and whether each index's walk reported nothing, as the
  // constraint stage (§3.4, after every table) needs them.
  private readonly tableReadable = new Map<string, boolean>();
  private readonly indexClean = new Map<string, boolean>();

  constructor(
    target: IntegrityTarget,
    level: IntegrityLevel,
    constraints: boolean,
    max: number,
    signal: AbortSignal | undefined,
  ) {
    this.target = target;
    this.level = level;
    this.constraints = constraints;
    this.max = max;
    this.signal = signal;
    const paging = target.storage.paging;
    if (paging === null) throw engineError("io_error", "database storage is closed");
    this.paging = paging;
    this.cap = paging.cap();
    this.tagLen = this.cap.tag;
  }

  private add(
    kind: IntegrityKindName,
    object: string | null,
    page: number | null,
    key: Uint8Array | null,
    detail: string,
  ): void {
    if (this.max !== 0 && this.findings.length === this.max) {
      this.truncated = true;
      throw new Truncated();
    }
    this.findings.push({
      kind,
      database: this.target.database,
      object,
      page,
      key: key === null ? null : key.slice(),
      detail,
    });
  }

  // poll is the cancellation point at every page read and row (§5.3).
  private *poll(): Steps<void> {
    if (shouldCooperate()) yield COOPERATE;
    cancellationCheckpoint();
    throwIfAborted(this.signal);
  }

  private isReached(p: number): boolean {
    return p >= 0 && p < this.pageCount && (this.reached[p >> 3]! & (1 << (p & 7))) !== 0;
  }

  private mark(p: number): void {
    this.reached[p >> 3]! |= 1 << (p & 7);
    this.reachedCount++;
  }

  private inRange(p: number): boolean {
    return p >= this.first && p < this.pageCount;
  }

  // reach records a page as reached, reporting (and returning false) when the walk must not read it.
  private reach(object: string, page: number): boolean {
    if (!this.inRange(page)) {
      this.add(
        "page_out_of_range",
        object,
        page,
        null,
        `page ${page} outside [${this.first}, ${this.pageCount})`,
      );
      return false;
    }
    if (this.isReached(page)) {
      this.add(
        "page_reached_twice",
        object,
        page,
        null,
        `page ${page} is already part of the database`,
      );
      return false;
    }
    this.mark(page);
    return true;
  }

  // readPage reads and verifies one body page through the handle's pager.
  private *readPage(page: number, tag: Uint8Array): Steps<ReturnType<typeof parsePage>> {
    yield* this.poll();
    return parsePage(yield* this.paging.readBodySteps(page, tag));
  }

  *run(): Steps<IntegrityReport> {
    try {
      const gen = yield* this.generation();
      if (gen !== null) {
        this.pageCount = gen.pageCount;
        this.first = capFirstBody(this.cap);
        this.reached = new Uint8Array(Math.ceil(gen.pageCount / 8));
        yield* this.catalog(gen);
        this.sequences();
        const snap = this.target.snap;
        const names = [...snap.tables.keys()].sort(compareTextC);
        for (const name of names) yield* this.table(name);
        if (this.constraints) for (const name of names) yield* this.tableConstraints(name);
        yield* this.freeList(gen);
        if (!this.damaged && gen.livePages !== null && gen.livePages !== this.reachedCount) {
          this.add(
            "live_count_mismatch",
            null,
            null,
            null,
            `meta records ${gen.livePages} live pages, ${this.reachedCount} are reachable`,
          );
        }
      } else this.sequences();
    } catch (e) {
      if (!(e instanceof Truncated)) throw e;
    }
    return {
      findings: this.findings,
      truncated: this.truncated,
      skipped: this.skipped,
      pages: this.reachedCount + this.freeChainPages,
      rows: this.rows,
      ok: this.findings.length === 0 && !this.truncated,
    };
  }

  // --- generation (§3.1) ---

  // generation selects the pinned generation's meta slot and re-verifies it and its manifest. It
  // returns null when no slot carries the pinned generation in a form the walk can follow.
  private *generation(): Steps<Generation | null> {
    const st = this.target.storage;
    const paging = this.paging;
    const ps = paging.pageSize();
    const txid = this.target.snap.txid;
    const blocks = [yield* paging.readPageSteps(0), yield* paging.readPageSteps(1)];
    const rawTxid = (b: Uint8Array): bigint | null =>
      b.length >= 20 ? new DataView(b.buffer, b.byteOffset, b.byteLength).getBigUint64(12) : null;
    if (!this.target.durable) {
      // A never-reopened in-memory store keeps its generation's facts in RAM: the last commit's catalog
      // root and the high-water (an in-memory attachment's commit writes no meta at all — persist.ts
      // stageInMemory), and its free list. Before any commit, and after compaction, the meta of its
      // from-scratch image names the root.
      let root = st.storageBudget.lastCatRoot;
      if (root === 0) {
        const metas = [0, 1]
          .map((s) => parseCommitMeta(blocks[s]!, ps, paging.physicalPages()))
          .filter((m): m is CommitMeta => m !== null)
          .sort((a, b) => (a.txid > b.txid ? -1 : a.txid < b.txid ? 1 : 0));
        if (metas.length === 0) return null;
        root = metas[0]!.rootPage;
      }
      return {
        rootPage: root,
        rootTag: NO_TAG,
        pageCount: st.pageCount,
        freeHead: 0,
        freeTag: NO_TAG,
        livePages: null,
        protectedPages: new Set(),
        memoryFree: st.freePages.slice(),
      };
    }
    // The slot holding the pinned generation: the first of the slots carrying its txid whose meta
    // verifies, as open selects (both slots carry it after a from-scratch write).
    const holders = [0, 1].filter((s) => rawTxid(blocks[s]!) === txid);
    const physical = paging.physicalPages();
    const trusted = paging.trustsMeta();
    let slot = holders.find((s) => parseCommitMeta(blocks[s]!, ps, physical) !== null);
    let meta: CommitMeta | null =
      slot === undefined ? null : parseCommitMeta(blocks[slot]!, ps, physical);
    if (meta === null) {
      slot = holders[0] ?? Number(txid & 1n);
      if (!trusted)
        this.add("meta_invalid", null, slot, null, `meta slot ${slot} no longer verifies`);
      if (holders.length === 0) return null;
      // Follow the generation the handle pinned from the slot's fields as written.
      const patched = blocks[slot]!.slice();
      new DataView(patched.buffer).setUint32(32, metaChecksum(patched), false);
      meta = parseCommitMeta(patched, ps, 0xffff_ffff);
      if (meta === null) return null;
    }
    const block = blocks[slot!]!;
    const protectedPages = yield* this.manifest(meta, slot!, block, !trusted);
    return {
      rootPage: meta.rootPage,
      rootTag: meta.rootTag,
      pageCount: meta.pageCount,
      freeHead: meta.freeListHead,
      freeTag: meta.freeListTag,
      livePages: meta.livePages,
      protectedPages,
      memoryFree: null,
    };
  }

  // manifest re-reads the generation's commit manifest (validated-cow.md §1): each listed page and
  // each manifest-overflow page must carry its recorded identity. It returns the pages the generation
  // depends on (listed and overflow pages), which the free list must not name.
  private *manifest(
    meta: CommitMeta,
    slot: number,
    block: Uint8Array,
    report: boolean,
  ): Steps<Set<number>> {
    const deps = new Set<number>();
    if (meta.dirtyCount === 0) return deps;
    const paging = this.paging;
    const ps = paging.pageSize();
    const encrypted = meta.suite !== 0;
    const first = encrypted ? 4 : 2;
    const entries = meta.inline.slice();
    const chain: { index: number; bytes: Uint8Array }[] = [];
    let next = meta.overflowHead;
    let chainChecksum = 0n;
    const capacity = manifestOverflowCapacity(ps);
    let chainOk = true;
    for (let ordinal = 0; ordinal < meta.overflowCount; ordinal++) {
      if (next < first || next >= meta.pageCount || deps.has(next)) {
        if (report)
          this.add(
            "manifest_page_mismatch",
            null,
            next,
            null,
            "manifest chain leaves the body range",
          );
        chainOk = false;
        break;
      }
      yield* this.poll();
      const index = next;
      deps.add(index);
      const bytes = yield* paging.readPageSteps(index);
      chain.push({ index, bytes });
      const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const count = dv.getUint32(4, false);
      if (
        bytes[0] !== 8 ||
        dv.getUint32(12, false) !== pageChecksum(bytes) ||
        dv.getBigUint64(16, false) !== meta.txid ||
        dv.getUint32(24, false) !== ordinal ||
        count > capacity
      ) {
        if (report)
          this.add("manifest_page_mismatch", null, index, null, "manifest page does not verify");
        chainOk = false;
        break;
      }
      for (let i = 0; i < count; i++) {
        entries.push({
          index: dv.getUint32(32 + i * 12, false),
          checksum: dv.getBigUint64(36 + i * 12, false),
        });
      }
      chainChecksum = crc64Update(crc64Update(chainChecksum, u32Bytes(index)), bytes);
      next = dv.getUint32(8, false);
    }
    if (chainOk && chainChecksum !== meta.chainChecksum && report) {
      this.add(
        "manifest_page_mismatch",
        null,
        meta.overflowHead,
        null,
        "manifest chain digest differs",
      );
    }
    if (chainOk && report && encrypted) {
      const codec = paging.codec();
      if (codec !== null) {
        try {
          yield* codec.verifyMetaSteps(slot, block, chain);
        } catch (e) {
          if (fatal(e)) throw e;
          this.add("meta_invalid", null, slot, null, message(e));
        }
      }
    }
    for (const entry of entries) {
      deps.add(entry.index);
      if (!report) continue;
      if (entry.index < first || entry.index >= meta.pageCount) {
        this.add(
          "manifest_page_mismatch",
          null,
          entry.index,
          null,
          "listed page outside the body range",
        );
        continue;
      }
      yield* this.poll();
      const bytes = yield* paging.readPageSteps(entry.index);
      const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      if (
        crc64Update(0n, bytes) !== entry.checksum ||
        (!encrypted &&
          (bytes[0]! < 1 || bytes[0]! > 7 || pageChecksum(bytes) !== dv.getUint32(12, false)))
      ) {
        this.add(
          "manifest_page_mismatch",
          null,
          entry.index,
          null,
          `page ${entry.index} no longer carries its recorded identity`,
        );
      }
    }
    return deps;
  }

  // catalog re-reads the stored catalog chain, reaching its pages first, and compares it with the
  // encoding of the snapshot's catalog (§3.1). The walk then continues from the snapshot's catalog.
  private *catalog(gen: Generation): Steps<void> {
    const stored: { page: number; itemCount: number; continued: boolean; content: Uint8Array }[] =
      [];
    let page = gen.rootPage;
    let tag = gen.rootTag;
    while (page !== 0) {
      if (!this.inRange(page) || this.isReached(page)) {
        this.damaged = true;
        this.add("catalog_page_corrupt", null, page, null, `catalog chain names page ${page}`);
        return;
      }
      this.mark(page);
      let content: { next: number; nextTag: Uint8Array; content: Uint8Array };
      let pg: ReturnType<typeof parsePage>;
      try {
        pg = yield* this.readPage(page, tag);
        if (pg.pageType !== PAGE_TYPE_CATALOG || (pg.flags & ~1) !== 0)
          throw engineError("data_corrupted", "expected a catalog page");
        content = chainPayload(pg, this.tagLen);
      } catch (e) {
        if (fatal(e)) throw e;
        this.damaged = true;
        this.add("catalog_page_corrupt", null, page, null, message(e));
        return;
      }
      stored.push({
        page,
        itemCount: pg.itemCount,
        continued: (pg.flags & 1) !== 0,
        content: content.content,
      });
      page = content.next;
      tag = content.nextTag;
    }
    let expected: ReturnType<typeof catalogImagePages>;
    try {
      expected = catalogImagePages(this.target.snap, this.cap);
    } catch (e) {
      if (fatal(e)) throw e;
      this.add("catalog_mismatch", null, gen.rootPage, null, message(e));
      return;
    }
    for (let i = 0; i < Math.max(stored.length, expected.length); i++) {
      const s = stored[i];
      const want = expected[i];
      if (s === undefined) {
        const last = stored[stored.length - 1]?.page ?? gen.rootPage;
        this.add("catalog_mismatch", null, last, null, "the stored catalog chain ends early");
        return;
      }
      if (
        want === undefined ||
        s.itemCount !== want.itemCount ||
        s.continued !== want.continued ||
        !paddedEqual(s.content, want.payload)
      ) {
        this.add(
          "catalog_mismatch",
          null,
          s.page,
          null,
          "the stored catalog page is not the encoding of the snapshot's catalog",
        );
        return;
      }
    }
  }

  private sequences(): void {
    for (const s of this.target.snap.sequencesSorted()) {
      if (s.lastValue >= s.minValue && s.lastValue <= s.maxValue) continue;
      this.add(
        "sequence_out_of_range",
        s.name,
        null,
        null,
        `value ${s.lastValue} outside [${s.minValue}, ${s.maxValue}]`,
      );
    }
  }

  // --- trees and pages (§3.2) ---

  private *table(name: string): Steps<void> {
    const snap = this.target.snap;
    const table = snap.tables.get(name)!;
    const store = snap.stores.get(name)!;
    const colTypes = store.columnTypes();
    const colls: (Collation | null)[] = table.columns.map((c) =>
      c.collation !== null ? (snap.resolveCollation(c.collation) ?? null) : null,
    );
    let walked = 0;
    let rowFailed = false;
    const readable = yield* this.btree(
      name,
      treeRef(store.treeRoot()),
      colTypes,
      function* (this: Checker, page: number, leaf: LeafView, i: number): Steps<void> {
        walked++;
        if (!(yield* this.row(name, table.pk, colTypes, colls, page, leaf, i))) rowFailed = true;
      }.bind(this),
    );
    const tableReadable = readable && !rowFailed;
    this.tableReadable.set(name, tableReadable);
    const count = store.count();
    if (tableReadable && count !== null && count !== BigInt(walked)) {
      this.add(
        "row_count_mismatch",
        name,
        null,
        null,
        `catalog records ${count} rows, the tree holds ${walked}`,
      );
    }
    if (!tableReadable) this.damaged = true;
    const indexes = [...table.indexes].sort((a, b) => compareTextC(a.name, b.name));
    for (const def of indexes) {
      const before = this.findings.length;
      let entries = 0;
      let indexReadable: boolean;
      const ops = def.kind === "gist" ? gistOpclasses(table, def) : [];
      if (def.kind === "gist") {
        const walk = yield* this.gist(def.name, snap.gistRoots.get(def.name) ?? NO_TREE_REF, ops);
        indexReadable = walk.readable;
        entries = walk.entries;
      } else {
        this.ginPostings = 0;
        indexReadable = yield* this.btree(
          def.name,
          treeRef(snap.indexStore(def.name).treeRoot()),
          [],
          (): undefined => {
            entries++;
          },
          def.kind === "gin",
        );
        if (def.kind === "gin") entries = this.ginPostings;
      }
      if (!indexReadable) this.damaged = true;
      this.indexClean.set(def.name, indexReadable && this.findings.length === before);
      if (this.level !== "full") continue;
      // §3.5: the definition's own skips first, decided before any row is read.
      let reason = this.definitionSkip(def);
      let rindex: ResolvedIndex | null = null;
      if (reason === null) {
        try {
          rindex = this.target.storage.resolveIndex(table, def);
        } catch (e) {
          reason = resolveSkip(e);
        }
        if (rindex !== null && this.collationSkip(table, rindex)) reason = "missing_loaded_data";
      }
      if (reason !== null) {
        this.skipped.push({ object: def.name, reason });
        continue;
      }
      if (!tableReadable) {
        this.skipped.push({ object: def.name, reason: "table_unreadable" });
        continue;
      }
      // An index whose walk reported anything, even an order or bounds finding, is not checked for
      // agreement: its own lookups cannot be trusted to find what it holds (§5.2).
      if (!indexReadable || this.findings.length !== before) {
        this.skipped.push({ object: def.name, reason: "index_unreadable" });
        continue;
      }
      yield* this.agreement(name, def, rindex!, ops, colTypes, colls, entries);
    }
  }

  // definitionSkip is the first of the definition-level skips (§3.5) that applies to an index, decided
  // from its definition and the handle's environment alone: a stored expression this binary cannot
  // parse; a host function the environment does not register at the component and version the index
  // pinned; a time zone the index pinned that is not loaded or is at another version. Collations are
  // collationSkip's, after the index resolves.
  private definitionSkip(def: IndexDef): IntegritySkipReasonName | null {
    if (indexUnparsed(def)) return "unparsable_definition";
    const env = this.target.storage.environment;
    if (this.hostDepsMissing(def.hostDeps)) return "missing_host_function";
    try {
      verifyIndexTimezoneDeps(def.timezoneDeps, env);
    } catch (e) {
      if (e instanceof EngineError && e.code() === "XX002") return "missing_loaded_data";
      throw e;
    }
    return null;
  }

  // hostDepsMissing reports whether a pinned host-function dependency is not registered by the
  // environment at its signature, component, and version (§3.5).
  private hostDepsMissing(deps: HostFuncDep[] | undefined): boolean {
    const registered = this.target.storage.environment.hostFunctions()?.entries() ?? [];
    return (deps ?? []).some(
      (dep) =>
        !registered.some(
          (hf) =>
            hf.name === dep.name &&
            hf.argTypes.length === dep.argTypes.length &&
            hf.argTypes.every((t, i) => t === dep.argTypes[i]) &&
            hf.componentId === dep.componentId &&
            hf.semanticVersion === dep.semanticVersion,
        ),
    );
  }

  // collationUnavailable reports whether a column collation is not loaded, or is loaded at a version
  // other than the catalog's pin (§3.5). C (no collation) is always available.
  private collationUnavailable(name: string | null | undefined): boolean {
    return (
      name !== null &&
      name !== undefined &&
      name !== "" &&
      (this.target.snap.collationSkew(name) !== undefined ||
        this.target.storage.environment.collation(name) === undefined)
    );
  }

  // collationSkip reports whether a resolved index depends on a collation the environment does not
  // load, or loads at a version other than the catalog's pin (§3.5): a bare column key's column
  // collation (any index kind), an expression key's derived collation, and the collation of every
  // column an expression key or the partial predicate reads. C (no collation) never counts.
  private collationSkip(table: Table, rindex: ResolvedIndex): boolean {
    const unavailable = (name: string | null | undefined): boolean =>
      this.collationUnavailable(name);
    const refs = (node: unknown): boolean =>
      rexprColumns(node).some((i) => unavailable(table.columns[i]?.collation));
    for (const key of rindex.keys) {
      if (key.kind === "column") {
        if (unavailable(table.columns[key.column]!.collation)) return true;
      } else if (unavailable(key.coll?.name) || refs(key.expr)) return true;
    }
    return rindex.predicate !== undefined && refs(rindex.predicate);
  }

  // agreement checks that an index holds exactly the entries its table derives (§3.4): each row's
  // entries, derived through the write path's own derivation (index-entry keys, the partial predicate
  // as a filter, GIN terms, the GiST leaf bound), are looked up through the index's own lookup, and
  // the entries found must number the index's entry count.
  private *agreement(
    table: string,
    def: IndexDef,
    rindex: ResolvedIndex,
    ops: GistOpclass[],
    colTypes: ColType[],
    colls: (Collation | null)[],
    entries: number,
  ): Steps<void> {
    const snap = this.target.snap;
    const columns = snap.tables.get(table)!.columns;
    const store = snap.stores.get(table)!;
    const ist = def.kind === "gist" ? null : snap.indexStore(def.name);
    const gistRoot = snap.gistRoots.get(def.name) ?? NO_TREE_REF;
    const env = this.target.storage.indexMaintenanceEnv();
    const tagLen = this.tagLen;
    const paging = this.paging;
    const fetch: PageFetch = (p, t) => paging.readBodySteps(p, t);
    let found = 0;
    yield* this.rewalk(
      treeRef(store.treeRoot()),
      colTypes,
      function* (this: Checker, _page: number, leaf: LeafView, i: number): Steps<void> {
        yield* this.poll();
        const key = leaf.keys[i]!;
        const row: Row = new Array(colTypes.length);
        for (let c = 0; c < colTypes.length; c++)
          row[c] = yield* decodeLeafValueSteps(leaf, colTypes, i, c, fetch, tagLen);
        let want: Uint8Array[];
        try {
          want = yield* indexEntryKeysSteps(columns, colTypes, colls, rindex, key, row, env);
        } catch (e) {
          if (evalFatal(e)) throw e;
          this.add("index_entry_error", def.name, null, key, message(e));
          return;
        }
        for (const entry of want) {
          let present: boolean;
          if (def.kind === "gin") present = yield* this.ginHolds(ist!, entry, key);
          else if (def.kind === "gist") present = yield* this.gistHolds(gistRoot, ops, entry);
          else {
            try {
              present = (yield* ist!.getSteps(entry)) !== undefined;
            } catch (e) {
              if (fatal(e)) throw e;
              present = false;
            }
          }
          if (present) found++;
          else this.add("index_entry_missing", def.name, null, key, "no entry for the row");
        }
      }.bind(this),
    );
    if (entries !== found) {
      this.add(
        "index_entry_count_mismatch",
        def.name,
        null,
        null,
        `index holds ${entries} entries, ${found} derive from rows`,
      );
    }
  }

  // ginHolds looks up one derived GIN posting (term ‖ storage key) as the index's own lookup does: the
  // first run at or after term ‖ key within the term's range, decoded with the term's length, holds
  // key. A run that does not decode holds nothing (§3.4).
  private *ginHolds(ist: TableStore, entry: Uint8Array, key: Uint8Array): Steps<boolean> {
    const term = entry.subarray(0, entry.length - key.length);
    try {
      const run = yield* ginRunAtOrAfterSteps(ist, term, key);
      if (run === null) return false;
      return ginDecodeRun(term.length, run).some((k) => compareBytes(k, key) === 0);
    } catch (e) {
      if (fatal(e)) throw e;
      return false;
    }
  }

  // gistHolds looks up one derived GiST leaf entry (bound ‖ storage key): it descends into every child
  // whose bound contains the derived bound, and the entry is present when a leaf holds the identical
  // bound bytes with that storage key (§3.4).
  private *gistHolds(root: TreeRef, ops: GistOpclass[], entry: Uint8Array): Steps<boolean> {
    if (root.page === 0) return false;
    try {
      const want = gistSplitLeafKey(ops, entry);
      const stack: TreeRef[] = [root];
      while (stack.length > 0) {
        const ref = stack.pop()!;
        const node = parseGistNode(yield* this.readPage(ref.page, ref.tag), this.tagLen, ops);
        if (node === null) continue;
        for (const it of node.items) {
          if (node.leaf) {
            if (
              compareBytes(it.raw, want.boundBytes) === 0 &&
              compareBytes(it.skey!, want.skey) === 0
            )
              return true;
          } else if (gistBoundContains(it.bound, want.bound)) {
            stack.push(it.child!);
          }
        }
      }
      return false;
    } catch (e) {
      if (fatal(e)) throw e;
      return false;
    }
  }

  // row decodes one table record (§3.2): each value as its column's type, an out-of-line value's
  // chain reached and verified, then the storage key against the primary-key encoding. It returns
  // false when a value did not decode (the table is then unreadable).
  private *row(
    table: string,
    pk: number[],
    colTypes: ColType[],
    colls: (Collation | null)[],
    page: number,
    leaf: LeafView,
    i: number,
  ): Steps<boolean> {
    this.rows++;
    yield* this.poll();
    const key = leaf.keys[i]!;
    const row: Row = new Array(colTypes.length);
    const paging = this.paging;
    const self = this;
    const fetch: PageFetch = function* (p, t) {
      if (!self.inRange(p)) throw new Reach("page_out_of_range", p);
      if (self.isReached(p)) throw new Reach("page_reached_twice", p);
      self.mark(p);
      yield* self.poll();
      return yield* paging.readBodySteps(p, t);
    };
    const columns = this.target.snap.tables.get(table)!.columns;
    for (let c = 0; c < colTypes.length; c++) {
      const tag = leafValueTag(leaf, colTypes, i, c);
      try {
        row[c] = yield* decodeLeafValueSteps(leaf, colTypes, i, c, fetch, this.tagLen);
      } catch (e) {
        const column = `column ${columns[c]?.name ?? c}`;
        if (e instanceof Reach) {
          this.add(e.kind, table, e.page, key, `${column}: page ${e.page}`);
          return false;
        }
        if (fatal(e)) throw e;
        this.add(
          isExternalTag(tag) ? "overflow_chain_corrupt" : "value_undecodable",
          table,
          page,
          key,
          `${column}: ${message(e)}`,
        );
        return false;
      }
    }
    if (pk.length > 0) {
      let want: Uint8Array | null;
      try {
        want = encodePkKey(colTypes, pk, colls, row);
      } catch (e) {
        if (fatal(e)) throw e;
        want = null;
      }
      if (want === null || compareBytes(want, key) !== 0) {
        this.add(
          "row_key_not_primary_key",
          table,
          page,
          key,
          "stored key is not the row's primary-key encoding",
        );
      }
    }
    return true;
  }

  // btree walks one B+tree depth-first in child order (format.md *Interior node*): page reach and
  // verification, node types, uniform leaf depth, strict key order within a node, and every key within
  // the bounds its parent's separators give it. Each leaf record goes to visit in key order. The walk
  // keeps one root-to-leaf path. It returns false when part of the tree could not be read (§5.2).
  private *btree(
    object: string,
    root: TreeRef,
    colTypes: ColType[],
    visit: Visit,
    gin = false,
  ): Steps<boolean> {
    if (root.page === 0) return true;
    let readable = true;
    let leafDepth: number | null = null;
    const stack: Frame[] = [];
    let next: Pending | null = { page: root.page, tag: root.tag, depth: 0, lo: null, hi: null };
    while (next !== null) {
      const node = next;
      next = null;
      if (!this.reach(object, node.page)) readable = false;
      else if (node.depth >= INTEGRITY_MAX_TREE_DEPTH) {
        // Deeper than any tree the engine writes: misdirected pointers. Reached, not read (§5.2).
        readable = false;
        this.add(
          "page_corrupt",
          object,
          node.page,
          null,
          `tree deeper than ${INTEGRITY_MAX_TREE_DEPTH}`,
        );
      } else {
        let pg: ReturnType<typeof parsePage> | null = null;
        try {
          pg = yield* this.readPage(node.page, node.tag);
        } catch (e) {
          if (fatal(e)) throw e;
          readable = false;
          this.add("page_corrupt", object, node.page, null, message(e));
        }
        if (pg !== null && pg.pageType === PAGE_TYPE_INTERIOR) {
          let parsed: { children: TreeRef[]; seps: Uint8Array[] } | null = null;
          try {
            parsed = parseInterior(pg.payload, pg.itemCount, this.tagLen);
          } catch (e) {
            if (fatal(e)) throw e;
            readable = false;
            this.add("page_corrupt", object, node.page, null, message(e));
          }
          if (parsed !== null) {
            this.checkKeys(object, node.page, parsed.seps, node.lo, node.hi);
            stack.push({ ...parsed, i: 0, depth: node.depth, lo: node.lo, hi: node.hi });
          }
        } else if (pg !== null && pg.pageType === PAGE_TYPE_LEAF) {
          leafDepth ??= node.depth;
          if (node.depth !== leafDepth) {
            this.add(
              "tree_depth_uneven",
              object,
              node.page,
              null,
              `leaf at depth ${node.depth}, others at ${leafDepth}`,
            );
          }
          let leaf: LeafView | null = null;
          try {
            leaf = parseLeafView(pg.payload, pg.itemCount, colTypes);
          } catch (e) {
            if (fatal(e)) throw e;
            readable = false;
            this.add("page_corrupt", object, node.page, null, message(e));
          }
          if (leaf !== null) {
            this.checkKeys(object, node.page, leaf.keys, node.lo, node.hi);
            if (gin) {
              // A GIN tree's records are posting runs, counted from their tails (§3.2); the first run
              // that does not parse makes the leaf unreadable.
              let postings = 0;
              try {
                for (const k of leaf.keys) postings += ginRunPostings(k);
                this.ginPostings += postings;
              } catch (e) {
                if (fatal(e)) throw e;
                readable = false;
                this.add("page_corrupt", object, node.page, null, message(e));
                leaf = null;
              }
            }
          }
          if (leaf !== null) {
            for (let i = 0; i < leaf.n; i++) {
              const steps = visit(node.page, leaf, i);
              if (steps !== undefined) yield* steps;
            }
          }
        } else if (pg !== null) {
          readable = false;
          this.add(
            "page_type_wrong",
            object,
            node.page,
            null,
            `page type ${pg.pageType} in a B-tree`,
          );
        }
      }
      next = nextChild(stack);
    }
    return readable;
  }

  // rewalk visits a readable table's records again, in key order, for index agreement: no reach,
  // no findings (the structural walk already read every page of it).
  private *rewalk(root: TreeRef, colTypes: ColType[], visit: Visit): Steps<void> {
    if (root.page === 0) return;
    const stack: Frame[] = [];
    let next: Pending | null = { page: root.page, tag: root.tag, depth: 0, lo: null, hi: null };
    while (next !== null) {
      const pg = yield* this.readPage(next.page, next.tag);
      if (pg.pageType === PAGE_TYPE_INTERIOR) {
        const parsed = parseInterior(pg.payload, pg.itemCount, this.tagLen);
        stack.push({ ...parsed, i: 0, depth: next.depth, lo: null, hi: null });
      } else {
        const leaf = parseLeafView(pg.payload, pg.itemCount, colTypes);
        for (let i = 0; i < leaf.n; i++) {
          const steps = visit(next.page, leaf, i);
          if (steps !== undefined) yield* steps;
        }
      }
      next = nextChild(stack);
    }
  }

  // checkKeys reports at most one keys_unordered and one separator_violation per page.
  private checkKeys(
    object: string,
    page: number,
    keys: Uint8Array[],
    lo: Uint8Array | null,
    hi: Uint8Array | null,
  ): void {
    for (let i = 1; i < keys.length; i++) {
      if (compareBytes(keys[i - 1]!, keys[i]!) >= 0) {
        this.add("keys_unordered", object, page, null, "keys do not ascend strictly");
        break;
      }
    }
    for (const k of keys) {
      if ((lo !== null && compareBytes(k, lo) < 0) || (hi !== null && compareBytes(k, hi) >= 0)) {
        this.add(
          "separator_violation",
          object,
          page,
          null,
          "a key lies outside the bounds its parent gives it",
        );
        return;
      }
    }
  }

  // gist walks a GiST R-tree (gist.md §4.1) for page reach and verification, node types, uniform leaf
  // depth, every bound decoding under the index's opclasses, and each interior entry's bound containing
  // every bound in the child it names (§3.2). It returns whether the tree was readable and its leaf
  // entry count.
  private *gist(
    object: string,
    root: TreeRef,
    ops: GistOpclass[],
  ): Steps<{ readable: boolean; entries: number }> {
    if (root.page === 0) return { readable: true, entries: 0 };
    let readable = true;
    let entries = 0;
    let leafDepth: number | null = null;
    const stack: { items: GistItem[]; i: number; depth: number }[] = [];
    let next: { page: number; tag: Uint8Array; depth: number; parent: GistBound[] | null } | null =
      { page: root.page, tag: root.tag, depth: 0, parent: null };
    while (next !== null) {
      const node = next;
      next = null;
      if (!this.reach(object, node.page)) readable = false;
      else if (node.depth >= INTEGRITY_MAX_TREE_DEPTH) {
        // Deeper than any tree the engine writes: misdirected pointers. Reached, not read (§5.2).
        readable = false;
        this.add(
          "page_corrupt",
          object,
          node.page,
          null,
          `tree deeper than ${INTEGRITY_MAX_TREE_DEPTH}`,
        );
      } else {
        let pg: ReturnType<typeof parsePage> | null = null;
        let parsed: GistNode | null = null;
        try {
          pg = yield* this.readPage(node.page, node.tag);
          parsed = parseGistNode(pg, this.tagLen, ops);
        } catch (e) {
          if (fatal(e)) throw e;
          readable = false;
          pg = null;
          this.add("page_corrupt", object, node.page, null, message(e));
        }
        if (pg !== null && parsed === null) {
          readable = false;
          this.add(
            "page_type_wrong",
            object,
            node.page,
            null,
            `page type ${pg.pageType} in a GiST tree`,
          );
        } else if (parsed !== null) {
          if (parsed.leaf) {
            leafDepth ??= node.depth;
            if (node.depth !== leafDepth) {
              this.add(
                "tree_depth_uneven",
                object,
                node.page,
                null,
                `leaf at depth ${node.depth}, others at ${leafDepth}`,
              );
            }
          }
          const parent = node.parent;
          if (parent !== null && !parsed.items.every((it) => gistBoundContains(parent, it.bound))) {
            this.add(
              "gist_bounds_violation",
              object,
              node.page,
              null,
              "a bound lies outside its parent's bound for this page",
            );
          }
          if (parsed.leaf) entries += parsed.items.length;
          else stack.push({ items: parsed.items, i: 0, depth: node.depth });
        }
      }
      while (stack.length > 0) {
        const top = stack[stack.length - 1]!;
        if (top.i < top.items.length) {
          const it = top.items[top.i++]!;
          next = {
            page: it.child!.page,
            tag: it.child!.tag,
            depth: top.depth + 1,
            parent: it.bound,
          };
          break;
        }
        stack.pop();
      }
    }
    return { readable, entries };
  }

  // --- constraints (§3.4) ---

  // tableConstraints re-checks one table's declared constraints against its stored rows (§3.4, stage
  // 3 of §4): the column pass, then every named constraint in name byte order, ties by kind (CHECK,
  // then unique index, then foreign key). A table whose tree was unreadable skips every one of them.
  private *tableConstraints(name: string): Steps<void> {
    const snap = this.target.snap;
    const table = snap.tables.get(name)!;
    const named: Named[] = [
      ...table.checks.map((c): Named => ({ name: c.name, kind: 0, check: c })),
      ...table.indexes
        .filter((d) => d.unique && d.kind === "btree")
        .map((d): Named => ({ name: d.name, kind: 1, index: d })),
      ...table.fks.map((fk): Named => ({ name: fk.name, kind: 2, fk })),
    ].sort((a, b) => compareTextC(a.name, b.name) || a.kind - b.kind);
    if (!this.tableReadable.get(name)) {
      this.skipped.push({ object: name, reason: "table_unreadable" });
      for (const n of named) this.skipped.push({ object: n.name, reason: "table_unreadable" });
      return;
    }
    yield* this.columnPass(table);
    for (const n of named) {
      if (n.kind === 0) yield* this.checkPass(table, n.check);
      else if (n.kind === 1) yield* this.uniquePass(table, n.index);
      else yield* this.foreignKeyPass(table, n.fk);
    }
  }

  // tableRows re-walks a readable table's records in storage-key order, decoding the columns `want`
  // names (every column when null) for visit.
  private *tableRows(
    table: Table,
    want: number[] | null,
    visit: (key: Uint8Array, row: Row) => Steps<void> | undefined,
  ): Steps<void> {
    const store = this.target.snap.stores.get(table.name)!;
    const colTypes = store.columnTypes();
    const tagLen = this.tagLen;
    const paging = this.paging;
    const fetch: PageFetch = (p, t) => paging.readBodySteps(p, t);
    const cols = want ?? colTypes.map((_, c) => c);
    yield* this.rewalk(
      treeRef(store.treeRoot()),
      colTypes,
      function* (this: Checker, _page: number, leaf: LeafView, i: number): Steps<void> {
        yield* this.poll();
        const row: Row = new Array(colTypes.length);
        for (const c of cols)
          row[c] = yield* decodeLeafValueSteps(leaf, colTypes, i, c, fetch, tagLen);
        const steps = visit(leaf.keys[i]!, row);
        if (steps !== undefined) yield* steps;
      }.bind(this),
    );
  }

  // columnPass reports, per row and column in ordinal order, a NULL in a NOT NULL or primary-key
  // column (not_null_violation) and a value its column's write-time typmod coercion would raise on or
  // change (typmod_violation). Top-level scalar columns only (§8).
  private *columnPass(table: Table): Steps<void> {
    const colTypes = this.target.snap.stores.get(table.name)!.columnTypes();
    const pk = new Set(table.pk);
    yield* this.tableRows(table, null, (key, row): undefined => {
      for (let c = 0; c < table.columns.length; c++) {
        const col = table.columns[c]!;
        const v = row[c]!;
        if (v.kind === "null") {
          if (col.notNull || col.primaryKey || pk.has(c))
            this.add("not_null_violation", table.name, null, key, `column ${col.name} is NULL`);
        } else if (typmodViolated(col, colTypes[c]!, v)) {
          this.add(
            "typmod_violation",
            table.name,
            null,
            key,
            `column ${col.name} exceeds its typmod`,
          );
        }
      }
    });
  }

  // checkPass evaluates a CHECK constraint against every row as a write does (constraints.md §4):
  // FALSE is check_violation, an error check_error. The definition-level skips come first (§3.5).
  private *checkPass(table: Table, c: CheckConstraint): Steps<void> {
    let reason: IntegritySkipReasonName | null = null;
    let node: RExpr | null = null;
    if (c.unparsed) reason = "unparsable_definition";
    else if (this.hostDepsMissing(c.hostDeps)) reason = "missing_host_function";
    else {
      try {
        node = this.target.storage.resolveCheck(table, c);
      } catch (e) {
        reason = resolveSkip(e);
      }
      if (
        node !== null &&
        rexprColumns(node).some((i) => this.collationUnavailable(table.columns[i]?.collation))
      )
        reason = "missing_loaded_data";
    }
    if (reason !== null) {
      this.skipped.push({ object: c.name, reason });
      return;
    }
    const env = this.target.storage.indexMaintenanceEnv();
    const self = this;
    yield* this.tableRows(table, null, function* (key, row) {
      let v: Value;
      try {
        // Unmetered, with a fresh scalar allowance per row.
        v = yield* evalExprSteps(node!, row, env, new Meter());
      } catch (e) {
        if (evalFatal(e)) throw e;
        self.add("check_error", c.name, null, key, message(e));
        return;
      }
      if (v.kind === "bool" && !v.value)
        self.add("check_violation", c.name, null, key, "the row fails the check");
    });
  }

  // uniquePass reads a unique B-tree index's entries in order and reports each whose key prefix (the
  // entry before its storage key) equals the previous entry's with no NULL element: jed's NULLs are
  // always distinct (§3.4). It keeps one prefix, never the index.
  private *uniquePass(table: Table, def: IndexDef): Steps<void> {
    const elems = this.uniqueElements(table, def);
    if (typeof elems === "string") {
      this.skipped.push({ object: def.name, reason: elems });
      return;
    }
    if (!this.indexClean.get(def.name)) {
      this.skipped.push({ object: def.name, reason: "index_unreadable" });
      return;
    }
    let prev: Uint8Array | null = null;
    yield* this.rewalk(
      treeRef(this.target.snap.indexStore(def.name).treeRoot()),
      [],
      function* (this: Checker, _page: number, leaf: LeafView, i: number): Steps<void> {
        yield* this.poll();
        const entry = leaf.keys[i]!;
        const split = splitIndexEntry(entry, elems);
        if (split === null) {
          // Unreachable for an index whose walk was clean and whose entries derive from rows; an
          // entry that does not split starts a new run.
          prev = null;
          return;
        }
        const prefix = entry.subarray(0, split.prefixLen);
        if (!split.hasNull && prev !== null && compareBytes(prefix, prev) === 0) {
          this.add(
            "unique_violation",
            def.name,
            null,
            entry.subarray(split.prefixLen),
            "the key repeats the previous entry's",
          );
        }
        prev = split.hasNull ? null : prefix.slice();
      }.bind(this),
    );
  }

  // uniqueElements is each key element's type, key form, and order, which split an entry into its key
  // prefix and storage key. An expression key's type needs the index resolved; a failure to resolve is
  // its skip reason (§3.5). A column key needs nothing evaluated, so it never skips here.
  private uniqueElements(table: Table, def: IndexDef): KeyElem[] | IntegritySkipReasonName {
    const snap = this.target.snap;
    const colTypes = snap.stores.get(table.name)!.columnTypes();
    let rindex: ResolvedIndex | null = null;
    if (def.keys.some((k) => k.kind !== "column")) {
      try {
        rindex = this.target.storage.resolveIndex(table, def);
      } catch (e) {
        return resolveSkip(e);
      }
    }
    return def.keys.map((k, i): KeyElem => {
      const order = indexKeyOrder(def, i);
      if (k.kind === "column")
        return {
          ct: colTypes[k.column]!,
          collated: table.columns[k.column]!.collation !== null,
          order,
        };
      const rk = rindex!.keys[i]!;
      if (rk.kind === "column")
        return {
          ct: colTypes[rk.column]!,
          collated: table.columns[rk.column]!.collation !== null,
          order,
        };
      return { ct: resolveColType(rk.type, snap.types), collated: rk.coll !== null, order };
    });
  }

  // foreignKeyPass looks up each referencing row's key in the referenced table as the write path's
  // enforcement does (constraints.md §6.3–§6.4): a point lookup by primary key, or a prefix lookup in
  // the unique index over the referenced columns. MATCH SIMPLE exempts a row with any NULL key column,
  // MATCH FULL one with all NULL, and a MATCH FULL row with some but not all NULL is a violation.
  private *foreignKeyPass(table: Table, fk: ForeignKey): Steps<void> {
    const snap = this.target.snap;
    const parent = snap.tables.get(fk.refTable);
    // The probe encodes under the referenced columns' collations; one this handle cannot use would make
    // every probe miss, so it skips like an index whose collation is not loaded (§3.5).
    if (
      parent !== undefined &&
      fk.refColumns.some((c) => this.collationUnavailable(parent.columns[c]?.collation))
    ) {
      this.skipped.push({ object: fk.name, reason: "missing_loaded_data" });
      return;
    }
    if (parent === undefined || !this.tableReadable.get(fk.refTable)) {
      this.skipped.push({ object: fk.name, reason: "table_unreadable" });
      return;
    }
    // The referenced key, as fkProbe chooses it: the primary key when it is exactly the referenced
    // set, else a unique index over exactly that set.
    const refSet = sortedUnique(fk.refColumns);
    const viaPk = parent.pk.length > 0 && sameSet(sortedUnique(parent.pk), refSet);
    if (!viaPk) {
      const idx = parent.indexes.find((d) => {
        const cols = indexColumnOrdinals(d);
        return d.unique && cols !== null && sameSet(sortedUnique(cols), refSet);
      });
      // A catalog the engine writes always has the index (it cannot be dropped under the key); without
      // one the key cannot be looked up, which is reported as that index being unreadable.
      if (idx === undefined || !this.indexClean.get(idx.name)) {
        this.skipped.push({ object: fk.name, reason: "index_unreadable" });
        return;
      }
    }
    const parentColls: (Collation | null)[] = parent.columns.map((c) =>
      c.collation !== null ? (snap.resolveCollation(c.collation) ?? null) : null,
    );
    const parentStore = snap.stores.get(parent.name)!;
    const self = this;
    yield* this.tableRows(table, fk.columns, function* (key, row) {
      let found: boolean;
      if (fkMatchFullViolated(fk, row)) found = false;
      else {
        const probe = yield* fkProbeSteps(fk, parent, parentColls, row, fk.columns);
        if (probe === null) return; // exempt under its MATCH rule
        if (probe.kind === "absent") found = false;
        else if (probe.kind === "pk")
          found = (yield* parentStore.getSteps(probe.bytes)) !== undefined;
        else
          found =
            (yield* snap.indexStore(probe.index).rangeEntriesSteps(uniqueProbeBound(probe.prefix)))
              .length > 0;
      }
      if (!found) self.add("foreign_key_violation", fk.name, null, key, "no referenced row");
    });
  }

  // --- space accounting (§3.3) ---

  // freeList checks the generation's free list and returns how many chain pages it read: first the
  // chain (each page in range, not revisited, not reachable, verifying as a free-list page), then each
  // entry in list order. The entries are read in a second pass over the verified chain, so memory is a
  // bitmap of pages, never the list.
  private *freeList(gen: Generation): Steps<number> {
    const chainSeen = new Uint8Array(Math.ceil(this.pageCount / 8));
    let chain = 0;
    let page = gen.freeHead;
    let tag = gen.freeTag;
    while (page !== 0) {
      if (!this.inRange(page) || (chainSeen[page >> 3]! & (1 << (page & 7))) !== 0) {
        this.add("free_list_corrupt", null, page, null, `free-list chain names page ${page}`);
        break;
      }
      if (this.isReached(page))
        this.add("free_page_live", null, page, null, `free-list page ${page} is reachable`);
      let link: { next: number; nextTag: Uint8Array; content: Uint8Array; count: number };
      try {
        link = yield* this.freeListPage(page, tag);
      } catch (e) {
        if (fatal(e)) throw e;
        this.add("free_list_corrupt", null, page, null, message(e));
        break;
      }
      chainSeen[page >> 3]! |= 1 << (page & 7);
      chain++;
      this.freeChainPages++;
      page = link.next;
      tag = link.nextTag;
    }
    const seen = new Uint8Array(Math.ceil(this.pageCount / 8));
    const entry = (p: number): void => {
      if (!this.inRange(p)) {
        this.add(
          "free_page_out_of_range",
          null,
          p,
          null,
          `free page ${p} outside [${this.first}, ${this.pageCount})`,
        );
      } else if ((seen[p >> 3]! & (1 << (p & 7))) !== 0) {
        this.add("free_page_duplicate", null, p, null, `free page ${p} listed twice`);
      } else {
        seen[p >> 3]! |= 1 << (p & 7);
        if (this.isReached(p))
          this.add("free_page_live", null, p, null, `free page ${p} is reachable`);
        else if (gen.protectedPages.has(p))
          this.add(
            "free_page_manifest_dependency",
            null,
            p,
            null,
            `free page ${p} is a dependency of the generation`,
          );
      }
    };
    if (gen.memoryFree !== null) for (const p of gen.memoryFree) entry(p);
    page = gen.freeHead;
    tag = gen.freeTag;
    for (let n = 0; n < chain; n++) {
      const link = yield* this.freeListPage(page, tag);
      const dv = new DataView(
        link.content.buffer,
        link.content.byteOffset,
        link.content.byteLength,
      );
      for (let i = 0; i < link.count; i++) entry(dv.getUint32(i * 4, false));
      page = link.next;
      tag = link.nextTag;
    }
    return chain;
  }

  // freeListPage reads and verifies one free-list chain page (format.md *Free-list page*).
  private *freeListPage(
    page: number,
    tag: Uint8Array,
  ): Steps<{ next: number; nextTag: Uint8Array; content: Uint8Array; count: number }> {
    const pg = yield* this.readPage(page, tag);
    if (pg.pageType !== PAGE_FREELIST) throw engineError("data_corrupted", "not a free-list page");
    const link = chainPayload(pg, this.tagLen);
    if (pg.itemCount > Math.floor(link.content.length / 4))
      throw engineError("data_corrupted", "invalid free-list count");
    return { ...link, count: pg.itemCount };
  }
}

// INTEGRITY_MAX_TREE_DEPTH caps a walk's depth (root at 0; spec/design/integrity-check.md §5.2): a page
// at this depth or deeper is reached and reported page_corrupt without being read.
const INTEGRITY_MAX_TREE_DEPTH = 256;
const PAGE_GIST_LEAF = 5;
const PAGE_GIST_INTERIOR = 6;
const NO_TREE_REF: TreeRef = { page: 0, tag: NO_TAG };

type Pending = {
  page: number;
  tag: Uint8Array;
  depth: number;
  lo: Uint8Array | null;
  hi: Uint8Array | null;
};
type Frame = {
  children: TreeRef[];
  seps: Uint8Array[];
  i: number;
  depth: number;
  lo: Uint8Array | null;
  hi: Uint8Array | null;
};

// nextChild advances the walk's path to the next unvisited child: child i of an interior holds
// [sep[i−1], sep[i]), the first and last inheriting the interior's own bounds.
function nextChild(stack: Frame[]): Pending | null {
  while (stack.length > 0) {
    const top = stack[stack.length - 1]!;
    if (top.i < top.children.length) {
      const i = top.i++;
      const c = top.children[i]!;
      return {
        page: c.page,
        tag: c.tag,
        depth: top.depth + 1,
        lo: i === 0 ? top.lo : top.seps[i - 1]!,
        hi: i === top.seps.length ? top.hi : top.seps[i]!,
      };
    }
    stack.pop();
  }
  return null;
}

function treeRef(n: { page: number; tag?: Uint8Array } | null): TreeRef {
  return n === null || n.page === 0 ? NO_TREE_REF : { page: n.page, tag: n.tag ?? NO_TAG };
}

// One GiST node entry: its decoded bound, the bound's bytes, and the leaf's storage key or the
// interior's child pointer.
type GistItem = {
  bound: GistBound[];
  raw: Uint8Array;
  skey: Uint8Array | null;
  child: TreeRef | null;
};
type GistNode = { leaf: boolean; items: GistItem[] };

// parseGistNode parses a GiST page's entries (gist.md §4.1), decoding every bound under ops exactly
// (§3.2), or returns null for a page of another type. A malformed entry or bound is data_corrupted.
function parseGistNode(
  pg: ReturnType<typeof parsePage>,
  tagLen: number,
  ops: GistOpclass[],
): GistNode | null {
  const buf = pg.payload;
  let pos = 0;
  const take = (n: number): Uint8Array => {
    if (n > buf.length - pos) throw engineError("data_corrupted", "unexpected end of page data");
    const at = pos;
    pos += n;
    return buf.subarray(at, pos);
  };
  const u16 = (): number => {
    const b = take(2);
    return (b[0]! << 8) | b[1]!;
  };
  const items: GistItem[] = [];
  if (pg.pageType === PAGE_GIST_LEAF) {
    for (let i = 0; i < pg.itemCount; i++) {
      const raw = take(u16());
      const skey = take(u16());
      items.push({ bound: gistDecodeBound(ops, raw), raw, skey, child: null });
    }
    return { leaf: true, items };
  }
  if (pg.pageType !== PAGE_GIST_INTERIOR) return null;
  if (pg.itemCount * (6 + tagLen) > buf.length)
    throw engineError("data_corrupted", "GiST interior item count overruns page");
  for (let i = 0; i < pg.itemCount; i++) {
    const raw = take(u16());
    const p = take(4);
    const page = ((p[0]! << 24) | (p[1]! << 16) | (p[2]! << 8) | p[3]!) >>> 0;
    const tag = tagLen === 0 ? NO_TAG : take(tagLen).slice();
    if (page === 0 && tag.some((b) => b !== 0))
      throw engineError("data_corrupted", "a pointer to no page carries a tag");
    items.push({ bound: gistDecodeBound(ops, raw), raw, skey: null, child: { page, tag } });
  }
  return { leaf: false, items };
}

// gistOpclasses are a GiST index's per-column opclasses (gist.md §5, §6): its key columns in order.
function gistOpclasses(table: Table, def: IndexDef): GistOpclass[] {
  return (indexColumnOrdinals(def) ?? []).map((ci) => gistOpclassFor(table.columns[ci]!.type));
}

// resolveSkip maps a failure to resolve an index for maintenance onto its skip reason (§3.5): XX003 is
// an unparsed definition, 42883 a host function the environment no longer registers, and XX002 data
// (a collation or time zone) the environment does not load at the pinned version. Anything else ends
// the check.
function resolveSkip(e: unknown): IntegritySkipReasonName {
  if (e instanceof EngineError) {
    switch (e.code()) {
      case "XX003":
        return "unparsable_definition";
      case "42883":
        return "missing_host_function";
      case "XX002":
        return "missing_loaded_data";
    }
  }
  throw e;
}

// rexprColumns is the column ordinals a resolved index expression reads: every `column` node anywhere
// in it. An index expression holds no subquery or outer reference, so every column node is the table's.
function rexprColumns(root: unknown): number[] {
  const out: number[] = [];
  const seen = new Set<object>();
  const walk = (v: unknown): void => {
    if (v === null || typeof v !== "object" || ArrayBuffer.isView(v) || seen.has(v)) return;
    seen.add(v);
    if (Array.isArray(v)) {
      for (const x of v) walk(x);
      return;
    }
    const o = v as { kind?: unknown; index?: unknown };
    if (o.kind === "column" && typeof o.index === "number") out.push(o.index);
    for (const x of Object.values(v)) walk(x);
  };
  walk(root);
  return out;
}

// A named constraint object of the constraint pass (§3.4), with its kind's tie-break rank.
type Named =
  | { name: string; kind: 0; check: CheckConstraint }
  | { name: string; kind: 1; index: IndexDef }
  | { name: string; kind: 2; fk: ForeignKey };

// One B-tree key element as an entry splitter reads it: its resolved type, whether a text body is a
// collation sort key, and its sort order (encoding.md §2.16).
type KeyElem = { ct: ColType; collated: boolean; order: KeyOrder };

// splitIndexEntry finds where an ordered index entry's key prefix ends and its storage key begins,
// reading each element's ordered slot (encoding.md §2.16), and whether an element is NULL. null for an
// entry whose slots do not read.
function splitIndexEntry(
  entry: Uint8Array,
  elems: KeyElem[],
): { prefixLen: number; hasNull: boolean } | null {
  let at = 0;
  let hasNull = false;
  for (const e of elems) {
    if (at >= entry.length) return null;
    const tag = entry[at]!;
    if (tag === keyOrderNullTag(e.order)) {
      hasNull = true;
      at++;
      continue;
    }
    if (tag !== keyOrderPresentTag(e.order)) return null;
    const n = keyTypeBodyLen(e.ct, e.collated, entry, at + 1, e.order.desc ? 0xff : 0);
    if (n === null) return null;
    at += 1 + n;
  }
  return at <= entry.length ? { prefixLen: at, hasNull } : null;
}

// keyTypeBodyLen is the length of a key body of type ct at key[at], each byte read as b ^ m (m = 0xFF
// under DESC): a scalar's self-delimited body, or a container's framing around its elements' bodies
// (encoding.md §2.11 range, §2.14 array, §2.15 composite). null when it does not read.
function keyTypeBodyLen(
  ct: ColType,
  collated: boolean,
  key: Uint8Array,
  at: number,
  m: number,
): number | null {
  const scalarLen = (s: ColType, coll: boolean, pos: number): number | null => {
    if (s.kind !== "scalar") return null;
    const slot = keySlotForScalar(s.scalar, coll);
    return slot === null ? null : keyBodyLen(slot, key, pos, m);
  };
  let pos = at;
  const byte = (): number | null => (pos < key.length ? key[pos++]! ^ m : null);
  switch (ct.kind) {
    case "scalar":
      return scalarLen(ct, collated, at);
    case "range": {
      const b = byte();
      if (b === 0) return 1; // empty
      if (b !== 1) return null;
      for (const unbounded of [0x00, 0x02]) {
        const marker = byte();
        if (marker === unbounded) continue;
        if (marker !== 1) return null;
        const n = scalarLen(ct.elem, false, pos);
        if (n === null) return null;
        pos += n + 1; // the element body, then its inclusivity byte
      }
      return pos <= key.length ? pos - at : null;
    }
    case "array": {
      for (;;) {
        const marker = byte();
        if (marker === 0) break;
        if (marker === 2) continue; // a NULL element
        if (marker !== 1) return null;
        const n = scalarLen(ct.elem, collated, pos);
        if (n === null) return null;
        pos += n;
      }
      const ndim = byte();
      if (ndim === null) return null;
      pos += ndim * 8; // per dimension a u32 length and an i32 lower bound
      return pos <= key.length ? pos - at : null;
    }
    case "composite": {
      for (const f of ct.fields) {
        const tag = byte();
        if (tag === 1) continue; // a NULL field
        if (tag !== 0) return null;
        const n = keyTypeBodyLen(f.type, false, key, pos, m);
        if (n === null) return null;
        pos += n;
      }
      return pos - at;
    }
  }
}

// typmodViolated reports whether a stored non-NULL value of a top-level scalar column would raise, or
// change its stored bytes, under the column's write-time typmod coercion (§3.4): varchar(n),
// decimal(p, s), timestamp(p) / timestamptz(p), and an interval qualifier or precision. Typmods inside
// containers are a follow-on (§8).
function typmodViolated(col: Column, ct: ColType, v: Value): boolean {
  if (ct.kind !== "scalar") return false;
  let out: Value;
  try {
    switch (v.kind) {
      case "text":
        if (col.varcharLen === null) return false;
        out = textValue(coerceVarcharStore(v.text, col.varcharLen, col.name));
        break;
      case "decimal":
        if (col.decimal === null) return false;
        out = decimalValue(coerceDecimal(v.dec, col.decimal));
        break;
      case "timestamp":
      case "timestamptz":
        if (col.temporal === null) return false;
        out = { ...v, micros: adjustTimestampTypmod(v.micros, col.temporal) };
        break;
      case "interval":
        if (col.temporal === null) return false;
        out = intervalValue(adjustIntervalTypmod(v.iv, col.temporal));
        break;
      default:
        return false;
    }
  } catch (e) {
    if (evalFatal(e)) throw e;
    return true;
  }
  return compareBytes(encodeValue(ct, v), encodeValue(ct, out)) !== 0;
}

// evalFatal reports whether an error deriving a row's index entries ends the check instead of being
// the row's index_entry_error (§3.4): a cancellation, a device or connection error, or anything that
// is not an engine error (a bug). An evaluation that raises is a finding.
function evalFatal(e: unknown): boolean {
  if (!(e instanceof EngineError)) return true;
  const code = e.code();
  return code === "57014" || code.startsWith("58") || code.startsWith("08");
}

// paddedEqual reports whether stored (a page's content, zero-padded to the page) holds exactly want.
function paddedEqual(stored: Uint8Array, want: Uint8Array): boolean {
  if (stored.length < want.length) return false;
  for (let i = 0; i < want.length; i++) if (stored[i] !== want[i]) return false;
  for (let i = want.length; i < stored.length; i++) if (stored[i] !== 0) return false;
  return true;
}

function u32Bytes(n: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n, false);
  return b;
}

// integrityCheckSteps runs the check over target (spec/design/integrity-check.md §3–§5). The caller
// pins target.snap for the duration (Database.integrityCheck; the simulation's image checker owns its
// copy outright).
export function* integrityCheckSteps(
  target: IntegrityTarget,
  opts: IntegrityCheckOptions = {},
): Steps<IntegrityReport> {
  const level = opts.level ?? "full";
  if (level !== "quick" && level !== "full")
    throw engineError(
      "invalid_parameter_value",
      `invalid integrity check level "${String(level)}"`,
    );
  const max = opts.maxFindings ?? 100;
  if (!Number.isSafeInteger(max) || max < 0)
    throw engineError("invalid_parameter_value", "maxFindings must be a nonnegative integer");
  throwIfAborted(opts.signal);
  return yield* new Checker(target, level, opts.constraints === true, max, opts.signal).run();
}
