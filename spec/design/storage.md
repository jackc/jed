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

> **Step-5b status (whole-image commit).** Persistence has landed in a deliberately
> narrowed form: a commit serializes the **entire database to one byte image** rather than
> writing only changed pages. The incremental machinery this section describes —
> copy-on-write of just the dirty path, the free-list, per-page reuse, B-tree interior
> pages — is **deferred until `UPDATE`/`DELETE` exist** (nothing exercises it before then;
> CLAUDE.md §11). What *is* built now and forward-compatible: the two meta slots, the
> checksum, the root pointer, and the load-bearing **write-ordering rule** (write body
> pages + `sync()`, *then* publish the meta + `sync()`), so the live incremental commit is
> an additive change, not a reshape. The whole-image writer fills both meta slots with the
> same `txid`; slot alternation belongs to the future incremental path. The byte layout is
> [../fileformat/format.md](../fileformat/format.md).
>
> The **host API** ([api.md](api.md)) makes whole-image durability crash-safe at the file
> level with a temp-file + `fsync` + atomic `rename` + directory `fsync` sequence (since a
> commit rewrites the entire file, rename gives all-or-nothing replacement for free). The
> double-meta slots above remain the hook for the future *incremental in-place* commit; they
> are not needed for whole-image durability. `commit` is **explicit** and `close` does not
> auto-flush (api.md §2).

**The root swap, in two backings (transactions.md).** The §3 staging buffer + atomic publish
this section describes is realized in [transactions.md](transactions.md): the writer's
pending set is an **immutable in-memory `Snapshot`** (a working root built from the committed
root via a persistent, structurally-shared ordered map), and the "root-pointer swap" of step
3 is **the in-memory `Snapshot` swap in Phase 5** and **the meta-page root pointer in Phase
6** — the same atomic publish, two backings. Phase 5 keeps durability whole-image (the recipe
above, behind the §2 block seam); Phase 6 replaces only the materialization with incremental
copy-on-write — write the dirty pages the new root introduced, `sync`, publish the alternate
meta slot, `sync` — under a **frozen** transaction API. Because the in-memory store is already
a copy-on-write B-tree (transactions.md §3), Phase 6's "B-tree interior pages" and "incremental
COW commit" (§6 below) become one slice: page-backing the tree that already exists.

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

- **On-disk byte format** — ✅ **authored** (step-5b) in [../fileformat/format.md](../fileformat/format.md):
  magic/version, double-buffered meta with a checksum, catalog + data page chains, record
  layout, value codec, byte-exact fixtures, and the cross-core golden round-trip (a file
  written by Rust is byte-identical to one written by Go — CLAUDE.md §8). This doc fixes the
  *model*; that fixes the *bytes*. **Whole-image** form for now (see the §4 status note).
- **Incremental commit (COW path, free-list, page reclamation, B-tree interior pages)** —
  still deferred. `UPDATE`/`DELETE` have **landed** (step 6) on the whole-image store: a
  mutation is applied in memory and the next serialize rewrites the full image, so nothing
  yet *requires* incremental copy-on-write or free-list reclamation — they remain a later
  slice once write volume makes full-image rewrites costly. The data page layout is a
  simple sorted-record chain, not yet a B-tree. (`DELETE` does free in-memory rows; the
  no-PK rowid is a **monotonic counter**, reconstructed on load as `max key + 1`, so a
  freed rowid is never reissued — see [../fileformat/format.md](../fileformat/format.md).)
- **Within-page structure** — currently variable-length records packed greedily into the
  page payload (a record stores its key + each column's value). Slotted pages / a B-tree
  leaf layout arrive with the incremental commit path.
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
- **Compression of large values** — kept open, not built (CLAUDE.md §9). Large
  `text`/`bytea`/`json` values may be compressed (likely **LZ4**) before they reach a page,
  pairing with the deferred overflow-page path — a value larger than one page currently
  trips the `0A000` oversized-item narrowing ([types.md §11](types.md),
  [../fileformat/format.md](../fileformat/format.md)). Any compression library is a
  third-party dependency, added under CLAUDE.md §14.
- **Cost unit for storage reads** — the cost-accounting seam (CLAUDE.md §13,
  [cost.md](cost.md)) meters storage with a `storage_row_read` unit (one row read from a
  store during a scan), because the store is whole-image / row-granular today (§6 above —
  a scan reads N rows; there is no page abstraction yet). When a real paged store lands, a
  distinct `page_read` unit is **added** to [../cost/schedule.toml](../cost/schedule.toml)
  — `storage_row_read` is **not** renamed (a row read and a page read are distinct events
  that can coexist).
