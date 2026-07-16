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
  Most of the Phase-6 path has **landed**: incremental COW commit (dirty pages only; large
  write sets stage to disk pages, not all RAM — §4), B-tree interior pages (replacing the
  step-5b flat record chain — §6), and demand paging / a bounded **buffer pool** (resident
  set = a cache of pages with eviction — [pager.md](pager.md), §6). Still deferred (none
  foreclosed): the **spill-to-disk** blocking operators beyond sort (hash join / aggregate /
  DISTINCT under a memory budget — the `ORDER BY` external merge sort has landed,
  [spill.md](spill.md); the others are follow-ons). **Binding rule for present work:** no code
  above the storage seam may assume full residency — no "load = whole file into one buffer," no
  operator that requires its whole input/output in RAM. The whole-image serializer now survives
  only as `create`'s from-scratch write and the golden generator (§4), not the commit path.
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

> **The formal interface is [hosts.md](hosts.md).** Those are the *pager's* block-level
> operations; the per-host **byte backing** beneath them — the five-method `BlockStore`
> (`read_at`/`write_at`/`sync`/`size`/`set_size`), the per-core idiomatic mapping, the host
> catalog (in-memory / file / OPFS), and the decoration layering (where the encryption codec
> and the replication tee sit) — is specified there. This section fixes the *model*; hosts.md
> fixes the *interface every host conforms to*.

Hosts:

- **Go core** — `os.File` with `ReadAt`/`WriteAt`/`Sync` (pure Go, no cgo — CLAUDE.md §2).
- **Rust core** — direct file access (`pread`/`pwrite`/`fsync`-equivalent).
- **TS core** — a Node host (direct `fs`, `fileblockstore.ts`) **and** the browser host (OPFS,
  `FileSystemSyncAccessHandle`, `opfsblockstore.ts`) both exist (hosts.md §5). The browser host
  landed as an *added seam, not a reshape* — exactly as the host-agnostic design intended — with the
  engine running in a Web Worker behind an async client.
- **In-memory** — a `Vec`/slice of pages; the natural fit for the in-RAM target and the
  default for tests (no filesystem, fully deterministic).

The core logic above this seam is **identical across implementations** and storage-host
agnostic. Only the few methods above are per-host. Keep the interface this small: every
method is something every host (including OPFS and pure memory) can implement cheaply.

## 3. Page model

- **Fixed-size pages**, page-aligned (SSD target, §1). **Default 8 KiB**, recorded in the
  file header so it is a format parameter, not a hardcoded constant — matches PostgreSQL's
  default block size (a well-proven choice on SSD/OS page granularity) and stays revisitable
  per the fixtures without code assumptions. The size must be a **power of two** in
  `[256, 65536]` (nine legal values; [../fileformat/format.md](../fileformat/format.md) *Page
  model*) — power-of-two keeps every page boundary sector-aligned (no read-modify-write
  amplification) and matches SQLite/PostgreSQL.
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
> **Free-list reclamation has since landed** — P6.2 reconstruct-on-open, then **v25 persisted +
> continuous within-session** (§6): the commit allocator reuses dead pages from a free-list that
> is now **read from the persisted `page_type 7` chain on open** (no reachability walk) and
> **reclaimed in-commit** each commit, so the file stays bounded within a session too. **Open now
> reads only the interior spine** — the two reasons it used to touch every leaf are both gone (v25
> dropped the free-list reachability walk; v28 persists the exact per-table row count in the
> checksum-protected catalog, replacing the former eager leaf sum), making open
> O(interior spine) rather than O(file). Demand paging / the bounded buffer pool (P6.4,
> [pager.md](pager.md)), overflow pages for over-large values, and LZ4 compression (both v3,
> [large-values.md](large-values.md)) have **also landed**. **Still deferred** (later Phase-6 items,
> none foreclosed): the spill-to-disk hash join / aggregate / DISTINCT operators
> ([spill.md](spill.md)). The
> whole-image `to_image` survives as the **from-scratch** serializer used by
> `create`'s initial write and the golden fixtures (the special case where every node is
> dirty); the live commit path is the incremental one.
>
> The **host API** ([api.md](api.md)): the durability recipe is now a **dirty-page write +
> alternate-meta-slot publish + `fsync`** (replacing the step-5b temp-file + atomic-`rename`
> whole-file swap, which only worked because the whole file was rewritten). Atomicity comes
> from the meta-slot alternation + the checksum + the write ordering, exactly as this section
> describes. `commit` is **explicit** and `close` does not auto-flush (api.md §2).
>
> **Durable-commit preallocation ([pager.md](pager.md) §7).** That per-commit `fsync` was made
> ~3× cheaper without changing the model: the block seam **preallocates file growth geometrically**
> (≈doubling, floored at 16 KiB and capped at a 1 MiB chunk — real, durably-allocated zero blocks
> ahead of the committed `page_count`, SSD-target page-aligned writes, CLAUDE.md §9) and the body+meta
> barrier uses **`fdatasync`**, so a steady-state commit overwrites already-allocated space and pays no
> ext4 file-growth metadata journaling. A small database's file stays proportional to its data (no
> fixed 1 MiB minimum); a large one still grows in 1 MiB chunks. Byte- and cost-neutral (the slack is
> unreferenced trailing zeros the loader ignores; `create`'s from-scratch image is **not**
> preallocated, so the goldens stay byte-exact).

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
- **Free-list / page reclamation** — ✅ **landed (P6.2 reconstruct-on-open, then v25
  persisted + continuous).** P6.1 *leaked* every page an old root dropped (the file grew on
  every commit); P6.2 reconstructed a free-list — `[2, page_count)` minus the pages reachable
  from the committed root — **on open**, and the commit allocator (§4) reuses them (lowest
  index first) before extending the file. **v25 persists the free-list** (meta offset 28 →
  a `page_type 7` chain — [../fileformat/format.md](../fileformat/format.md) *Free-list page*),
  so **open reads it directly** rather than reconstructing it by walking every leaf (the second
  full leaf pass for spillable overflow chains is gone). Persistence is paired with **continuous within-session
  reclamation** for the file/in-memory main domain: a file commit reclaims **this commit's fresh
  orphans in-commit** — periodically, once the high-water passes ~2× the live count, so the file
  oscillates in `[live, 2×live]` and the O(live) reachability walk is amortized O(height)/commit
  — because with open no longer reconstructing, orphans left unpersisted would leak permanently
  (a short open→commit→close session especially). This is what makes the **oldest-live-snapshot
  watermark** (transactions.md §8 — a page freed at `txid T` is reusable only once
  `oldest_live_txid > T`) **load-bearing**: the in-commit rebuild is deferred while any reader
  pins an older version. A free-list page is drawn from the free-list itself (never the
  high-water), so persisting the free-list does not grow the file, and it is torn-write-safe (a
  reused/rewritten page is dead at the fallback snapshot). An in-memory database reclaims the
  same way with **no persistence** (no meta, never reopened — post-commit RAM rebuild).
- **Open reads only the interior spine** — ✅ **landed; exact counts persisted in v28.** After v25
  removed the reachability walk, the *last* reason open touched every leaf was summing the per-table
  row count from each leaf header. `read_skeleton` builds the demand-paged interior skeleton
  **without reading the leaf level** — it exploits the B+tree
  same-depth invariant (an interior's children are homogeneous), resolving only the **first** child of
  each interior to classify the level, then referencing leaf siblings as `OnDisk` without reading
  them. v28 appends the table's exact nonnegative `i64` count to its catalog entry and installs it
  with that skeleton; `(root_data_page == 0) == (row_count == 0)` is checked without a leaf walk.
  Working-snapshot insert/remove maintains the count with the root, so rollback restores both.
  Open remains **O(interior
  spine)** — catalog + interior pages + ~one leaf per bottom-level interior (the classify peek) + the
  meta/free-list pages — not O(file). The lone exception is a **no-PK** table, whose synthetic-rowid
  reconstruction still faults its leaves to find `max key + 1` (most tables have a PK; bounded by the
  pool).
- **Multi-process file locking** — ✅ **landed ([locking.md](locking.md)).** Protocol-aware handles and
  file attachments share the same file safely through a stable `<path>.lock/` bundle carrying
  presence/arrival/transition/writer/commit OS locks. An
  uncontended presence-EX lease preserves the current foreground path and v29 allocator. While
  co-resident, begins refresh the newest meta, one global writer commits append-only with
  `free_list_head = 0`, and body pages stay immutable. Free-page reuse, free-list persistence/rebuild,
  truncation, and compaction require presence-EX proof of aloneness plus the existing in-process
  watermark. The bundle, rather than the replaceable database inode, remains locked across future
  `to_image` compaction (locking.md §3–§6).
- **File compaction / shrink (returning space to the OS)** — ⏳ **approach decided, not built.**
  The free-list (above) recycles dead space for *jed*, but it never gives it back: `page_count` is
  a monotonic high-water (plus the pager.md §7 preallocation slack), so the file is **grow-only** —
  insert-a-lot-then-delete leaves it permanently at its peak size. (This is the SQLite/PostgreSQL
  default too — both reuse freed space and shrink only under an explicit `VACUUM`.) **The decided
  shrink mechanism is `to_image`-based whole-image compaction:** re-serialize the committed
  snapshot through the existing from-scratch `to_image` serializer — the **garbage-free packed
  image** `create` already writes ([../fileformat/format.md](../fileformat/format.md) *From-scratch
  image*) — into a fresh file, atomically swap it in (the `create` temp-file + `fsync` + atomic
  `rename` + dir `fsync` recipe, [api.md](api.md) §3), and re-adopt the pager on the new, minimal
  file. One pass reclaims **all** dead space and **defragments** (the SQLite `VACUUM` / PostgreSQL
  `VACUUM FULL` flavor), and it is **crash-safe for free** — the atomic rename is all-or-nothing,
  so a crash leaves the prior file intact (the property `create` and the step-5b whole-image era
  relied on). It is **host-invoked / explicit, not automatic-per-commit** — a per-commit
  truncation would fight the §9/pager.md §7 preallocation (truncate → regrow → re-`fsync` churn) —
  and, being a writer operation that replaces the file under any demand-paging readers, it is gated
  on the reader-liveness watermark (transactions.md §8) like any reclamation. It **needs nothing
  new at the seam** (§2, [hosts.md](hosts.md)): the compact image is written through the block
  device and simply ends smaller — the file shrinks because the fresh image is smaller, not by an
  in-place truncate. A lighter **in-place trailing-free truncation** — lower `page_count` and
  `set_size` down when the top pages `[k, page_count)` are all free (the PG-plain-`VACUUM` /
  SQLite-`incremental_vacuum` flavor) — stays open as a cheaper *partial* complement: no rewrite,
  but it reclaims only *trailing* free space and must be sequenced against the two-meta-slot
  fallback (§4/§7) and the watermark. Tracked in [../../TODO.md](../../TODO.md) Phase 6.
- **Buffer pool / demand paging** — ✅ **landed (P6.4)** ([pager.md](pager.md)). The resident
  set is a **bounded cache of pages** with eviction instead of the whole file (CLAUDE.md §9), so
  a database far larger than RAM is served by paging the working set in on demand through the
  block seam (§2). It is a **universal** buffer pool (every read paged, no full-residency fast
  path — pager.md §1), reached **seam-foundation-first** (P6.4a routed the load/commit through
  the pager with no residency change; P6.4b made `pmap` nodes lazy + bounded the resident set;
  P6.4c added the `cache_bytes` memory-budget API + hardening). The `page_read` cost unit (P6.3)
  is a **logical** count, so the cache stays invisible to the deterministic cost (pager.md §5,
  cost.md §3). The whole-image load survives only as `from_image`/`create`/the goldens (§1).
- **Within-page structure** — the tree is a **B+tree** (v24, [bplus-reshape.md](bplus-reshape.md)):
  a **leaf** page holds **all** the records, column-major (each record stores its key + each
  column's value in per-column class-shaped regions — format.md *Leaf node*); an **interior**
  page holds only `separator keys + N+1 child pointers` (record-free routing). Slotted-page
  layout (intra-page free space, in-place updates) is a later refinement; P6.1 rewrites a whole
  node page when it changes.
- **Crash-recovery story** — the meta double-buffer (§4) gives atomic commit; **no WAL is
  needed** — the copy-on-write + root-swap model gives both atomicity *and* reader/writer
  concurrency (transactions.md §10) for free, which are the two reasons an embedded engine
  usually grows a WAL ([replication.md](replication.md) §1). A separate WAL stays deferred and,
  on current analysis, unmotivated even for replication (block-shipping the commit delta
  suffices — replication.md). The atomicity is **verified at the actual commit points** by the
  **fault-injection seam** (§7): a per-core, test-only one-shot crash/tear armed on the pager,
  with a cross-core recovery matrix asserting that a crash anywhere recovers to a valid snapshot.
- **At-rest corruption detection** — distinct from crash recovery (which protects the *commit*
  boundary), this protects a *quiescent* page against bit-rot. Through `format_version` 6 only
  the two meta slots were checksummed; a flipped bit in any catalog page, B-tree node, or
  overflow page went undetected and silently produced wrong rows or a panic. **`format_version`
  7 adds a per-page CRC-32/IEEE to every body page** (the page header grows 12→16 bytes —
  [../fileformat/format.md](../fileformat/format.md) *Page header*), verified the instant a page
  is parsed: a mismatch is `data_corrupted` (`XX001`). Because every read funnels through that
  parse, corruption is caught the instant the page is read: **at open for a catalog or interior
  (routing-spine) page** — the pages the demand-paged loader reads — and **at fault for a leaf or
  overflow page**, which open no longer reads (open reads only the interior spine — the free-list
  reachability walk is gone and v28 loads row counts from the catalog). Either way it is caught **or inert**
  (a corrupted *dead* page is never read), never served as wrong rows; a full scan validates every
  live page. It is **not** end-to-end integrity (a malicious rewriter can recompute the CRC; that is
  the encryption-at-rest / authenticated-page door below, not this), and it is **not** metered
  (physical I/O, invisible to `page_read` cost). A dedicated per-core corruption test
  complements the fault-injection matrix.
- **Alternative physical layouts & direct-access API** — kept open (§5), not scheduled.
- **Encryption at rest (file-level)** — kept open, not built (CLAUDE.md §9); **designed in
  [encryption.md](encryption.md)**. The insertion point is a **page codec in the core, just
  above the block seam** (a thin layer, not a per-host duty — hosts.md §6), encrypting page
  bodies with a standardized AEAD under a **deterministic `(page_index, txid)` nonce** that
  keeps the §8 cross-core byte-identity, while the auth tag *closes* the tamper gap the
  `format_version` 7 CRC explicitly leaves open (above). The crypto comes from a **vetted
  library, never hand-rolled** (CLAUDE.md §14, the build gate). The present requirement is only
  that the format and seam not foreclose it — concretely, don't bake in assumptions that page
  bytes are plaintext-comparable on disk (already satisfied — hosts keep page bytes opaque).
- **Replication** — kept open, not built; **designed in [replication.md](replication.md)**.
  Decided: **block-shipping** the per-commit page-delta (the dirty pages + meta swap §4 already
  produces), in `txid` order, **not a WAL**. Inherits the §8 byte-identity (a delta applies
  byte-identically on any core/host) and the §4 atomic-apply recipe; the tee sits **below** the
  encryption codec so a replica can be **keyless** (hosts.md §6). A *logical* changeset stream
  (compact wire / heterogeneous consumers) is a separate higher-layer door, not foreclosed.
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

## 7. Fault injection & crash-recovery testing

The crash-safety claims of §4 — **body pages + `sync()`, *then* the alternate meta slot + `sync()`**,
backed by the **two checksummed meta slots** ([../fileformat/format.md](../fileformat/format.md)) — are
the load-bearing reliability property: a crash at *any* point in a commit must leave the file readable
as a **valid snapshot**, never corrupt. A golden fixture with a hand-corrupted checksum
(`torn_meta_slot{0,1}.jed`) tests the *post-hoc* fallback, but it cannot exercise the **actual commit
points** — mid-body, between the body and meta syncs, mid-meta-write. The **fault-injection seam** does.

**The seam.** The pager (§2) carries an optional **one-shot fault**, armed only by tests, that simulates
a crash at a chosen point in the commit write sequence (`persist`):

- **`BodyWrite(n)`** — fail on the *n*-th write to a **body** page (a clean crash mid-body, before the
  body `sync()`).
- **`MetaWrite`** — let the body write + `sync()` complete, then fail on the **meta-slot** write (a crash
  *between* the body sync and the meta sync — the critical window §4 protects).
- **`Sync(n)`** — fail on the *n*-th `sync()` since arming (`1` = body barrier, `2` = meta barrier).

Pages **0 and 1 are always the meta slots** and every body/catalog page is **≥ 2**
([../fileformat/format.md](../fileformat/format.md)), so `MetaWrite` is identified by the page **index**
(`< 2`), never by counting body pages — stable regardless of how many pages a commit dirties. A write
fault may additionally **tear** the page: write `k` leading bytes before failing, simulating a partial
(torn) page write — the case the meta checksum exists to catch.

**Not a §8 byte contract.** Like the buffer pool (pager.md §3) and the `fdatasync` flavor (pager.md §7),
the seam is **per-core internal machinery, realized idiomatically** — the Rust core gates it behind
`#[cfg(test)]` (zero production footprint); Go/TS carry an inert `None`/`nil` field checked on the write
path. What is a **cross-core contract is the recovery *outcome***, asserted identically in all three
cores' per-core tests (not the corpus — a crash mid-commit is not SQL-level deterministic, like P5.3
concurrency and `$N`).

**The recovery matrix (the invariant: recover to a valid snapshot — prior *or* new — never corruption).**

| Injected crash point | Durable result | Reopen yields |
|---|---|---|
| `BodyWrite(1)` (mid-body, unsynced) | new body pages partial/unreferenced; prior meta intact | **prior** snapshot, fully readable |
| `BodyWrite(1)` torn (partial page) | a torn body page, unreferenced by the prior meta | **prior** snapshot (the torn page is never read) |
| `Sync(1)` (before body durable) | body written-through but unsynced; meta not written | **prior** snapshot |
| `MetaWrite` (body durable, meta not written) | new body durable but unreferenced; prior meta intact | **prior** snapshot |
| `MetaWrite` torn (partial meta page) | the published slot's checksum fails | **prior** snapshot (checksum → fall back to the other slot) |
| `Sync(2)` (meta written, unsynced) | the new meta is written-through | a **valid** snapshot (atomicity holds either way; see below) |
| no fault (baseline) | full commit | **new** snapshot |

After any recovery-to-prior, a follow-on test **continues committing** (insert/delete churn) to confirm
the **free-list** (§6 — v25 persisted + reclaimed in-commit) is correct after a crash — reuse stays
torn-write-safe and the file does not corrupt or grow unbounded.

**Write-through fidelity caveat.** The seam writes through to the real file (it does not model "unsynced
bytes are lost on power loss"). For every point *before* the meta is written this is exactly faithful —
the prior meta references only prior pages, so unreferenced new bytes are inert. At the **`Sync(2)`**
boundary (meta written, not yet synced) a real power loss could lose the meta (→ prior) or keep it
(→ new); write-through deterministically yields **new**. Both are *valid* — that boundary tests
**atomicity** (never a half-published state), and the loss-direction is already covered by the
`MetaWrite` / `Sync(1)` rows, so the matrix is complete without modeling unsynced-data loss.
