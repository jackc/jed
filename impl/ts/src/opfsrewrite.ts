// Storage replacement for the Browser/OPFS host (spec/design/hosts.md §5.1): compaction and rekey
// write the database's from-scratch image to a sibling journal file and copy it over the database in
// place. OPFS gives no atomic rename over an existing file, so the replace is made crash-safe by
// roll-forward instead: once the journal holds a complete image (its trailer is durable) the copy is
// committed, and an open that finds a complete journal finishes the copy before it reads the database.
// A journal without a valid trailer is an interrupted write and is discarded; the database was not
// touched. The database's own exclusive sync access handle is held throughout, so no other context can
// open it between the journal write and the copy.
//
// Like opfsblockstore.ts this module is browser-clean (no node:*). The journal's acquisition is async
// in a browser, so it reaches the engine as continuation steps (OpfsJournal); the Node parity tests
// supply a synchronous fake.

import { capFirstBody, capOf } from "./cap.ts";
import { crc32Ieee } from "./crc32.ts";
import { ClosedBlockStore } from "./blockstore.ts";
import { engineError } from "./errors.ts";
import type { Steps } from "./execution.ts";
import { writeImageSteps } from "./format.ts";
import { OpfsBlockStore, type SyncAccessHandle } from "./opfsblockstore.ts";
import { Pager } from "./pager.ts";
import { SharedPaging } from "./paging.ts";
import type { RekeyTarget, StorageRewriter } from "./shared.ts";
import type { Snapshot } from "./snapshot.ts";

// JOURNAL_SUFFIX names a database's journal file: `<name>.jedcompact`, beside it in the same directory.
export const JOURNAL_SUFFIX = ".jedcompact";

// The journal trailer, written after the image at byte offset L (the image length) once the image is
// durable: the magic, L as a big-endian u64, and the CRC-32/IEEE of those 16 bytes. A journal is
// complete exactly when its length is L + TRAILER_LEN and the trailer at L is valid.
const TRAILER_MAGIC = new TextEncoder().encode("JEDCMPT1");
const TRAILER_LEN = 20;

// COPY_CHUNK bounds the copy's buffer (bytes).
const COPY_CHUNK = 1 << 20;

// OpfsJournal reaches a database's journal file. acquire returns a sync access handle to it, creating
// an empty one when create is set, or null when it is absent and create is not set. discard closes the
// handle and removes the file. In a browser both are asynchronous (opfs.ts opfsJournal).
export interface OpfsJournal {
  acquire(create: boolean): Steps<SyncAccessHandle | null>;
  discard(handle: SyncAccessHandle): Steps<void>;
}

// opfsRewriter is the OPFS host's StorageRewriter over the database's held sync access handle main and
// its journal (shared.ts compactStorageSteps / rekeyStorageSteps).
export function opfsRewriter(main: SyncAccessHandle, journal: OpfsJournal): StorageRewriter {
  return (snap, pageSize, txid, old, capacity, rekey) =>
    rewriteSteps(main, journal, snap, pageSize, txid, old, capacity, rekey);
}

// rewriteSteps writes snap's from-scratch image at txid to the journal (under rekey's codec when given,
// else old's), makes it durable, writes and flushes the trailer, then copies it over main and discards
// the journal. A failure before the trailer is durable leaves main and old as they were. Past it, old
// is closed and poisoned: a failed copy leaves the handle needing a reopen, which finishes the copy.
function* rewriteSteps(
  main: SyncAccessHandle,
  journal: OpfsJournal,
  snap: Snapshot,
  pageSize: number,
  txid: bigint,
  old: SharedPaging,
  capacity: number,
  rekey?: RekeyTarget,
): Steps<SharedPaging> {
  // An encrypted database keeps its data key and key block; every page is resealed under fresh
  // nonces (spec/design/encryption.md §5). A rekey writes under its target codec instead.
  const codec = rekey === undefined ? old.codec() : rekey.codec;
  const jh = (yield* journal.acquire(true))!;
  let length: number;
  try {
    jh.truncate(0);
    const store = new OpfsBlockStore(jh);
    const pageCount = yield* writeImageSteps(
      snap,
      pageSize,
      txid,
      (index, page) => store.writeAt(index * pageSize, page),
      codec,
    );
    if (rekey !== undefined && rekey.limit > 0n) {
      // A from-scratch image holds no free page: every body page is live (memory.md §8.7).
      const live = pageCount - capFirstBody(capOf(pageSize, codec !== null));
      if (BigInt(live) * BigInt(pageSize) > rekey.limit) {
        throw engineError(
          "storage_limit_exceeded",
          `storage of database "${rekey.name}" exceeded the limit of ${rekey.limit} bytes`,
        );
      }
    }
    length = pageCount * pageSize;
    jh.flush(); // the image is durable before the trailer that commits it
    store.writeAt(length, trailer(length));
    jh.flush();
  } catch (e) {
    yield* discardQuietly(journal, jh);
    throw e;
  }
  // The trailer is durable: the replace is committed, and an open after a crash finishes it. Close
  // old over its store (a stale snapshot fails closed rather than reading the new layout).
  const store = old.swapStore(new ClosedBlockStore());
  old.poison();
  try {
    copyJournal(jh, main, length);
  } catch (e) {
    // Keep the complete journal: the next open copies it again.
    try {
      jh.close();
    } catch {
      // best-effort release
    }
    throw e;
  }
  yield* finishJournal(journal, jh);
  return new SharedPaging(Pager.fromStore(store), capacity, codec);
}

// recoverJournalSteps finishes or discards a replace an earlier handle left behind, before the
// database main is read (opfs.ts open): a complete journal is copied over main, and the journal is
// then removed whatever it held.
export function* recoverJournalSteps(main: SyncAccessHandle, journal: OpfsJournal): Steps<void> {
  const jh = yield* journal.acquire(false);
  if (jh === null) return;
  try {
    const length = completeLength(jh);
    if (length !== null) copyJournal(jh, main, length);
  } catch (e) {
    try {
      jh.close();
    } catch {
      // best-effort release
    }
    throw e;
  }
  yield* finishJournal(journal, jh);
}

// discardJournalSteps removes a journal left beside a database name a create is about to reuse: a
// stale journal must never be copied over a new database.
export function* discardJournalSteps(journal: OpfsJournal): Steps<void> {
  const jh = yield* journal.acquire(false);
  if (jh !== null) yield* discardQuietly(journal, jh);
}

// trailer is the journal trailer for an image of length bytes.
function trailer(length: number): Uint8Array {
  const out = new Uint8Array(TRAILER_LEN);
  out.set(TRAILER_MAGIC, 0);
  new DataView(out.buffer).setBigUint64(8, BigInt(length));
  new DataView(out.buffer).setUint32(16, crc32Ieee(out.subarray(0, 16)));
  return out;
}

// completeLength is the image length of a complete journal, or null for an incomplete one.
function completeLength(jh: SyncAccessHandle): number | null {
  const size = jh.getSize();
  if (size < TRAILER_LEN) return null;
  const buf = new Uint8Array(TRAILER_LEN);
  if (jh.read(buf, { at: size - TRAILER_LEN }) !== TRAILER_LEN) return null;
  for (let i = 0; i < TRAILER_MAGIC.length; i++) if (buf[i] !== TRAILER_MAGIC[i]) return null;
  const view = new DataView(buf.buffer);
  if (view.getUint32(16) !== crc32Ieee(buf.subarray(0, 16))) return null;
  const length = view.getBigUint64(8);
  return length === BigInt(size - TRAILER_LEN) ? size - TRAILER_LEN : null;
}

// copyJournal copies the journal's first length bytes over main, sets main's length to length and
// makes it durable. Repeating it after a partial run gives the same file.
function copyJournal(jh: SyncAccessHandle, main: SyncAccessHandle, length: number): void {
  const from = new OpfsBlockStore(jh);
  const to = new OpfsBlockStore(main);
  for (let at = 0; at < length; at += COPY_CHUNK) {
    const n = Math.min(COPY_CHUNK, length - at);
    to.writeAt(at, from.readAt(at, n));
  }
  if (main.getSize() > length) main.truncate(length);
  main.flush();
}

// finishJournal empties the journal (so a crash before its removal leaves nothing to copy) and removes
// it. The database is already replaced, so a failure here is ignored: an empty journal left behind is
// removed by the next open.
function* finishJournal(journal: OpfsJournal, jh: SyncAccessHandle): Steps<void> {
  try {
    jh.truncate(0);
    jh.flush();
  } catch {
    // a complete journal left behind is copied again by the next open, which changes nothing
  }
  yield* discardQuietly(journal, jh);
}

// discardQuietly closes and removes the journal, ignoring a failure (the next open retries it).
function* discardQuietly(journal: OpfsJournal, jh: SyncAccessHandle): Steps<void> {
  try {
    yield* journal.discard(jh);
  } catch {
    try {
      jh.close();
    } catch {
      // already released
    }
  }
}
