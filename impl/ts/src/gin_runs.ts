// GIN posting runs (spec/design/gin.md §4.5, format_version 43). Mirrors impl/rust/src/gin_runs.rs.
//
// A GIN entry tree stores each term's posting list as runs: one record key holds the term, the run's
// last (largest) storage key, then the run's earlier storage keys front-coded, then the coded length.
// The record keys of one term sort by their last storage key, so the run that holds — or would hold —
// a storage key k is the first record at or after term ‖ k, and every term's runs are the prefix range
// [term, successor(term)). A from-scratch build packs each posting list greedily, and maintenance
// edits one run at a time by the rules below, so the same statements produce the same records in
// every core.

import { engineError } from "./errors.ts";
import { compareBytes, type KeyBound } from "./pmap.ts";
import type { TableStore } from "./storage.ts";

// GIN_RUN_MAX_BYTES is the longest run key maintenance builds by adding a storage key to a run
// (gin.md §4.5). A run of one storage key may be longer.
export const GIN_RUN_MAX_BYTES = 64;

function corrupt(): Error {
  return engineError("data_corrupted", "a GIN posting run is malformed");
}

function pushVarint(out: number[], v: number): void {
  for (;;) {
    const b = v & 0x7f;
    v >>>= 7;
    if (v === 0) {
      out.push(b);
      return;
    }
    out.push(b | 0x80);
  }
}

function sharedPrefix(a: Uint8Array, b: Uint8Array): number {
  let n = 0;
  while (n < a.length && n < b.length && a[n] === b[n]) n++;
  return n;
}

// ginEncodeRun is the run key of term over the ascending, distinct storage keys (non-empty):
// term ‖ last ‖ body ‖ u16 BE len(body), the body front-coding every key but the last, in ascending
// order, each as varint(shared) ‖ varint(suffix length) ‖ suffix against the previous key (the first
// against the last).
export function ginEncodeRun(term: Uint8Array, keys: Uint8Array[]): Uint8Array {
  const last = keys[keys.length - 1]!;
  const body: number[] = [];
  let prev = last;
  for (let i = 0; i < keys.length - 1; i++) {
    const k = keys[i]!;
    const shared = sharedPrefix(prev, k);
    pushVarint(body, shared);
    pushVarint(body, k.length - shared);
    for (let j = shared; j < k.length; j++) body.push(k[j]!);
    prev = k;
  }
  const out = new Uint8Array(term.length + last.length + body.length + 2);
  out.set(term, 0);
  out.set(last, term.length);
  out.set(body, term.length + last.length);
  out[out.length - 2] = (body.length >>> 8) & 0xff;
  out[out.length - 1] = body.length & 0xff;
  return out;
}

// ginDecodeRun is the ascending storage keys of a run key whose term is termLen bytes long. A
// malformed run is XX001.
export function ginDecodeRun(termLen: number, run: Uint8Array): Uint8Array[] {
  const n = run.length;
  if (n < termLen + 3) throw corrupt();
  const bodyLen = (run[n - 2]! << 8) | run[n - 1]!;
  const bodyEnd = n - 2;
  const lastEnd = bodyEnd - bodyLen;
  if (lastEnd <= termLen) throw corrupt();
  const last = run.subarray(termLen, lastEnd);
  const body = run.subarray(lastEnd, bodyEnd);
  const keys: Uint8Array[] = [];
  let at = 0;
  const read = (): number => {
    let v = 0;
    for (let shift = 0; shift < 21; shift += 7) {
      if (at >= body.length) throw corrupt();
      const c = body[at++]!;
      v |= (c & 0x7f) << shift;
      if ((c & 0x80) === 0) return v;
    }
    throw corrupt();
  };
  while (at < body.length) {
    const shared = read();
    const suffix = read();
    const prev = keys.length > 0 ? keys[keys.length - 1]! : last;
    if (shared > prev.length || at + suffix > body.length || shared + suffix === 0) throw corrupt();
    const key = new Uint8Array(shared + suffix);
    key.set(prev.subarray(0, shared), 0);
    key.set(body.subarray(at, at + suffix), shared);
    at += suffix;
    if (keys.length > 0 && compareBytes(keys[keys.length - 1]!, key) >= 0) throw corrupt();
    keys.push(key);
  }
  if (keys.length > 0 && compareBytes(keys[keys.length - 1]!, last) >= 0) throw corrupt();
  keys.push(last.slice());
  return keys;
}

// ginPackRuns is the runs of term over ascending, distinct keys, packed greedily: a run takes each
// next key while its key stays within GIN_RUN_MAX_BYTES (a from-scratch build, gin.md §4.5).
export function ginPackRuns(term: Uint8Array, keys: Uint8Array[]): Uint8Array[] {
  const runs: Uint8Array[] = [];
  let start = 0;
  while (start < keys.length) {
    let end = start + 1;
    while (
      end < keys.length &&
      ginEncodeRun(term, keys.slice(start, end + 1)).length <= GIN_RUN_MAX_BYTES
    )
      end++;
    runs.push(ginEncodeRun(term, keys.slice(start, end)));
    start = end;
  }
  return runs;
}

// splitRuns is the runs of a run's key list after an edit: one run when it fits, else the first
// ceil(n/2) keys and the rest, each split again the same way until it fits or holds one key.
function splitRuns(term: Uint8Array, keys: Uint8Array[], out: Uint8Array[]): void {
  const run = ginEncodeRun(term, keys);
  if (run.length <= GIN_RUN_MAX_BYTES || keys.length === 1) {
    out.push(run);
    return;
  }
  const mid = Math.ceil(keys.length / 2);
  splitRuns(term, keys.slice(0, mid), out);
  splitRuns(term, keys.slice(mid), out);
}

function successor(p: Uint8Array): Uint8Array | null {
  const s = Array.from(p);
  while (s.length > 0) {
    const last = s.pop()!;
    if (last !== 0xff) {
      s.push(last + 1);
      return Uint8Array.from(s);
    }
  }
  return null;
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

// ginTermBound is the posting range of term: every run key that starts with it.
export function ginTermBound(term: Uint8Array): KeyBound {
  return { lo: term, loInc: true, hi: successor(term), hiInc: false };
}

// ginRunAtOrAfter is the first run key of term at or after term ‖ key — the run holding key if any
// holds it.
export function ginRunAtOrAfter(
  store: TableStore,
  term: Uint8Array,
  key: Uint8Array,
): Uint8Array | null {
  const b = { ...ginTermBound(term), lo: concat(term, key) };
  let found: Uint8Array | null = null;
  store.scanRange(b, (k) => {
    found = k.slice();
    return false;
  });
  return found;
}

function runBefore(store: TableStore, term: Uint8Array, key: Uint8Array): Uint8Array | null {
  const b = { ...ginTermBound(term), hi: concat(term, key) };
  let found: Uint8Array | null = null;
  store.scanRangeRev(b, (k) => {
    found = k.slice();
    return false;
  });
  return found;
}

function replaceRun(store: TableStore, old: Uint8Array, runs: Uint8Array[]): void {
  store.remove(old);
  for (const r of runs) store.insert(r, []);
}

function searchKeys(keys: Uint8Array[], key: Uint8Array): { at: number; found: boolean } {
  let lo = 0;
  let hi = keys.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (compareBytes(keys[mid]!, key) < 0) lo = mid + 1;
    else hi = mid;
  }
  return { at: lo, found: lo < keys.length && compareBytes(keys[lo]!, key) === 0 };
}

// ginInsert adds the posting (term, key) (gin.md §4.5). The run at or after term ‖ key takes the key
// (it holds a larger last key), split in halves when it outgrows GIN_RUN_MAX_BYTES. A key above every
// run joins the term's last run when that run stays within the limit, else starts a new run. false
// when the posting is already present.
export function ginInsert(store: TableStore, term: Uint8Array, key: Uint8Array): boolean {
  const run = ginRunAtOrAfter(store, term, key);
  if (run !== null) {
    const keys = ginDecodeRun(term.length, run);
    const { at, found } = searchKeys(keys, key);
    if (found) return false;
    keys.splice(at, 0, key.slice());
    const runs: Uint8Array[] = [];
    splitRuns(term, keys, runs);
    replaceRun(store, run, runs);
    return true;
  }
  const prev = runBefore(store, term, key);
  if (prev !== null) {
    const keys = ginDecodeRun(term.length, prev);
    keys.push(key.slice());
    const grown = ginEncodeRun(term, keys);
    if (grown.length <= GIN_RUN_MAX_BYTES) {
      replaceRun(store, prev, [grown]);
      return true;
    }
  }
  return store.insert(ginEncodeRun(term, [key.slice()]), []);
}

// ginRemove removes the posting (term, key) (gin.md §4.5): its run loses the key, and an emptied run is
// removed. false when the posting is absent.
export function ginRemove(store: TableStore, term: Uint8Array, key: Uint8Array): boolean {
  const run = ginRunAtOrAfter(store, term, key);
  if (run === null) return false;
  const keys = ginDecodeRun(term.length, run);
  const { at, found } = searchKeys(keys, key);
  if (!found) return false;
  keys.splice(at, 1);
  const runs: Uint8Array[] = [];
  if (keys.length > 0) splitRuns(term, keys, runs);
  replaceRun(store, run, runs);
  return true;
}

// IndexEntry is one logical index entry with the length of its storage-key suffix.
export type IndexEntry = { key: Uint8Array; skeyLen: number };

// indexEntryInsert inserts one logical index entry (entry ends in a skeyLen-byte storage key). A GIN
// entry term ‖ storage_key joins its term's posting runs (gin.md §4.5); every other entry is its own
// record. false when the entry is already present.
export function indexEntryInsert(
  istore: TableStore,
  kind: string,
  entry: Uint8Array,
  skeyLen: number,
): boolean {
  if (kind === "gin") {
    const split = entry.length - skeyLen;
    return ginInsert(istore, entry.subarray(0, split), entry.subarray(split));
  }
  return istore.insert(entry, []);
}

// indexEntryRemove removes one logical index entry (see indexEntryInsert). false when it is absent.
export function indexEntryRemove(
  istore: TableStore,
  kind: string,
  entry: Uint8Array,
  skeyLen: number,
): boolean {
  if (kind === "gin") {
    const split = entry.length - skeyLen;
    return ginRemove(istore, entry.subarray(0, split), entry.subarray(split));
  }
  return istore.remove(entry);
}

// sortIndexEntries sorts entries by key (then storage-key length), the build order.
export function sortIndexEntries(entries: IndexEntry[]): void {
  entries.sort((a, b) => compareBytes(a.key, b.key) || a.skeyLen - b.skeyLen);
}

// indexBuildInsert fills an empty index store from its complete entry list, sorted by entry key
// (indexes.md §1): each entry its own record, or for GIN each term's storage keys packed greedily into
// posting runs (gin.md §4.5). Every insert is a right-edge append, so the built tree's shape is a
// function of the entry set — part of the byte contract.
export function indexBuildInsert(istore: TableStore, kind: string, entries: IndexEntry[]): void {
  const dup = () =>
    engineError("data_corrupted", "index entry keys are unique (storage-key suffix)");
  if (kind !== "gin") {
    for (const e of entries) if (!istore.insert(e.key, [])) throw dup();
    return;
  }
  let i = 0;
  while (i < entries.length) {
    const first = entries[i]!;
    const term = first.key.subarray(0, first.key.length - first.skeyLen);
    const keys: Uint8Array[] = [];
    let j = i;
    for (; j < entries.length; j++) {
      const e = entries[j]!;
      const split = e.key.length - e.skeyLen;
      if (compareBytes(e.key.subarray(0, split), term) !== 0) break;
      keys.push(e.key.subarray(split));
    }
    for (const run of ginPackRuns(term, keys)) if (!istore.insert(run, [])) throw dup();
    i = j;
  }
}
