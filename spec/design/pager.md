# Pager & buffer pool — design

> The reasoning behind demand paging: how the engine serves a database whose data far
> exceeds RAM without falling over (CLAUDE.md §9), by making the resident set a **bounded
> cache of pages** instead of the whole file. This is a *design* doc — the byte-exact
> on-disk format is [../fileformat/format.md](../fileformat/format.md); the block seam is
> [storage.md](storage.md) §2; the cost contract is [cost.md](cost.md). When a decision
> here changes, update [CLAUDE.md](../../CLAUDE.md) §9 and [storage.md](storage.md) §6 in
> the same edit.

This is the **Phase 6 "Buffer pool / demand paging"** item (TODO.md). It depends on the
page-backed B-tree + incremental commit (P6.1) and the free-list (P6.2), both landed, and
on the *logical* `page_read` cost unit (P6.3), which was designed precisely so a cache
cannot perturb the deterministic cost (§ "Cost" below).

## 1. What we changed, and why

> **Status: landed (P6.4, all three cores — see §6).** This section is the motivation; the
> realized form is in §6.

**Before this change (the full-residency form, CLAUDE.md §9 / storage.md §1).** `open` read the
**whole file** into one buffer and `from_image` rebuilt **every** B-tree node of every table
into resident memory (`read_tree` → an in-memory node per on-disk page). The entire dataset was
resident; reads then chased resident pointers. This was correct and fast for the **dominant
RAM-sized case**, but the residency itself is the wall a larger-than-RAM file hits.

**The change.** The resident set becomes a **bounded buffer pool**: a fixed-budget cache of
decoded pages with eviction. A node not in the pool is loaded **on demand** from the open file
through the block seam (storage.md §2) and cached; under memory pressure the pool **evicts** a
clean, unpinned page. A file far larger than RAM is served by paging the working set in and
out, never materializing the whole image.

**Decision — universal buffer pool, not a hybrid.** Every committed-tree read goes through the
pager + pool, for **every** database, not only ones above a size threshold. One uniform path:
no "fully resident under a threshold, paged above it" fork to maintain, no two code paths to
keep in cost/result agreement. The dominant RAM-sized case simply ends up with its whole working
set cache-resident (the pool is large enough), so it pays only a thin indirection — while a
larger-than-RAM file is handled by the *same* code, just with more misses. (The alternative —
keep today's full residency as a fast path and engage the pool only above a budget — was
weighed and rejected: two residency paths is exactly the kind of accidental divergence surface
§2 of CLAUDE.md exists to avoid, and the indirection cost on small DBs is negligible.)

This honors CLAUDE.md §9 read precisely: *RAM-sized is the dominant case, larger-than-RAM must
not be foreclosed.* A universal pool sized to comfortably hold a RAM-sized working set **is**
full residency for the common case, and is the *same* machinery that bounds a huge one.

**Two scope refinements of "universal" (made when planning the implementation; §6).** The pool
bounds what is *expensive* to hold and trivial to refault, and leaves resident what is cheap to
keep and costly to recompute:

- **In-memory databases stay fully resident.** A database with no backing file (the pure
  in-memory mode, and the conformance harness's default) — SUPERSEDED (bplus-reshape.md B3): it
  now pages from its `MemoryBlockStore` through this same pool, pinned/unbounded (an in-memory
  database is resident by definition; `cache_bytes` bounds only file-backed eviction). The
  historical carve-out — it had nowhere to page *from*, so it kept
  its tree resident — the pool is the read path for **file-backed** databases. This is not the
  rejected "fast path for small files"; it is the degenerate no-backing case (a query against an
  in-memory database touches RAM either way). The dominant durable-disk mode is fully paged.
- **The interior B-tree skeleton stays resident; only leaf pages are demand-paged** (P6.4b's
  first form). Interior nodes are roughly `1/fan-out` of all pages (well under 1% at the default
  page size), and keeping them resident is what lets `node_count` — and therefore cost (§5) — be
  computed **without loading any leaf** (an interior node already lists its leaf children's page
  ids). The leaves are the bulk and the part that must page to bound a larger-than-RAM file.
  Paging the interior skeleton too (for a file whose *interior* alone exceeds RAM — a multi-TB
  extreme) is a deferred follow-on (§6), not foreclosed.

## 2. The pager (the block device)

The pager realizes the storage seam (storage.md §2) for **reads**, which today's whole-image
load bypasses. It owns the open backing for the life of a `Database` handle and serves single
pages by index:

```
read_block(index)        -> bytes      # one page, random access (pread / ReadAt / fs read-at-offset)
write_block(index, bytes)              # one page (already used by the incremental commit, P6.1)
allocate_block()         -> index      # grow by one page (high-water / free-list, P6.2)
sync()                                 # durability barrier (fsync), at commit
block_count()            -> count
```

Backings (one `Pager` trait/interface, per-host impls — storage.md §2):

- **File** — the open file kept for the handle's lifetime (today `open` reads all bytes then
  drops the file; the pager keeps it open and `read_block`s on demand; `close` closes it).
  Rust `pread`/`File::seek+read`, Go `os.File.ReadAt`, TS Node `fs.readSync` at an offset; the
  browser/OPFS host (`FileSystemSyncAccessHandle`) slotted in here as another `BlockStore`, unchanged
  above the seam (storage.md §2, hosts.md §5).
- **In-memory** — a `Vec`/slice of page buffers; the default for tests and the pure-in-memory
  database mode. The pool sits above it too (a trivial, never-evicting backing), so the
  in-memory path exercises the same code.

The pager is **below** the relational core and storage-host agnostic; only the few methods
above are per-host (storage.md §2).

## 3. The buffer pool

A fixed-capacity cache mapping `page_id → decoded page`, with:

- **A memory budget** — a configurable bound on resident leaf memory (the resident-set bound),
  stated in **bytes** at the handle (`open`'s `cache_bytes`, [api.md](api.md) §2.1) and converted
  to a leaf-page capacity by the file's page size: `cache_leaves = max(1, cache_bytes / page_size)`.
  Bytes, not a page count, so the caller's budget does not silently scale with the file's `page_size`
  (a page count would mean a 256× different footprint across page sizes); the `max(1, …)` floor keeps
  one leaf resident even when `cache_bytes < page_size`. Default sized so a RAM-sized working set stays
  fully cache-resident (§1) — `DEFAULT_CACHE_BYTES = 256 MiB`. (Originally 8 MiB — the historical
  1024-leaf count at the 8192 page size — which contradicted this sizing rule: a typical RAM-sized
  database thrashed the pool under the default, paying a fault + leaf decode on most point lookups.
  256 MiB keeps the dominant RAM-sized case fully resident; a host that wants the old bound passes
  `cache_bytes` explicitly.) The budget is a *handle* setting, not an
  on-disk parameter. A resident clean leaf is the retained page block plus shared PAX directories:
  rows decode by touched column, keys are borrowed spans of the key blob, and record weights derive
  lazily. It owns no per-record key, row, or weight objects, so resident leaf memory is
  `≈ cache_leaves × page_size` and the byte budget means what it says. This Packed representation is
  results/cost/byte-neutral above the seam ([packed-leaf.md](packed-leaf.md)).
- **Eviction — CLOCK (second-chance).** A simple per-core CLOCK over the resident pages: a
  reference bit set on access, a hand that sweeps and evicts the first unreferenced, unpinned,
  clean page. CLOCK over strict LRU because it needs no per-access list surgery and is the
  well-trodden DB choice; the policy is **not observable** (next section), so it carries no
  cross-core obligation.
- **Pinning.** A page in active use (on the current root→leaf path, or held by a live snapshot
  cursor) is **pinned** and never evicted. Pins are reference-counted; the page becomes
  evictable when the last user releases it. (In the Rust core an `Arc` to the decoded page is a
  natural pin; Go/TS use an explicit pin count.)
- **Dirty pages are never evicted.** An uncommitted (dirty) node is not on disk yet — it has no
  page to reload from — so it stays resident until the commit assigns it a page id and writes it
  (P6.1). Only **clean** pages (already durable) are eviction candidates; evicting one just drops
  a cache entry, losing nothing.

### Why the pool is NOT a §8 cross-core byte contract

This is the load-bearing simplification. The buffer pool changes **when** a page is in RAM,
never **what** a query observes. Two cores with different cache states — different eviction
timing, different resident sets — return the **identical** result multiset, types, names,
errors, **and cost** (next section). So, unlike the on-disk format or the key encoding, the
pool is **not** a byte contract: each core may implement CLOCK (or even a different policy)
independently, the way P5.3 let each core realize concurrency differently. What *is* contract —
results and cost — is held invariant by construction. This is why the universal-pool decision
costs little: there is no second observable surface to keep in lockstep.

## 4. Demand paging the B-tree (lazy nodes)

The heart of the residency change is in the persistent B-tree (`pmap`). Today
`Node.children: Vec<Arc<Node>>` is an eager, fully-resident pointer tree. It becomes a **lazy
child reference**:

```
ChildRef = Resident(node)      # an in-memory node: a dirty/uncommitted node, or one currently cached
         | OnDisk(page_id)     # a clean child not (necessarily) resident; load through the pool on access
```

Traversal (`get`, `iter`, insert/delete descent) resolves an `OnDisk(p)` by asking the buffer
pool for page `p` — a **hit** returns the cached decoded node; a **miss** `read_block`s it,
decodes it (the existing node codec, format.md), inserts it into the pool (evicting if full),
and returns it. A `Resident(node)` is used directly. So the resident set is bounded by the pool
budget, not by the tree size.

**P6.4b's first form — interior skeleton resident, leaves `OnDisk` (§1).** In the landed slice,
`open` materializes the **interior** nodes resident (`Resident`) and leaves each *leaf* child an
`OnDisk(page_id)`; a full scan or lookup faults the leaf it needs through the pool, which bounds
how many leaves are resident at once. Because eviction only drops the cache entry while any
in-flight `Arc`/reference to a node keeps it alive (a clean node is immutable, so a re-load is a
harmless duplicate), the pool needs **no pins** — the traversal holds at most a root-to-leaf path
of nodes, a bound of tree height. Fully paging the interior too (so even the skeleton is bounded)
is the deferred follow-on (§6).

**Interaction with copy-on-write snapshots (the subtle part).** The persistent map's invariants
are preserved:

- A **clean** subtree is identified by its page id and is **immutable on disk**, so any number
  of snapshots (readers) can share it; loading it into the pool is shared, idempotent, and
  safe — the cached bytes are the durable bytes.
- A **writer's working set** keeps its new (dirty) nodes `Resident` and pinned (page 0, §3); the
  untouched clean subtrees it inherits are `OnDisk` page ids, paged like any read. Commit
  assigns the dirty nodes page ids, writes them (P6.1), and they become ordinary clean,
  evictable pages.
- The **free-list / watermark** (P6.2, transactions.md §8) is unchanged and now does real work:
  a page a commit frees may not be reused while a **live reader** still pins a snapshot that
  references it (`oldest_live_txid` gates reuse) — exactly the guard a paged, file-backed
  multi-reader setup needs, which a fully-resident single image did not stress.

This slice does **not** add a point-lookup/index path; the executor still **full-scans** every
table (cost.md §3 "page_read"). Demand paging changes *where the bytes live*, not *which pages a
query touches*.

## 5. Cost — logical page accesses, cache-independent (§13)

P6.3 made `page_read` a **logical** unit on purpose: it counts pages a query *touches*, not
physical disk fetches, so a cache cannot perturb the deterministic cost (cost.md §3 "page_read",
CLAUDE.md §13). Demand paging is the reason that mattered, and it changes the **accrual site**
but not the **totals**:

- **`page_read` stays a structural block — `node_count()`, charged in the executor, unchanged
  from P6.3.** This is the dividend of keeping the **interior skeleton resident** (§1/§4): an
  interior node already lists its leaf children's page ids, so `node_count()` = (resident
  interior nodes) + (their leaf-child counts) is computable by walking only the resident skeleton
  — **without loading a single leaf**. A full scan still touches every node, so the count is the
  same total; the corpus `# cost:` values **do not move**, and cost accrual does **not** have to
  migrate into the pmap traversal (it stays in the executor, cost.md §3). Had we paged the
  interior too, `node_count` would force-load the tree and accrual would have to move to
  per-node-visit; keeping the skeleton resident is precisely what avoids that.

So cost stays **deterministic, cross-core identical, and cache-independent**: the same
`(query, db)` charges the same `page_read` total no matter the pool budget or eviction history —
the pool changes only *when* a leaf is in RAM, never the count of pages the query logically
touches. A future point-lookup path would visit fewer pages and legitimately cost less, but that
is a separate feature.

## 6. Slicing (the mergeable steps)

Sequenced **seam-foundation-first** so each step is independently testable and the risky core
change lands alone, on a frozen seam:

- **P6.4a — the pager seam (no residency change).** Introduce the `Pager` (file + in-memory
  backings, kept open) and route the whole-image load **and** the incremental commit (P6.1)
  through `read_block`/`write_block`. A buffer-pool scaffold exists but the loader still
  materializes the full tree, so behavior, results, and cost are **byte-unchanged** (the
  goldens and `# cost:` values are untouched). This de-risks the seam and the keep-file-open
  lifecycle (`close` now closes the file) before any data-structure surgery. *Mergeable, no
  observable change.*
- **P6.4b — leaf demand-paging + the bounded pool (the residency win).** `pmap` children become
  a `ChildRef` (`Resident | OnDisk(page_id)`); for a **file-backed** database the interior
  skeleton loads resident and each **leaf** is `OnDisk`, faulted through the bounded **CLOCK**
  pool (no pins — §4) on access. In-memory databases stay fully resident (§1). `node_count`/cost
  is unchanged (§5). The resident set becomes bounded by (interior skeleton + pool budget). *The
  XL heart; the slice the whole item exists for.* Built Rust-first, then Go/TS (a `# cost:`
  corpus-neutral change, so each core can land green independently — like P5.1).
- **P6.4c — budget config + hardening. ✅ landed.** The handle-level memory budget is now a public
  **open-time** setting (`open(path, opts)` with `opts.cache_bytes` — the buffer-pool budget in
  **bytes**, default `DEFAULT_CACHE_BYTES = 256 MiB`, converted to a leaf-page capacity by the file's
  page size as `max(1, cache_bytes / page_size)`; a **handle** setting, never stored in the file —
  api.md §2.1), with a read-only **`resident_leaves`** gauge (a real count for in-memory too
  since bplus-reshape.md B3). The internal
  `open_with_capacity` seam was promoted to this public API. **Bytes, not a page count**, so the
  caller's budget does not silently scale with the file's `page_size` (§3). **Page-size hardening:** the
  page size is now constrained to a **power of two in `[256, 65536]`** (`MIN_PAGE_SIZE = 256` through
  `MAX_PAGE_SIZE = 64 KiB`, nine legal values) and rejected otherwise on both paths — `0A000` on
  `create`, `XX001` on `open` — so a corrupt or hostile file cannot record a multi-gigabyte `page_size`
  and force that allocation before its content is validated (CLAUDE.md §13; format.md *Page model*). A large-file test in each core opens a database
  whose leaf pages far exceed a tiny budget and confirms it scans, mutates, and round-trips correctly
  while the resident leaf count stays `≤ cache_leaves` throughout (including under a repeated-lookup
  workload), plus a `page_size > cache_bytes` case that keeps exactly one leaf resident.

Deferred follow-ons (none foreclosed): **paging the interior skeleton too** (for a file whose
interior alone exceeds RAM — a multi-TB extreme; needs `node_count`/cost to move to per-node-visit
and the free-list to persist rather than reconstruct-on-open, P6.2's deferred item), and a Memory
backing so in-memory databases page through the identical path (currently they stay resident, §1).

Deferred and explicitly **out of this item** (separate TODO entries, none foreclosed):
**streaming + spill-to-disk operators** (sort / hash join / aggregate / DISTINCT under a memory
budget — a *query-operator* memory bound, distinct from this *storage* page cache) and a
**point-lookup / index** path (would change which pages a query touches, hence cost).

## 7. Durable-commit preallocation (the metadata-free body sync)

The `synchronous=on` commit chokepoint (transactions.md §9) is two `fsync`s — body pages, then
the alternate meta slot. Measured on ext4 (the dev/CI host, 2026-06-13), each of those was
**~4.3 ms** when the commit **grew the file**: appending pages past the high-water drags ext4's
**metadata journaling** (the inode size + extent/block-allocation change) into the flush. With the
free-list draining only on reopen (P6.2), a long write session appends fresh pages on essentially
every commit — so a single-row durable commit cost **~9 ms** (two growing-file syncs), well behind
PostgreSQL's ~1.5 ms (one metadata-free `fdatasync` into its preallocated WAL segment).

The fix has **two** load-bearing halves — a microbenchmark on the same host showed preallocation
*alone* barely helped (`fsync` still journals the inode timestamp), and `fdatasync` *alone* on a
growing file still pays the size-metadata journal; only **both together** win:

- **Preallocate file growth geometrically.** The pager tracks an `allocated_pages` high-water (the
  physical file length in pages) distinct from the committed logical `page_count`. Before a
  commit's body write, `reserve(new_page_count)` grows the file — when short — **geometrically**:
  each step adds the current size (≈doubling), **floored** at `max(1, 16 KiB / page_size)` pages and
  **capped** at `max(1, 1 MiB / page_size)` pages, of **real, durably-allocated zero blocks** (a write
  of zeros, not a sparse `ftruncate` — a hole would re-allocate on first write and re-journal). So a
  **small** database's file stays proportional to its data (bounded by ≈2× the committed high-water —
  *not* a fixed 1 MiB minimum, which for an embedded engine's many small databases would be gross
  over-allocation), while a **large** one still grows in 1 MiB chunks once past that size. Both bounds
  are denominated in **bytes** (converted to pages), so they scale with `page_size`: at a 64 KiB page
  size the floor bottoms out at a single page; the amortization economics are per-byte, not per-page.
  Each growth is made durable with **one full `fsync`**, *amortized* across the pages it reserves — the
  doubling keeps the number of allocating fsyncs logarithmic while the file is small and identical to
  the old fixed-chunk behavior once it exceeds 1 MiB. Almost every commit then writes its body entirely
  into **already-allocated** space.
- **`fdatasync`, not `fsync`, for the per-commit barrier.** An overwrite into the preallocated
  region changes no file metadata (size fixed, blocks already allocated), and `fdatasync` skips
  the inode-timestamp flush `fsync` forces — so the body and meta syncs become **metadata-free**.

Steady state is therefore **two metadata-free `fdatasync`s ≈ 2.8 ms** per commit. **Measured
result (all three cores):** the `insert_commit_durable` benchmark fell from **~9.0 ms → ~2.5–3.1 ms**
p50 (~2.7–2.9×), at PostgreSQL's order of magnitude (jed pays two syncs to PG's one).

**What this does *not* touch:**

- **The byte contract (§8 / CLAUDE.md §8).** The committed image — pages `[0, page_count)` — is
  byte-identical; the preallocated tail is **unreferenced trailing zeros past the high-water**.
  `create`'s from-scratch `to_image` write and the golden fixtures are **not** preallocated, so the
  goldens stay byte-exact and the cross-core round-trip is unchanged. The loader reads `page_count`
  and reconstructs the free-list from the **meta**, never the physical file length, so slack pages
  are never mistaken for free pages.
- **Cost (§5 / CLAUDE.md §13).** `page_read` is a **logical** count; the physical file size and the
  preallocation are invisible to it.
- **Crash safety (storage.md §4).** The preallocated zeros are referenced by no committed meta, and
  each allocating `fsync` lands *before* any commit relies on the region — so a crash at any
  point falls back to a valid prior snapshot exactly as before.
- **The commit-visibility boundary (transactions.md §9).** Only fsync *timing/flavor* changed, never
  *when* a commit becomes visible. The future `synchronous=off` batching is an orthogonal,
  still-deferred step on top of this.

**Per-core realization (not a byte contract — like the pool, §3).** `fdatasync` is the metadata-free
barrier in each core, chosen idiomatically: Rust `File::sync_data()`, TS Node `fs.fdatasyncSync`, Go
`syscall.Fdatasync` (pure Go, no cgo — CLAUDE.md §2) behind a `linux` build tag with a full-`Sync`
fallback for platforms lacking it (still correct, just without the optimization). The preallocation
floor/cap and the geometric `reserve` logic are identical across cores (pure integer arithmetic on
`page_size` and `allocated_pages`); the *physical* file size it produces is **not** a byte contract
(the tail is unreferenced zeros), so a core need only match the policy, not the exact length.

## 8. Determinism & cross-core notes

- **Results + cost are the only contract**, and both are invariant to the pool (§3, §5). The
  pager, the pool, and CLOCK are internal performance machinery, **not** a byte contract — each
  core implements them idiomatically (like P5.3's per-core concurrency), provided results and
  cost stay byte-identical.
- **No nondeterminism leaks.** The pool keys on page id (deterministic), never on hashmap
  iteration order; eviction never affects which rows/pages a query logically touches; the I/O on
  a miss is unmetered, so timing never enters cost (CLAUDE.md §8/§10).
- **Memory safety holds** — the file backing is `pread`-style random reads into owned buffers in
  every core (no `unsafe`, no cgo; CLAUDE.md §2/§13).
