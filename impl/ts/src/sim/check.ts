// The deterministic-simulation integrity checkers (spec/design/simulation-testing.md §8.3), a port
// of impl/go/sim_check_test.go.
//
// The image checker opens a copy of the disk image the way a new process would — so the meta
// selection and manifest validation of validated-cow.md §1 run first — and then walks the selected
// generation:
//   - the live page count at meta offset 56 equals the pages reachable from the catalog root, GiST
//     R-tree pages included;
//   - every reachable page lies in [2, page_count);
//   - the free list has no duplicates and is disjoint from the reachable set and from the selected
//     generation's manifest dependencies;
//   - each table's storage keys ascend strictly and equal the primary-key encoding of their rows;
//   - each table's persisted row count equals its leaf total;
//   - each column index holds exactly the entries derived from its table's rows;
//   - no overflow page is named by two stored values (large-values.md §15).
//
// The live checker checks the running process instead (see checkLive).

import { indexColumnOrdinals } from "../catalog.ts";
import type { Collation } from "../collation.ts";
import { type CommitMeta, parseCommitMeta, validateCommitMeta } from "../commit_manifest.ts";
import { type Engine, encodePkKey, indexEntryKeysColumns, type Snapshot } from "../executor.ts";
import { capFirstBody, NO_TAG } from "../cap.ts";
import type { Credential } from "../crypto.ts";
import { Environment } from "../environment.ts";
import { ioSteps, runSync } from "../execution.ts";
import {
  chainPages,
  chainStep,
  collectTreePages,
  loadEnginePagedAt,
  parsePage,
  reachablePages,
  unlockPagerSteps,
} from "../format.ts";
import { type GistPageRead, readGistLeafKeysSteps } from "../gist.ts";
import { MemoryBlockStore } from "../memoryblockstore.ts";
import { Pager } from "../pager.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "../paging.ts";
import type { Database } from "../shared.ts";
import type { PNode } from "../pmap.ts";
import type { Value } from "../value.ts";
import { bytesEqual, compareBytes, compareUtf8 } from "./util.ts";

// CheckError names the first violated invariant; cause is the engine error behind it, when one is, so
// the runner can read its SQLSTATE as Go's simCode does through a wrapped error.
export class CheckError extends Error {
  constructor(msg: string, cause?: unknown) {
    super(msg, cause === undefined ? undefined : { cause });
  }
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const PAGE_GIST_INTERIOR = 6;
const PAGE_GIST_LEAF = 5;

// selectMeta repeats the loader's meta selection (format.ts loadEnginePaged) against an already
// loaded paging context: the highest-txid parseable meta whose manifest validates. It returns the
// meta and the generation's manifest-protected pages.
function selectMeta(paging: SharedPaging): { meta: CommitMeta; protectedPages: Set<number> } {
  const pageSize = paging.pageSize();
  const physical = paging.physicalPages();
  const candidates = [paging.readBlock(0), paging.readBlock(1)]
    .map((bytes) => ({ bytes, meta: parseCommitMeta(bytes, pageSize, physical) }))
    .filter((c): c is { bytes: Uint8Array; meta: CommitMeta } => c.meta !== null)
    .sort((a, b) => (a.meta.txid > b.meta.txid ? -1 : a.meta.txid < b.meta.txid ? 1 : 0));
  for (const c of candidates) {
    const validated =
      paging.cachedValidation(c.bytes) ??
      validateCommitMeta(c.meta, pageSize, (p) => paging.readBlock(p));
    if (validated !== null) return { meta: c.meta, protectedPages: validated };
  }
  throw new CheckError("meta: no valid meta page");
}

// checkImage is simIntegrity: it returns [live, pages, free] from the selected meta and the loaded
// free list, or throws a CheckError naming the first violated invariant. credential opens an encrypted
// image (S12).
export function checkImage(
  image: Uint8Array,
  want: bigint,
  credential?: Credential,
): [number, number, number] {
  let e: Engine;
  let paging: SharedPaging;
  // A GiST R-tree is read whole at load (gist.md §4.1); its pages are the ones the load visits, which
  // the reachability walk does not list.
  const gist: number[] = [];
  try {
    const pager = Pager.fromStore(new MemoryBlockStore(image));
    const environment = new Environment();
    const codec = runSync(unlockPagerSteps(pager, credential, environment));
    paging = new SharedPaging(pager, cacheLeaves(DEFAULT_CACHE_BYTES, pager.pageSize), codec);
    e = loadEnginePagedAt(paging, environment, { gist: (pg) => gist.push(pg) });
  } catch (err) {
    throw new CheckError("open: " + message(err), err);
  }
  const { meta: mt, protectedPages } = selectMeta(paging);
  if (mt.txid !== want) {
    throw new CheckError(`the image holds version ${mt.txid}, want ${want}`);
  }
  const snap = e.committed;
  let reached: Set<number>;
  try {
    reached = reachablePages(snap, paging, mt.rootPage, mt.rootTag);
  } catch (err) {
    throw new CheckError("reachability walk: " + message(err), err);
  }
  for (const pg of gist) {
    if (reached.has(pg)) throw new CheckError(`GiST page ${pg} is reachable twice`);
    reached.add(pg);
  }
  if (reached.size !== mt.livePages) {
    throw new CheckError(
      `live page count ${mt.livePages}, but ${reached.size} pages are reachable`,
    );
  }
  // An encrypted file's body pages start after its key block (encrypted.md §1).
  const first = capFirstBody(paging.cap());
  for (const pg of reached) {
    if (pg < first || pg >= mt.pageCount) {
      throw new CheckError(`reachable page ${pg} outside [${first}, ${mt.pageCount})`);
    }
  }
  const seen = new Set<number>();
  for (const pg of e.freePages) {
    if (seen.has(pg)) throw new CheckError(`free page ${pg} listed twice`);
    if (pg < first || pg >= mt.pageCount) {
      throw new CheckError(`free page ${pg} outside [${first}, ${mt.pageCount})`);
    }
    if (reached.has(pg)) throw new CheckError(`free page ${pg} is reachable`);
    if (protectedPages.has(pg)) {
      throw new CheckError(`free page ${pg} is a dependency of the selected generation`);
    }
    seen.add(pg);
  }
  const claimed = new Set<number>();
  for (const name of [...snap.tables.keys()].sort(compareUtf8)) {
    try {
      checkTable(snap, name, paging, claimed);
    } catch (err) {
      throw new CheckError(
        `table ${name}: ${message(err)}`,
        err instanceof CheckError ? err.cause : err,
      );
    }
  }
  return [mt.livePages, mt.pageCount, e.freePages.length];
}

// checkTable also checks chain exclusivity (large-values.md §15): every overflow page the snapshot
// reaches is named by exactly one stored value, across all tables (claimed). A rewritten record may
// keep an unchanged value's chain, but two records of one snapshot never share one.
function checkTable(
  snap: Snapshot,
  name: string,
  paging: SharedPaging,
  claimed: Set<number>,
): void {
  const fetch = (p: number, tag: Uint8Array): Uint8Array => paging.readBody(p, tag);
  const tagLen = paging.cap().tag;
  const table = snap.tables.get(name)!;
  const store = snap.stores.get(name);
  if (store === undefined) throw new CheckError("no row store");
  const colTypes = store.columnTypes();
  const entries = store.entriesInKeyOrder();
  const colls: (Collation | null)[] = table.columns.map((c) =>
    c.collation !== null ? (snap.resolveCollation(c.collation) ?? null) : null,
  );
  for (let i = 0; i < entries.length; i++) {
    for (const v of entries[i]!.row) {
      const ext = externalRef(v);
      if (ext === null) continue;
      let pages: number[];
      try {
        pages = chainPages(ext[0], ext[1], fetch, ext[2], tagLen);
      } catch (err) {
        throw new CheckError(`row ${i}: an overflow chain: ${message(err)}`, err);
      }
      for (const pg of pages) {
        if (claimed.has(pg)) throw new CheckError(`overflow page ${pg} is referenced twice`);
        claimed.add(pg);
      }
    }
    let row: (typeof entries)[number]["row"];
    try {
      row = store.resolveAll(entries[i]!.row);
    } catch (err) {
      throw new CheckError(`row ${i}: ${message(err)}`, err);
    }
    entries[i] = { key: entries[i]!.key, row };
    if (i > 0 && compareBytes(entries[i - 1]!.key, entries[i]!.key) >= 0) {
      throw new CheckError(`storage keys out of order at row ${i}`);
    }
    if (table.pk.length > 0) {
      const want = encodePkKey(colTypes, table.pk, colls, row);
      if (!bytesEqual(want, entries[i]!.key)) {
        throw new CheckError(`row ${i} is stored under a key that is not its primary key`);
      }
    }
  }
  const count = store.count();
  if (count !== null && count !== BigInt(entries.length)) {
    throw new CheckError(`persisted row count ${count}, but the tree holds ${entries.length} rows`);
  }
  for (const def of table.indexes) {
    if (indexColumnOrdinals(def) === null || def.predicate !== undefined || def.kind !== "btree") {
      continue; // expression, partial, GIN, and GiST indexes are not generated yet
    }
    const want: Uint8Array[] = [];
    for (const en of entries) {
      want.push(...indexEntryKeysColumns(table.columns, colTypes, colls, def, en.key, en.row));
    }
    want.sort(compareBytes);
    const ist = snap.indexStores.get(def.name);
    if (ist === undefined) throw new CheckError(`index ${def.name} has no store`);
    const got = ist.entriesInKeyOrder();
    if (got.length !== want.length) {
      throw new CheckError(`index ${def.name} holds ${got.length} entries, want ${want.length}`);
    }
    for (let i = 0; i < got.length; i++) {
      if (!bytesEqual(got[i]!.key, want[i]!)) {
        throw new CheckError(`index ${def.name} entry ${i} does not match its table`);
      }
    }
  }
}

// The runtime-private shared core of a Database, reached for this white-box check only.
type CoreView = { committed: Snapshot; storage: Engine };

const TAG_EXTERNAL = 0x02;
const TAG_EXTERNAL_COMP = 0x04;

// checkLive checks the LIVE process, not a fresh open of its image: every page its committed
// in-memory trees reference — tree nodes, and the overflow chains reached through resident rows'
// unfetched references as well as faulted leaves — must lie below the page high-water and off the
// in-memory free list the next commit allocates from. A fresh open re-derives both from disk, so
// only this view sees a resident node whose references went stale.
export function checkLive(db: Database): void {
  const core = (db as unknown as { core: CoreView }).core;
  const snap = core.committed;
  const st = core.storage;
  const pageCount = st.pageCount;
  const free = new Set(st.freePages);
  const paging = st.paging!;
  const fetch = (p: number, tag: Uint8Array): Uint8Array => paging.readBody(p, tag);
  const tagLen = paging.cap().tag;
  const check = (what: string, pg: number): void => {
    if (pg >= pageCount) {
      throw new CheckError(`${what} references page ${pg} past the high-water ${pageCount}`);
    }
    if (free.has(pg))
      throw new CheckError(`${what} references page ${pg}, which is on the free list`);
  };
  const claimed = new Set<number>();
  for (const name of [...snap.stores.keys()].sort(compareUtf8)) {
    const store = snap.stores.get(name)!;
    const nodes = new Set<number>();
    collectTreePages(store.treeRoot(), nodes);
    for (const pg of nodes) check(`table ${name}'s tree`, pg);
    let entries: ReturnType<typeof store.entriesInKeyOrder>;
    try {
      entries = store.entriesInKeyOrder();
    } catch (err) {
      throw new CheckError(`table ${name}: ${message(err)}`);
    }
    for (const en of entries) {
      for (const v of en.row) {
        const ext = externalRef(v);
        if (ext === null) continue;
        let pages: number[];
        try {
          pages = chainPages(ext[0], ext[1], fetch, ext[2], tagLen);
        } catch (err) {
          throw new CheckError(`table ${name}: an overflow chain: ${message(err)}`);
        }
        for (const pg of pages) {
          check(`table ${name}'s overflow chain`, pg);
          // Chain exclusivity (large-values.md §15): a kept chain moves to the rewritten record; two
          // committed records never share one.
          if (claimed.has(pg)) {
            throw new CheckError(`table ${name}: overflow page ${pg} is referenced twice`);
          }
          claimed.add(pg);
        }
      }
    }
  }
  for (const [name, ist] of snap.indexStores) {
    const nodes = new Set<number>();
    collectTreePages(ist.treeRoot(), nodes);
    for (const pg of nodes) check(`index ${name}'s tree`, pg);
  }
}

// externalRef returns the overflow chain a stored value names — its first page, the bytes the chain
// carries, and the tag its pointer binds (encrypted files) — or null for an inline or resident value.
function externalRef(v: Value): [number, number, Uint8Array] | null {
  if (v.kind !== "unfetched") return null;
  if (v.ref.form !== TAG_EXTERNAL && v.ref.form !== TAG_EXTERNAL_COMP) return null;
  return [v.ref.firstPage, v.ref.storedLen, v.ref.firstTag ?? NO_TAG];
}

// --- page replay (simulation-testing.md §8.3 *Replays*, slice S13) ----------------------------

// pointerKinds classifies the pages of image's selected generation by the kind of bound pointer that
// names each (encrypted.md §2), and returns the pages that generation's manifest depends on: its listed
// pages and its manifest-overflow pages. A page's kind is its pointer's, so a chain's head is
// `catalog`, `free`, or an overflow kind, and every later page of the chain its `_next` kind; a tree's
// root is `root` and every other node `child` (`gist_child` in a GiST R-tree). Engine errors propagate.
export function pointerKinds(
  image: Uint8Array,
  credential?: Credential,
): { kinds: Map<string, number[]>; deps: Set<number> } {
  const pager = Pager.fromStore(new MemoryBlockStore(image));
  const environment = new Environment();
  const codec = runSync(unlockPagerSteps(pager, credential, environment));
  const paging = new SharedPaging(pager, cacheLeaves(DEFAULT_CACHE_BYTES, pager.pageSize), codec);
  const gist: number[] = [];
  const e = loadEnginePagedAt(paging, environment, { gist: (pg) => gist.push(pg) });
  const { meta: mt, protectedPages } = selectMeta(paging);
  const deps = new Set(protectedPages);
  const sets = new Map<string, Set<number>>();
  const add = (kind: string, pg: number): void => {
    let set = sets.get(kind);
    if (set === undefined) {
      set = new Set();
      sets.set(kind, set);
    }
    set.add(pg);
  };
  const cap = paging.cap();
  const chain = (head: number, headTag: Uint8Array, first: string, next: string): void => {
    let kind = first;
    let tag = headTag;
    for (let p = head; p !== 0; kind = next) {
      add(kind, p);
      const link = chainStep(paging.readBody(p, tag), cap);
      p = link.next;
      tag = link.nextTag;
    }
  };
  chain(mt.rootPage, mt.rootTag, "catalog", "catalog_next");
  chain(mt.freeListHead, mt.freeListTag, "free", "free_next");
  // Manifest-overflow pages are plaintext; the meta's tag binds them through its associated data.
  for (let id = mt.overflowHead, n = 0; id !== 0 && n < mt.overflowCount; n++) {
    add("manifest", id);
    id = parsePage(paging.readBlock(id)).nextPage;
  }
  const walk = (n: PNode, kind: string): void => {
    if (n.page !== 0) add(kind, n.page);
    for (const c of n.children) {
      if (c.node === null) add("child", c.page);
      else walk(c.node, "child");
    }
  };
  const snap = e.committed;
  const fetch = (p: number, tag: Uint8Array): Uint8Array => paging.readBody(p, tag);
  for (const st of snap.stores.values()) {
    const root = st.treeRoot();
    if (root !== null) walk(root, "root");
    for (const en of st.entriesInKeyOrder()) {
      for (const v of en.row) {
        const ext = externalRef(v);
        if (ext === null) continue;
        const pages = chainPages(ext[0], ext[1], fetch, ext[2], cap.tag);
        const head =
          v.kind === "unfetched" && v.ref.form === TAG_EXTERNAL_COMP ? "overflow_lz4" : "overflow";
        for (let k = 0; k < pages.length; k++) add(k === 0 ? head : "overflow_next", pages[k]!);
      }
    }
  }
  for (const ist of snap.indexStores.values()) {
    const root = ist.treeRoot();
    if (root !== null && root.page !== 0) walk(root, "root");
  }
  // A GiST R-tree is read whole at load (gist.md §4.1): its pages are the ones the load visited. Each
  // interior's children come from the loader's own parser, reading only that page; whatever no
  // interior names is a root.
  const named = new Set<number>();
  for (const pg of gist) {
    const stored = paging.readBlock(pg);
    // The load already checked the stored tag against its pointer.
    const tag = cap.tag === 0 ? NO_TAG : stored.slice(stored.length - cap.tag);
    const parsed = parsePage(paging.readBody(pg, tag));
    if (parsed.pageType !== PAGE_GIST_INTERIOR) continue;
    runSync(
      readGistLeafKeysSteps(
        (p) =>
          ioSteps((): GistPageRead => {
            if (p !== pg) {
              named.add(p);
              return { pageType: PAGE_GIST_LEAF, itemCount: 0, payload: new Uint8Array() };
            }
            return {
              pageType: parsed.pageType,
              itemCount: parsed.itemCount,
              payload: parsed.payload,
            };
          }),
        pg,
        [],
        new Set<number>(),
        1,
        tag,
        cap.tag,
      ),
    );
  }
  for (const pg of gist) add(named.has(pg) ? "gist_child" : "root", pg);
  const kinds = new Map<string, number[]>();
  for (const [kind, set] of sets)
    kinds.set(
      kind,
      [...set].sort((a, b) => a - b),
    );
  return { kinds, deps };
}
