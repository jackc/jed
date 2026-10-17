// The owner's validation of an incoming CommitDelta (spec/design/page-service.md §5.6;
// spec/pageservice/README.md §3.5). It is structural and cheap: it catches client bugs, not malice
// (page-service.md §13). Every check runs before the page it guards is written, so a rejected delta
// leaves only unreachable orphans — pages beyond the committed high-water or in the granted free list —
// and never touches a page any pinned snapshot or the fallback meta can reach.
//
// Browser-safe: no node: imports (the owner's service worker and the unit tests use it).

import {
  crc64Update,
  manifestInlineCapacity,
  manifestOverflowCapacity,
  pageChecksum,
  parseCommitMeta,
  type CommitMeta,
} from "../commit_manifest.ts";
import type { PsCommitBegin, PsIndexedPage } from "./codec.ts";

const PAGE_FREELIST = 7;
const PAGE_MANIFEST = 8;

// DeltaFailure is a delta the owner refuses: commit_end answers it with a fatal 08P01.
export class DeltaFailure extends Error {}

function failure(reason: string): DeltaFailure {
  return new DeltaFailure(`invalid commit delta: ${reason}`);
}

// DeltaBase is the owner state a delta is checked against, fixed at begin_write.
export type DeltaBase = {
  committedTxid: bigint;
  pageSize: number;
  // pageCount is the committed high-water.
  pageCount: number;
  // granted is the reusable free pages when the reuse grant passed; null otherwise.
  granted: Set<number> | null;
};

// legal reports whether a delta may write index: a body page beyond the committed high-water or a
// granted free page, below the new page count.
export function legal(base: DeltaBase, index: number, newPageCount: number): boolean {
  return (
    index >= 2 &&
    index < newPageCount &&
    (index >= base.pageCount || base.granted?.has(index) === true)
  );
}

function u32(b: Uint8Array, at: number): number {
  return ((b[at]! << 24) | (b[at + 1]! << 16) | (b[at + 2]! << 8) | b[at + 3]!) >>> 0;
}

function u64(b: Uint8Array, at: number): bigint {
  return (BigInt(u32(b, at)) << 32n) | BigInt(u32(b, at + 4));
}

function zero(b: Uint8Array, start: number, end = b.length): boolean {
  for (let i = start; i < end; i++) if (b[i] !== 0) return false;
  return true;
}

// Delta is one incoming CommitDelta after commit_begin passed: the decoded meta and manifest, and which
// listed pages have arrived.
export class Delta {
  readonly txid: bigint;
  readonly meta: Uint8Array;
  readonly parsed: CommitMeta;
  // manifest is the overflow chain, in chain order; it arrives whole in commit_begin.
  readonly manifest: PsIndexedPage[] = [];
  // written is every index the delta writes (listed and manifest pages), ascending: the invalidation
  // window's entry.
  readonly written: number[] = [];
  // expected maps every listed index to the CRC-64 of its bytes.
  private readonly expected = new Map<number, bigint>();
  private readonly received = new Set<number>();

  private constructor(txid: bigint, meta: Uint8Array, parsed: CommitMeta) {
    this.txid = txid;
    this.meta = meta;
    this.parsed = parsed;
  }

  // begin runs the checks before any page is written (README §3.5 check A): the meta, then the
  // manifest chain and every listed index.
  static begin(base: DeltaBase, cb: PsCommitBegin): Delta {
    if (cb.txid !== base.committedTxid + 1n) {
      throw failure(`txid ${cb.txid} does not follow the committed txid ${base.committedTxid}`);
    }
    if (cb.meta.length !== base.pageSize) throw failure("the meta page has the wrong size");
    const m = parseCommitMeta(cb.meta, base.pageSize, 0xffff_ffff);
    if (m === null) throw failure("the meta page does not parse");
    if (m.txid !== cb.txid) throw failure("the meta page names another txid");
    if (m.pageCount < base.pageCount) throw failure("the page count shrinks");
    if (m.dirtyCount === 0) throw failure("the manifest lists no page");
    const ps = base.pageSize;
    const d = new Delta(cb.txid, cb.meta.slice(), m);
    if (cb.manifestPages.length !== m.overflowCount) {
      throw failure("commit_begin carries the wrong number of manifest pages");
    }
    const protectedPages = new Set<number>();
    let last = 1;
    let rootSeen = false;
    let freeHeadSeen = m.freeListHead === 0;
    let listed = 0;
    const list = (entries: Uint8Array, at: number, count: number): void => {
      for (let i = 0; i < count; i++) {
        const e = at + i * 12;
        const index = u32(entries, e);
        if (index <= last) throw failure("manifest indices are not strictly ascending");
        if (!legal(base, index, m.pageCount)) {
          throw failure(`page ${index} is neither beyond the high-water nor a granted free page`);
        }
        last = index;
        d.expected.set(index, u64(entries, e + 4));
        protectedPages.add(index);
        rootSeen ||= index === m.rootPage;
        freeHeadSeen ||= index === m.freeListHead;
        listed++;
      }
    };
    list(cb.meta, 64, Math.min(m.dirtyCount, manifestInlineCapacity(ps)));
    let id = m.overflowHead;
    let chain = 0n;
    for (let ordinal = 0; ordinal < cb.manifestPages.length; ordinal++) {
      const pg = cb.manifestPages[ordinal]!;
      if (pg.index !== id || !legal(base, id, m.pageCount) || protectedPages.has(id)) {
        throw failure("the manifest chain names an illegal page");
      }
      const block = pg.page;
      const count = Math.min(m.dirtyCount - listed, manifestOverflowCapacity(ps));
      if (
        block.length !== ps ||
        pageChecksum(block) !== u32(block, 12) ||
        block[0] !== PAGE_MANIFEST ||
        !zero(block, 1, 4) ||
        u32(block, 4) !== count ||
        u64(block, 16) !== m.txid ||
        u32(block, 24) !== ordinal ||
        !zero(block, 28, 32) ||
        !zero(block, 32 + 12 * count)
      ) {
        throw failure("a manifest page is malformed");
      }
      const index = new Uint8Array(4);
      new DataView(index.buffer).setUint32(0, id, false);
      chain = crc64Update(crc64Update(chain, index), block);
      protectedPages.add(id);
      list(block, 32, count);
      d.manifest.push({ index: id, page: block.slice() });
      id = u32(block, 8);
    }
    if (id !== 0 || chain !== m.chainChecksum) {
      throw failure("the manifest chain does not match the meta");
    }
    if (listed !== m.dirtyCount || !rootSeen || !freeHeadSeen) {
      throw failure("the manifest does not cover the catalog root and free-list head");
    }
    d.written.push(...[...protectedPages].sort((a, b) => a - b));
    return d;
  }

  // checkPage runs check B before one body page is written: it is listed, not yet received, and
  // matches its manifest identity.
  checkPage(pg: PsIndexedPage): void {
    const crc = this.expected.get(pg.index);
    if (crc === undefined) throw failure(`page ${pg.index} is not listed in the manifest`);
    if (this.received.has(pg.index)) throw failure(`page ${pg.index} arrived twice`);
    const b = pg.page;
    if (b[0]! < 1 || b[0]! > 7 || pageChecksum(b) !== u32(b, 12) || crc64Update(0n, b) !== crc) {
      throw failure(`page ${pg.index} does not match its manifest identity`);
    }
    this.received.add(pg.index);
  }

  // complete runs check C at commit_end: every listed page arrived.
  complete(): void {
    if (this.received.size !== this.expected.size) {
      throw failure("commit_end before every listed page arrived");
    }
  }
}

// readFreeListChain reads the committed free list (the page_type 7 chain at head) through read, for the
// reuse grant's granted pages. A malformed chain yields null: the owner then grants no reuse.
export function readFreeListChain(
  read: (index: number) => Uint8Array,
  head: number,
  pageCount: number,
): number[] | null {
  const free: number[] = [];
  const chain = new Set<number>();
  for (let p = head; p !== 0; ) {
    if (p < 2 || p >= pageCount || chain.has(p)) return null;
    chain.add(p);
    const b = read(p);
    if (b[0] !== PAGE_FREELIST || pageChecksum(b) !== u32(b, 12)) return null;
    const count = u32(b, 4);
    if (count > Math.floor((b.length - 16) / 4)) return null;
    for (let i = 0; i < count; i++) free.push(u32(b, 16 + i * 4));
    p = u32(b, 8);
  }
  return free;
}
