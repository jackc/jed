// Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6): a consistent copy of the
// main database's committed snapshot, written to a new file, in one of two forms.
//
//   - Page-identical: every page reachable from the snapshot (the catalog chain, every table and index
//     B+tree, every GiST R-tree, every live overflow chain) and its free-list chain, each copied at its
//     own index; every other index below the snapshot's page count is zero; both meta slots carry the
//     snapshot's bootstrap meta (zero manifest fields). The copy keeps the page numbering and txid.
//   - Compacted: the from-scratch image of the snapshot at txid + 1, the file host compaction
//     (api.md §2.6) would leave for it.
//
// The backup pins the snapshot as a reader does and walks it with this process's own engine, so it
// works the same in every role: an owner or a handle outside the page service reads its own file, a
// page client reads through the owner with read_pages. It never takes the writer gate.
//
// A Node-host module (it writes through the OS file host by default), reached only from the test
// barrel (tooling.ts), so the browser bundle never imports it. Mirrors impl/go/backup.go.

import { dirname } from "node:path";

import { type CommitMeta, parseCommitMeta } from "./commit_manifest.ts";
import { engineError } from "./errors.ts";
import type { Engine, Snapshot } from "./executor.ts";
import type { Environment } from "./environment.ts";
import type { FileHost, WriteAt } from "./filehost.ts";
import { osFiles } from "./file.ts";
import {
  anySpillable,
  collectLeafOverflowWith,
  collectTreePages,
  loadEnginePagedAt,
  metaPage,
  PAGE_FREELIST,
  parsePage,
  ROOT_PAGE,
  writeImage,
} from "./format.ts";
import type { SharedPaging } from "./paging.ts";
import type { PageClientHandle, ServiceHandoffHost } from "./shared.ts";
import { compareTextC } from "./value.ts";

// BackupCore is the part of a Database's shared core a backup uses.
type BackupCore = {
  storage: Engine;
  committed: Snapshot;
  handoff: ServiceHandoffHost | null;
  client: PageClientHandle | null;
  roleGen: number;
  environment: Environment;
  refreshAndPin(): bigint;
  deregister(version: bigint, gen: number): void;
};

// backupForTesting backs up database main of db to dest, which must not exist: page-identical when
// pageIdentical is set, compacted otherwise. The host API's name and shape are not yet decided
// (spec/design/page-service.md §9); this test-only entry point drives the shared implementation.
export function backupForTesting(db: object, dest: string, pageIdentical: boolean): void {
  backup((db as { core: BackupCore }).core, dest, pageIdentical);
}

// BACKUP_META_RETRIES bounds the re-pins a page-identical backup makes when later commits overwrite its
// pinned version's meta slot between the pin and the read (each retry pins a newer version).
const BACKUP_META_RETRIES = 1000;

function backup(c: BackupCore, dest: string, pageIdentical: boolean): void {
  const st = c.storage;
  const files = st.fileHost ?? osFiles;
  if (pageIdentical && st.path === null && c.handoff === null) {
    throw engineError(
      "feature_not_supported",
      "a page-identical backup needs a file database; an in-memory database has only the compacted form",
    );
  }
  if (st.paging !== null && st.paging.codec() !== null) {
    // A backup of an encrypted file needs its key block taken under the key-block mutex
    // (spec/design/encryption.md §8), not yet built (spec/fileformat/encrypted.md §10).
    throw engineError(
      "feature_not_supported",
      "a backup of an encrypted database is not supported yet",
    );
  }
  if (files.exists(dest)) {
    throw engineError("duplicate_file", "database file already exists: " + dest);
  }
  for (let attempt = 0; ; attempt++) {
    const v = c.refreshAndPin();
    const gen = c.roleGen;
    const snap = c.committed;
    try {
      const paging = snap.storePaging;
      let noSync = c.handoff?.skipFsync ?? false;
      if (paging !== null) noSync = noSync || paging.skipsSync();
      const pageSize = st.pageSize;
      let fill: (writeAt: WriteAt) => void;
      if (pageIdentical) {
        if (paging === null) throw engineError("io_error", "database storage is closed");
        const mt = backupMeta(paging, v);
        if (mt === null) {
          // A later commit overwrote its meta slot: pin the newer version instead.
          if (attempt >= BACKUP_META_RETRIES) {
            throw engineError("object_in_use", "could not read a stable meta page for the backup");
          }
          continue;
        }
        const pages = backupPages(c, paging, mt);
        fill = (writeAt) => writePageIdentical(c, paging, pageSize, mt, pages, writeAt);
      } else {
        fill = (writeAt) => {
          writeImage(snap, pageSize, v + 1n, (index, page) => writeAt(index * pageSize, page));
        };
      }
      writeBackupFile(files, dest, noSync, fill);
      return;
    } finally {
      c.deregister(v, gen);
    }
  }
}

// backupMeta reads the meta that published version v. A commit writes the slot that does not hold the
// durable floor (validated-cow.md §4): txid & 1 under synchronous = on, so a commit two versions on can
// overwrite it after the pin, and one slot for a whole relaxed batch, so the next commit of the batch
// can. Either slot may hold it, and null means a later commit overwrote it.
function backupMeta(paging: SharedPaging, v: bigint): CommitMeta | null {
  const first = Number(v & 1n);
  for (const slot of [first, first ^ 1]) {
    const m = parseCommitMeta(paging.readBlock(slot), paging.pageSize(), 0xffff_ffff);
    if (m !== null && m.txid === v) return m;
  }
  return null;
}

// backupPages is the page-identical backup's page set (page-service.md §9), ascending: the pages
// reachable from mt's root — the catalog chain, every table and index B+tree node, every GiST R-tree
// page, every live overflow chain page — and mt's free-list chain. The reachable count must equal the
// meta's live page count (format.md *Live page count*); a mismatch is corruption.
export function backupPages(c: BackupCore, paging: SharedPaging, mt: CommitMeta): number[] {
  const reached = new Set<number>();
  // The catalog and the interior skeleton are loaded the way an adoption loads them.
  const loaded = loadEnginePagedAt(paging, c.environment, {
    meta: mt,
    gist: (p) => reached.add(p),
  });
  const snap = loaded.committed;
  const read = (p: number): Uint8Array => paging.readBlock(p);
  const seen = new Set<number>();
  for (let p = mt.rootPage; p !== 0; ) {
    if (seen.has(p) || p < ROOT_PAGE || p >= mt.pageCount) {
      throw engineError("data_corrupted", "invalid catalog chain");
    }
    seen.add(p);
    reached.add(p);
    p = parsePage(read(p)).nextPage;
  }
  const names = [...snap.stores.keys()].sort(compareTextC);
  for (const name of names) {
    const store = snap.stores.get(name)!;
    const root = store.treeRoot();
    collectTreePages(root, reached);
    if (root !== null && root.page !== 0 && anySpillable(store.columnTypes())) {
      collectLeafOverflowWith(read, root.page, store.columnTypes(), reached, new Set(), 1);
    }
  }
  for (const istore of snap.indexStores.values()) collectTreePages(istore.treeRoot(), reached);
  if (reached.size !== mt.livePages) {
    throw engineError(
      "data_corrupted",
      "the pages reachable from the snapshot do not match its live page count",
    );
  }
  for (let p = mt.freeListHead; p !== 0; ) {
    if (reached.has(p) || p < ROOT_PAGE || p >= mt.pageCount) {
      throw engineError("data_corrupted", "invalid free-list chain");
    }
    reached.add(p);
    const pg = parsePage(read(p));
    if (pg.pageType !== PAGE_FREELIST) {
      throw engineError("data_corrupted", "invalid free-list page");
    }
    p = pg.nextPage;
  }
  const pages: number[] = [];
  for (const p of reached) {
    if (p < ROOT_PAGE || p >= mt.pageCount) {
      throw engineError("data_corrupted", "a reachable page lies outside the snapshot");
    }
    pages.push(p);
  }
  pages.sort((a, b) => a - b);
  return pages;
}

// writePageIdentical writes the page-identical image: each page in pages copied at its index (a page
// client reads them in batches), every other index left zero, and the bootstrap meta in both slots.
function writePageIdentical(
  c: BackupCore,
  paging: SharedPaging,
  ps: number,
  mt: CommitMeta,
  pages: number[],
  writeAt: WriteAt,
): void {
  let read = (batch: number[]): Uint8Array[] => batch.map((p) => paging.readBlock(p));
  let batchSize = 1;
  const client = c.client;
  if (client !== null) {
    read = (batch) => client.readPages(batch);
    batchSize = client.maxReadPages();
  }
  for (let start = 0; start < pages.length; start += batchSize) {
    const batch = pages.slice(start, Math.min(start + batchSize, pages.length));
    const blocks = read(batch);
    for (let i = 0; i < blocks.length; i++) {
      const block = blocks[i]!;
      parsePage(block); // the page's CRC-32 (XX001)
      writeAt(batch[i]! * ps, block);
    }
  }
  const last = mt.pageCount - 1;
  if (pages.length === 0 || pages[pages.length - 1] !== last) {
    // The file must span the snapshot's page count: its trailing dead pages are zero.
    writeAt(last * ps, new Uint8Array(ps));
  }
  const bootstrap = metaPage(ps, mt.txid, mt.rootPage, mt.pageCount, mt.freeListHead, mt.livePages);
  writeAt(0, bootstrap);
  writeAt(ps, bootstrap);
}

// writeBackupFile writes dest the way create writes a new database (api.md §2.1): a sibling temp file,
// filled and synced, renamed into place, and the directory synced. A failure removes the temp file.
function writeBackupFile(
  files: FileHost,
  dest: string,
  noSync: boolean,
  fill: (writeAt: WriteAt) => void,
): void {
  const tmp = dest + ".jedtmp";
  try {
    files.writeFile(tmp, "", noSync, fill);
  } catch (e) {
    files.remove(tmp);
    throw e;
  }
  try {
    files.rename(tmp, dest);
  } catch (e) {
    files.remove(tmp);
    throw e;
  }
  if (!noSync) files.syncDir(dirname(dest));
}
