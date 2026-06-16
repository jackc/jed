// The incremental copy-on-write commit — the host-INDEPENDENT durable-write recipe (spec/design/
// storage.md §4, spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9).
// Shared by every storage host (file.ts Node `fs`, opfs.ts Browser/OPFS), because the recipe is
// identical across hosts: the only per-host code is the BlockStore beneath SharedPaging (spec/design/
// hosts.md §3). Installed as a Database's persistHook by the host bootstrap and called by commitTx with
// the working snapshot being published.
//
// Browser-clean: imports only host-agnostic core, no `node:*`, so it lands in a browser bundle.

import type { Database, Snapshot } from "./executor.ts";
import { incrementalImage, metaPage } from "./format.ts";

// persistImpl durably publishes snap to the backing store via an incremental commit: write the dirty
// pages this transaction introduced — reusing free-list pages a prior root abandoned before extending
// the file (P6.2) — sync, write the alternate meta slot (snap.txid & 1), sync. Clean pages are never
// rewritten. A crash between the two syncs leaves the prior meta — and thus the prior snapshot — intact
// (its pages were not overwritten: a reused free page is reachable from no live snapshot). An in-memory
// database has no paging context — a no-op success (committed swaps in commitTx after this). pageCount /
// freePages advance only after both syncs succeed, so a write failure leaves db, committed, and the
// file's prior meta untouched (the working snapshot is then discarded). The future synchronous=off mode
// gates here.
export function persistImpl(db: Database, snap: Snapshot): void {
  if (db.paging === null) return;
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging);
  // Preallocate the file ahead of the high-water in chunks, so this commit's body write — and most
  // later commits' — lands in already-allocated space and the body sync below carries no file-growth
  // metadata journaling on the file hosts (spec/design/pager.md §7). On OPFS, with a single flush()
  // barrier, the preallocation is harmless (hosts.md §5).
  db.paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    db.paging.writeBlock(pg.index, pg.bytes);
  }
  db.paging.sync(); // body pages durable before the meta can reference them
  const meta = metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount);
  db.paging.writeBlock(Number(snap.txid & 1n), meta);
  db.paging.sync(); // the commit is published
  db.pageCount = write.pageCount;
  db.freePages = write.freeRemaining;
}
