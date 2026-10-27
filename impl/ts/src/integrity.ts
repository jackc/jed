// The integrity check (spec/design/integrity-check.md): a host-API walk of one database's stored
// generation that reports structured findings instead of failing on the first problem. It reads every
// page the generation reaches — catalog, table and index trees, overflow chains, the free list —
// through the handle's paging context, checks the invariants between them, and, at level `full`,
// probes each column B-tree index once per row its table derives an entry for.
//
// The report is a cross-core contract (§4): the same findings in the same order, the same skips and
// counters, as spec/fileformat/integrity.rb. Finding order is the walk order: generation, then each
// table in name byte order (its tree, then each index's structure and agreement), then space
// accounting.
//
// Memory is bounded independent of rows (§5.4, CLAUDE.md §9): one reachability bitmap of page_count
// bits, one root-to-leaf path per tree being walked, and one row at a time. Index agreement re-walks
// the table and probes the index for each derived entry; nothing materializes a table or an index.
//
// It is unmetered, like compaction: no session cost or memory account is charged.

import { NO_TAG, capFirstBody, type Cap } from "./cap.ts";
import type { ColType } from "./catalog.ts";
import { indexColumnOrdinals } from "./catalog.ts";
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
import { encodePkKey, indexEntryKeysColumnsSteps, type Engine } from "./executor.ts";
import {
  catalogImagePages,
  chainPayload,
  decodeLeafValueSteps,
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
import type { IntegrityKindName, IntegritySkipReasonName } from "./integrity_kinds.ts";
import type { SharedPaging } from "./paging.ts";
import { compareBytes } from "./pmap.ts";
import type { Snapshot } from "./snapshot.ts";
import type { Row } from "./storage.ts";
import { compareTextC } from "./value.ts";

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
  private first = 2;
  private pageCount = 0;
  private readonly tagLen: number;
  private readonly cap: Cap;
  private readonly paging: SharedPaging;
  private readonly target: IntegrityTarget;
  private readonly level: IntegrityLevel;
  private readonly max: number;
  private readonly signal: AbortSignal | undefined;

  constructor(
    target: IntegrityTarget,
    level: IntegrityLevel,
    max: number,
    signal: AbortSignal | undefined,
  ) {
    this.target = target;
    this.level = level;
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
        for (const name of [...snap.tables.keys()].sort(compareTextC)) yield* this.table(name);
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
      if (def.kind === "gist") {
        indexReadable = yield* this.gist(def.name, snap.gistRoots.get(def.name) ?? NO_TREE_REF);
      } else {
        indexReadable = yield* this.btree(
          def.name,
          treeRef(snap.indexStore(def.name).treeRoot()),
          [],
          (): undefined => {
            entries++;
          },
        );
      }
      if (!indexReadable) this.damaged = true;
      if (this.level !== "full") continue;
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
      // I2 derives agreement for column-keyed, non-partial B-tree indexes only (§9).
      if (def.kind !== "btree" || def.predicate !== undefined || indexColumnOrdinals(def) === null)
        continue;
      let found = 0;
      const ist = snap.indexStore(def.name);
      const tagLen = this.tagLen;
      const paging = this.paging;
      const fetch: PageFetch = (p, t) => paging.readBodySteps(p, t);
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
            want = yield* indexEntryKeysColumnsSteps(table.columns, colTypes, colls, def, key, row);
          } catch (e) {
            if (fatal(e)) throw e;
            this.add("index_entry_error", def.name, null, key, message(e));
            return;
          }
          for (const entry of want) {
            let present: boolean;
            try {
              present = (yield* ist.getSteps(entry)) !== undefined;
            } catch (e) {
              if (fatal(e)) throw e;
              present = false;
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
  private *btree(object: string, root: TreeRef, colTypes: ColType[], visit: Visit): Steps<boolean> {
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

  // gist walks a GiST R-tree (gist.md §4.1) for page reach and verification, node types, and uniform
  // leaf depth. Bounding-box containment is slice I3's.
  private *gist(object: string, root: TreeRef): Steps<boolean> {
    if (root.page === 0) return true;
    let readable = true;
    let leafDepth: number | null = null;
    const stack: { children: TreeRef[]; i: number; depth: number }[] = [];
    let next: { page: number; tag: Uint8Array; depth: number } | null = {
      page: root.page,
      tag: root.tag,
      depth: 0,
    };
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
        let children: TreeRef[] | null = null;
        try {
          pg = yield* this.readPage(node.page, node.tag);
          children = parseGist(pg, this.tagLen);
        } catch (e) {
          if (fatal(e)) throw e;
          readable = false;
          pg = null;
          this.add("page_corrupt", object, node.page, null, message(e));
        }
        if (pg !== null && children === null) {
          readable = false;
          this.add(
            "page_type_wrong",
            object,
            node.page,
            null,
            `page type ${pg.pageType} in a GiST tree`,
          );
        } else if (pg !== null && pg.pageType === PAGE_GIST_LEAF) {
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
        } else if (children !== null) {
          stack.push({ children, i: 0, depth: node.depth });
        }
      }
      while (stack.length > 0) {
        const top = stack[stack.length - 1]!;
        if (top.i < top.children.length) {
          const c = top.children[top.i++]!;
          next = { page: c.page, tag: c.tag, depth: top.depth + 1 };
          break;
        }
        stack.pop();
      }
    }
    return readable;
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

// parseGist returns a GiST page's child pointers ([] for a leaf, whose entries are parsed and
// discarded), or null for a page of another type (gist.md §4.1). A malformed entry is data_corrupted.
function parseGist(pg: ReturnType<typeof parsePage>, tagLen: number): TreeRef[] | null {
  const buf = pg.payload;
  let pos = 0;
  const take = (n: number): number => {
    if (n > buf.length - pos) throw engineError("data_corrupted", "unexpected end of page data");
    const at = pos;
    pos += n;
    return at;
  };
  const u16 = (): number => {
    const at = take(2);
    return (buf[at]! << 8) | buf[at + 1]!;
  };
  if (pg.pageType === PAGE_GIST_LEAF) {
    for (let i = 0; i < pg.itemCount; i++) {
      take(u16());
      take(u16());
    }
    return [];
  }
  if (pg.pageType !== PAGE_GIST_INTERIOR) return null;
  if (pg.itemCount * (6 + tagLen) > buf.length)
    throw engineError("data_corrupted", "GiST interior item count overruns page");
  const children: TreeRef[] = [];
  for (let i = 0; i < pg.itemCount; i++) {
    take(u16());
    const at = take(4);
    const page =
      ((buf[at]! << 24) | (buf[at + 1]! << 16) | (buf[at + 2]! << 8) | buf[at + 3]!) >>> 0;
    const tag = tagLen === 0 ? NO_TAG : buf.slice(take(tagLen), pos);
    if (page === 0 && tag.some((b) => b !== 0))
      throw engineError("data_corrupted", "a pointer to no page carries a tag");
    children.push({ page, tag });
  }
  return children;
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
  return yield* new Checker(target, level, max, opts.signal).run();
}
