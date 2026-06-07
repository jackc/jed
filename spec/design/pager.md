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

## 1. What we are changing, and why

**Today (the full-residency current form, CLAUDE.md §9 / storage.md §1).** `open` reads the
**whole file** into one buffer and `from_image` rebuilds **every** B-tree node of every table
into resident memory (`read_tree` → an in-memory node per on-disk page). The entire dataset is
resident; reads then chase resident pointers. This is correct and fast for the **dominant
RAM-sized case**, but it **forecloses nothing only because** the format is already page
structured — the residency itself is the wall a larger-than-RAM file hits.

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
  browser/OPFS host slots in here later (storage.md §2), unchanged above the seam.
- **In-memory** — a `Vec`/slice of page buffers; the default for tests and the pure-in-memory
  database mode. The pool sits above it too (a trivial, never-evicting backing), so the
  in-memory path exercises the same code.

The pager is **below** the relational core and storage-host agnostic; only the few methods
above are per-host (storage.md §2).

## 3. The buffer pool

A fixed-capacity cache mapping `page_id → decoded page`, with:

- **A memory budget** — a configurable maximum number of resident pages (the resident-set
  bound). Default sized so a RAM-sized working set stays fully cache-resident (§1). The budget
  is a *handle* setting, not an on-disk parameter.
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

- **Today (full residency):** `page_read` is charged as a structural block — `node_count()`,
  computed by walking the already-resident tree — once per table scan.
- **Under demand paging:** computing `node_count()` would mean loading the whole tree, defeating
  the purpose. So accrual moves to **per node *visited* during the traversal** — each logical
  node the scan/lookup steps onto charges one `page_read`, **whether it was a cache hit or a
  miss**. A full table scan visits every node exactly once, so the per-visit total **equals** the
  old structural `node_count` — the existing corpus `# cost:` values **do not shift again**. The
  physical pool fetch (the I/O on a miss) is **unmetered**; only the logical visit is.

So cost stays **deterministic, cross-core identical, and cache-independent**: the same
`(query, db)` charges the same `page_read` total no matter the pool budget or eviction history.
A future point-lookup path would visit fewer pages and legitimately cost less, but that is a
separate feature; the move from structural to per-visit accrual is **corpus-neutral** today
because every metered query is a full scan.

## 6. Slicing (the mergeable steps)

Sequenced **seam-foundation-first** so each step is independently testable and the risky core
change lands alone, on a frozen seam:

- **P6.4a — the pager seam (no residency change).** Introduce the `Pager` (file + in-memory
  backings, kept open) and route the whole-image load **and** the incremental commit (P6.1)
  through `read_block`/`write_block`. A buffer-pool scaffold exists but the loader still
  materializes the full tree, so behavior, results, and cost are **byte-unchanged** (the 15
  goldens and `# cost:` values are untouched). This de-risks the seam and the keep-file-open
  lifecycle (`close` now closes the file) before any data-structure surgery. *Mergeable, no
  observable change.*
- **P6.4b — lazy nodes + the bounded pool (the residency win).** `pmap` children become
  `ChildRef`; clean children load on demand through the bounded pool with CLOCK eviction;
  dirty nodes stay pinned; `page_read` accrual moves to per-node-visit (§5, corpus-neutral).
  The resident set becomes bounded. *The XL heart; the slice the whole item exists for.*
- **P6.4c — budget config + hardening.** The handle-level memory budget (API surface),
  pin-safety hardening, and large-file tests (a database far exceeding the pool budget opens,
  scans, mutates, and round-trips correctly while the resident page count stays bounded).

Deferred and explicitly **out of this item** (separate TODO entries, none foreclosed):
**streaming + spill-to-disk operators** (sort / hash join / aggregate / DISTINCT under a memory
budget — a *query-operator* memory bound, distinct from this *storage* page cache) and a
**point-lookup / index** path (would change which pages a query touches, hence cost).

## 7. Determinism & cross-core notes

- **Results + cost are the only contract**, and both are invariant to the pool (§3, §5). The
  pager, the pool, and CLOCK are internal performance machinery, **not** a byte contract — each
  core implements them idiomatically (like P5.3's per-core concurrency), provided results and
  cost stay byte-identical.
- **No nondeterminism leaks.** The pool keys on page id (deterministic), never on hashmap
  iteration order; eviction never affects which rows/pages a query logically touches; the I/O on
  a miss is unmetered, so timing never enters cost (CLAUDE.md §8/§10).
- **Memory safety holds** — the file backing is `pread`-style random reads into owned buffers in
  every core (no `unsafe`, no cgo; CLAUDE.md §2/§13).
