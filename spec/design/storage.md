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

- **In-RAM dataset is the common case.** The whole dataset resident in memory is expected,
  so the in-memory representation is a **first-class concern**, not a cache over disk. The
  core operates on in-memory structures; persistence is how those structures are made
  durable, not how they are accessed.
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
- **TS core (browser)** — OPFS (`FileSystemSyncAccessHandle`), later (CLAUDE.md §9).
- **In-memory** — a `Vec`/slice of pages; the natural fit for the in-RAM target and the
  default for tests (no filesystem, fully deterministic).

The core logic above this seam is **identical across implementations** and storage-host
agnostic. Only the few methods above are per-host. Keep the interface this small: every
method is something every host (including OPFS and pure memory) can implement cheaply.

## 3. Page model

- **Fixed-size pages**, page-aligned (SSD target, §1). **Default 4 KiB**, recorded in the
  file header so it is a format parameter, not a hardcoded constant — chosen to match
  common SSD/OS page granularity; revisitable per the fixtures without code assumptions.
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
and alternates between them (with a checksum) so a torn write during publish can always
fall back to the previous valid meta — detail specified in [../fileformat/](../fileformat/).

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

- **On-disk byte format** — magic/version/meta-page/free-list/page layout, with fixtures
  and the cross-core round-trip test. Authored in [../fileformat/](../fileformat/) at
  CLAUDE.md §11 step 5 (when the first slice persists). This doc fixes the *model*; that
  fixes the *bytes*.
- **Within-page structure** — slotted page vs. fixed records; B-tree vs. other index page
  layout. Decided with the format, driven by the first slice's access patterns (point
  lookup by PK).
- **Free-list / page reclamation** — representation of freed pages. Specified with the
  format.
- **Crash-recovery story** — the meta double-buffer (§4) gives atomic commit; whether a
  separate WAL is ever added is deferred (the copy-on-write + root-swap model does not
  require one for atomicity).
- **Alternative physical layouts & direct-access API** — kept open (§5), not scheduled.
