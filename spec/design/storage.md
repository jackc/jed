# Storage seam — design

> The reasoning behind the storage architecture: the block interface every host
> implements, the page model, how it carries the §3 commit model, and how it stays
> pluggable. This is a *design* doc — there is no storage data table yet; the byte-exact
> on-disk **format** (and its fixtures) is authored in [../fileformat/](../fileformat/)
> when the first slice needs to persist (CLAUDE.md §11 step 5). When a decision here
> changes, update [CLAUDE.md](../../CLAUDE.md) §3/§9 in the same edit.

The engine is embeddable and single-file (CLAUDE.md §1, §9). The **storage seam** is the
narrow interface between the language-independent core logic and the host's actual storage
(a file, OPFS, memory). Designing it early is what makes "single-file, embeddable,
everywhere" real rather than retrofitted (CLAUDE.md §9).

## 1. Design targets (CLAUDE.md §9)

- **Durable on-disk storage is the dominant mode; the dataset is RAM-sized.** Two facts at
  once (CLAUDE.md §9). Persistent disk databases are the overwhelming-majority case — *not*
  ephemeral in-memory ones — so **durability is core** (crash recovery, ordered writes, fsync
  at commit); a pure in-memory database is a real but minority mode. And the dataset is
  typically small enough to be **fully resident**, so the in-memory representation is still a
  **first-class concern** (a fully-resident working set, not a partial cache) and warm reads
  are served from memory. The core operates on in-memory structures; persistence is how those
  structures are made durable, and for the dominant use case it is **always present**, not
  optional.
- **Must not preclude larger-than-RAM (TB-scale) datasets.** RAM-sized is the dominant case,
  not a hard limit (CLAUDE.md §9): the engine must eventually serve a **TB-scale file far
  larger than RAM without falling over**, and nothing here may foreclose it. The
  non-foreclosure hooks are the page-structured format (§5), the page/block storage seam
  (§2), order-preserving keys (encoding.md), and *logical* per-page cost metering (cost.md).
  The deferred path (all Phase 6, none foreclosed): demand paging / a bounded **buffer pool**
  (resident set = a cache of pages with eviction), incremental COW commit (dirty pages only;
  large write sets stage to disk pages, not all RAM — §4), B-tree interior pages (replace the
  flat record chain — §6), and streaming + **spill-to-disk** blocking operators (sort / hash
  join / aggregate / DISTINCT under a memory budget). **Binding rule for present work:** no
  code above the storage seam may assume full residency — no "load = whole file into one
  buffer," no operator that requires its whole input/output in RAM. The whole-image
  load/commit and flat record chain (§6) are deliberately-narrowed current forms, not the
  permanent shape.
- **SSD-backed persistence.** Persistence targets SSDs: page-aligned I/O,
  write-amplification awareness, no HDD seek-minimization heuristics.
- **Single file per database.** One database = one file (plus possibly a transient
  side file during commit — see §4).

## 2. The seam: a block device interface

The core sees storage as a flat array of fixed-size **blocks** (pages) addressed by index,
behind a tiny interface each host implements:

```
read_block(index)        -> bytes            # read one page
write_block(index, bytes)                     # stage one page write
allocate_block()         -> index             # grow by one page
sync()                                         # durability barrier (fsync-equivalent)
block_count()            -> count
```

Hosts:

- **Go core** — `os.File` with `ReadAt`/`WriteAt`/`Sync` (pure Go, no cgo — CLAUDE.md §2).
- **Rust core** — direct file access (`pread`/`pwrite`/`fsync`-equivalent).
- **TS core** — a Node host (direct `fs`) exists now; the browser host (OPFS,
  `FileSystemSyncAccessHandle`) is still later (CLAUDE.md §9). The engine is kept
  host-agnostic so the browser host is an added seam, not a reshape.
- **In-memory** — a `Vec`/slice of pages; the natural fit for the in-RAM target and the
  default for tests (no filesystem, fully deterministic).

The core logic above this seam is **identical across implementations** and storage-host
agnostic. Only the few methods above are per-host. Keep the interface this small: every
method is something every host (including OPFS and pure memory) can implement cheaply.

## 3. Page model

- **Fixed-size pages**, page-aligned (SSD target, §1). **Default 8 KiB**, recorded in the
  file header so it is a format parameter, not a hardcoded constant — matches PostgreSQL's
  default block size (a well-proven choice on SSD/OS page granularity) and stays revisitable
  per the fixtures without code assumptions.
- **Page 0 is the header / meta page**: magic number, format version, page size, and the
  **root pointer(s)** for the committed state (§4). Byte layout is specified in
  [../fileformat/](../fileformat/) with fixtures, including the cross-core round-trip test
  (a file written by Rust must be byte-readable by Go and vice versa — CLAUDE.md §8).
- Keys within a page (and across pages, via the page structure) are stored in the
  order-preserving key encoding ([encoding.md](encoding.md)), so iteration is raw byte
  order — no comparator callback into the core.

## 4. Commit model (carries CLAUDE.md §3)

The §3 concurrency rule — single writer, readers blocked only during the commit window —
lands in the storage layer as a **root-pointer swap**:

1. The last committed state is reachable from a **root pointer** in the meta page. Readers
   follow the *current* root and see a stable, consistent snapshot. They never block on an
   in-flight writer.
2. A writer accumulates all changes in a **private in-memory staging area** (the pending
   write set), leaving committed pages untouched. New/modified pages are written to
   *unused* page slots — committed pages are never overwritten in place while readers may
   be reading them (copy-on-write discipline).
3. **Commit** = `sync()` the new pages durably, then atomically publish the new root
   pointer in the meta page (a single small write), then `sync()` again. The root swap is
   the only globally-exclusive moment — the short commit lock of §3.
4. After commit, the pages the old root referenced but the new one does not become free for
   reuse. **Not MVCC** (CLAUDE.md §3): exactly one committed version plus one writer's
   pending set — no version chains, no per-row timestamps, no vacuum. Page reclamation is
   free-list bookkeeping, not version GC.

This is the bbolt model (single writer, copy-on-write pages, meta-page root swap), kept as
a reference checkout for exactly this reason (CLAUDE.md §12). The atomicity of step 3
depends on the meta-page write being all-or-nothing; the format reserves **two meta slots**
(with a checksum) so a torn write during publish can always fall back to the previous valid
meta — detail specified in [../fileformat/format.md](../fileformat/format.md).

> **Status (P6.1, `format_version` 2): incremental copy-on-write has landed.** A commit now
> writes only the **dirty** pages a mutation introduced — the path the copy-on-write B-tree
> copied (root→leaf), plus the rewritten catalog chain — to fresh **appended** slots, then
> publishes the new root by writing the **alternate meta slot** (`txid & 1`) and `sync`ing.
> The two meta slots, the checksum, the root pointer, and the **write-ordering rule** (body
> pages + `sync()`, *then* meta + `sync()`) carried forward from step-5b unchanged; P6.1
> activated the **slot alternation** the whole-image writer had stubbed (both slots = same
> `txid`). Each table's rows are now a per-table **on-disk B-tree** (interior + leaf node
> pages) whose node layout and **size-driven split/merge** are a §8 byte contract
> ([../fileformat/format.md](../fileformat/format.md)). The block seam (§2) is real: a commit
> writes individual pages in place / appends, rather than rewriting the whole file.
>
> **Free-list reclamation (P6.2) has since landed** (reconstruct-on-open): the commit allocator
> reuses dead pages from a free-list rebuilt on open, so the file no longer grows without bound
> (§6). **Still deferred** (later Phase-6 items, none foreclosed): continuous within-session
> reclamation + on-disk free-list persistence (P6.2 follow-ons), demand paging / a bounded
> buffer pool, overflow pages for over-large values (P6.1 caps a single row at `C/2` → `0A000`),
> and compression. The whole-image `to_image` survives as the **from-scratch** serializer used by
> `create`'s initial write and the golden fixtures (the special case where every node is
> dirty); the live commit path is the incremental one.
>
> The **host API** ([api.md](api.md)): the durability recipe is now a **dirty-page write +
> alternate-meta-slot publish + `fsync`** (replacing the step-5b temp-file + atomic-`rename`
> whole-file swap, which only worked because the whole file was rewritten). Atomicity comes
> from the meta-slot alternation + the checksum + the write ordering, exactly as this section
> describes. `commit` is **explicit** and `close` does not auto-flush (api.md §2).

**The root swap, in two backings (transactions.md).** The §3 staging buffer + atomic publish
this section describes is realized in [transactions.md](transactions.md): the writer's
pending set is an **immutable in-memory `Snapshot`** (a working root built from the committed
root via a persistent, structurally-shared ordered map), and the "root-pointer swap" of step
3 is **the in-memory `Snapshot` swap in Phase 5** and **the meta-page root pointer in Phase
6** — the same atomic publish, two backings. P6.1 realized the Phase-6 backing: the in-memory
store was already a copy-on-write B-tree (transactions.md §3), so "B-tree interior pages" and
"incremental COW commit" collapsed into **one slice — page-backing the tree that already
exists**. Each in-memory node carries an on-disk page id; copy-on-write leaves the new path
nodes without one (dirty); commit writes exactly those, assigns them appended pages, rewrites
the small catalog chain, and publishes the alternate meta slot. The transaction API is
unchanged (frozen) across the switch — only the materialization moved from whole-image to
dirty-page-only.

## 5. Pluggability (keep the door open — CLAUDE.md §9)

SQL is the primary access path and everything must be reachable through it (CLAUDE.md §1),
but the seam is deliberately positioned so it is **not** the only possible one:

- **Physical layout behind the relational layer.** The relational layer addresses tables
  and rows; *how* a table is laid out on pages (row-oriented now; column-oriented or
  key-value possible later, per CLAUDE.md §9) is below an internal table-access interface.
  Row-oriented is the only layout built now; the interface is shaped so an alternative
  layout is an added implementation, not a rerewrite. **Undecided whether either ships.**
- **Low-level direct access.** The block seam (§2) and the key encoding ([encoding.md](
  encoding.md)) together already make a sub-SQL access path *possible* (e.g.
  `value = getValue("tableName", key)` — CLAUDE.md §9). Not built now; the requirement is
  only that the architecture not foreclose it. Concretely: the key encoding and row format
  are specified independently of the SQL layer, so a direct reader could reuse them.

These are explicitly **not** commitments to build — they are constraints on where the seam
sits so the options stay open (CLAUDE.md §9).

## 6. Open / deferred

- **On-disk byte format** — ✅ **authored** in [../fileformat/format.md](../fileformat/format.md):
  magic/version, double-buffered meta with a checksum, the relocatable catalog chain, the
  **per-table page-backed B-tree** (interior + leaf nodes), the size-driven split/merge byte
  contract, record layout, value codec, byte-exact fixtures, and the cross-core golden
  round-trip (CLAUDE.md §8). This doc fixes the *model*; that fixes the *bytes*. **`format_version`
  2** (page-backed; the step-5b whole-image v1 is a clean break, not read).
- **Incremental commit (COW path, B-tree interior pages)** — ✅ **landed (P6.1).** A commit
  writes only the dirty path of the copy-on-write B-tree + the rewritten catalog to fresh
  appended pages, then publishes the alternate meta slot (§4 status note). The no-PK rowid is
  a **monotonic counter**, reconstructed on load as `max key + 1`.
- **Free-list / page reclamation** — ✅ **landed (P6.2), reconstruct-on-open form.** P6.1
  *leaked* every page an old root dropped (the file grew on every commit); P6.2 reconstructs a
  free-list — `[2, page_count)` minus the pages reachable from the committed root — **on open**,
  and the commit allocator (§4) reuses them (lowest index first) before extending the file. A
  page leaves the list only by being allocated into the new committed version, so it is never
  reachable from a fallback snapshot and reuse is torn-write-safe; the oldest-live-snapshot
  watermark (transactions.md §8 — a page freed at `txid T` is reusable only once
  `oldest_live_txid > T`) holds trivially on a single file-backed handle
  (`oldest_live_txid == committed.txid`). The free-list is **not persisted** (reserved meta
  offset 28 stays `0`); orphans created *within* a session are reclaimed at the next open.
  **Deferred follow-ons:** continuous within-session reclamation (return orphans immediately —
  the watermark gate then does real work, paired with file-backed reader sharing) and on-disk
  free-list persistence (so open skips the reachable-set walk —
  [../fileformat/format.md](../fileformat/format.md) *Reclamation*).
- **Buffer pool / demand paging** — ⏳ **design landed** ([pager.md](pager.md)), implementation
  in slices. Makes the resident set a **bounded cache of pages** with eviction instead of the
  whole file (CLAUDE.md §9), so a database far larger than RAM is served by paging the working
  set in on demand through the block seam (§2). Decision: a **universal** buffer pool (every
  read paged, no full-residency fast path — pager.md §1), reached **seam-foundation-first**
  (P6.4a routes the load/commit through the pager with no residency change; P6.4b makes `pmap`
  nodes lazy + bounds the resident set; P6.4c adds the memory-budget API + hardening). The
  `page_read` cost unit (P6.3) is already a **logical** count, so the cache stays invisible to
  the deterministic cost (pager.md §5, cost.md §3). Today's whole-image load is the
  deliberately-narrowed current form this replaces (§1).
- **Within-page structure** — variable-length records packed contiguously into a B-tree node
  page (a record stores its key + each column's value); an interior node prefixes its records
  with `N+1` child pointers. Slotted-page layout (intra-page free space, in-place updates) is
  a later refinement; P6.1 rewrites a whole node page when it changes.
- **Crash-recovery story** — the meta double-buffer (§4) gives atomic commit; whether a
  separate WAL is ever added is deferred (the copy-on-write + root-swap model does not
  require one for atomicity).
- **Alternative physical layouts & direct-access API** — kept open (§5), not scheduled.
- **Encryption at rest (file-level)** — kept open, not built (CLAUDE.md §9). The block seam
  (§2) is the natural insertion point: an encrypting host — or a thin layer just above it —
  can encrypt page bodies, with the meta/header carrying whatever non-secret parameters are
  needed. The crypto comes from a **vetted library, never hand-rolled** (CLAUDE.md §14). The
  present requirement is only that the format and seam not foreclose it — concretely, don't
  bake in assumptions that page bytes are plaintext-comparable on disk.
- **Overflow pages + compression of large values** — kept open, not built (CLAUDE.md §9);
  **designed in [large-values.md](large-values.md)** (the TOAST-equivalent subsystem). A value
  that pushes a record over `RECORD_MAX` currently trips the `0A000` oversized-item narrowing
  ([types.md §11](types.md), [../fileformat/format.md](../fileformat/format.md)); the design
  lifts it by pushing large `text`/`bytea`/`json` values **out-of-line onto an overflow-page
  chain**, optionally **compressed** first (likely **LZ4**). Build order is **overflow first,
  compression second**, behind one `format_version` 3 design. The compressor is **hand-rolled
  per core** (a library cannot satisfy the cross-core byte-identity of §8 — large-values.md §6),
  so the feature needs **no** third-party dependency; any later proposal is gated on CLAUDE.md §14.
- **Cost unit for storage reads** — ✅ the store is now a page-backed B-tree, so the
  cost-accounting seam (CLAUDE.md §13, [cost.md](cost.md)) meters a scan with **two** coexisting
  units: `storage_row_read` (one row read from a store during a scan) **and** `page_read` (one
  B-tree node/page touched), added in **P6.3**. A full table scan walks the whole tree, so it
  charges `page_read` once per node (the tree's structural node count) — an empty table charges
  none. `storage_row_read` was **not** renamed (a row read and a page read are distinct events).
  `page_read` counts a **logical** page access (the structural node count), not a physical disk
  fetch, so a future buffer pool / cache (§1, larger-than-RAM) stays invisible to the
  deterministic cost. Accrual rules: [cost.md](cost.md) §3 "`page_read`".
- **Bounded scan / point lookup** — ✅ the store exposes an order-preserving **bounded range scan**
  (`range_entries(lo, hi)` + a matching `overlap_node_count(lo, hi)` for the cost, mirrored across
  cores). A single-table WHERE on the primary key (`pk = c`, ranges, `BETWEEN`, AND-combinations)
  pushes down to a B-tree **seek/range** instead of a full scan: a bounded in-order traversal prunes
  any subtree whose separator span cannot overlap the key range, so it faults **only** the leaves the
  range spans, and `overlap_node_count` counts exactly the visited nodes from the resident interior
  skeleton **without faulting a leaf** (keeping `page_read` a logical count). The unbounded range
  reproduces the full scan exactly (`overlap_node_count == node_count`), so existing costs are
  unchanged. The same seek/range also bounds a **correlated subquery's inner re-scan** when its inner
  PK is compared to an enclosing column (`inner.pk = o.col`) — the bound's source is the current outer
  row's value, resolved per outer row, so the inner seeks instead of re-scanning the whole table each
  time (`query.correlated_pushdown`). In a **JOIN**, each base table is likewise bounded by the WHERE
  predicates on its own primary key against a constant (`query.join_pushdown`), so a filtered join
  materializes a seek/range per table instead of a full scan. A cross-relation `b.pk = a.x`
  (index-nested-loop) and `IN (list)` are not bounded yet (a follow-on). Accrual rules:
  [cost.md](cost.md) §3 "Bounded scan / point lookup" + "/ JOIN" + "/ correlated".
