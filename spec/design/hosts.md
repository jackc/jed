# Storage hosts — the formal block-device interface

> The narrow byte-level interface every storage host implements, and the catalog of hosts.
> This formalizes the seam [storage.md](storage.md) §2 describes informally: it fixes the
> *method set and contract* a host must satisfy so "single-file, embeddable, everywhere"
> (CLAUDE.md §1/§9) is a small added backing, not a reshape. [storage.md](storage.md) §4 owns
> the **commit model** that drives this seam; [api.md](api.md) owns the **host program** API
> above it; this doc owns the **host backing** below it. It also fixes where the two future
> decorations — **encryption** ([encryption.md](encryption.md)) and **replication**
> ([replication.md](replication.md)) — sit relative to the seam. When a decision here changes,
> update [CLAUDE.md](../../CLAUDE.md) §9 and [storage.md](storage.md) §2 in the same edit.

## 1. What a host is, and what it is not

A **storage host** is the per-platform byte backing for one database file: the thing that
turns "page *i*'s bytes" into actual durable storage. It is the *only* per-platform code in
the storage stack. Everything above it — the buffer pool, the page math, the copy-on-write
B-tree, the commit recipe, the catalog — is host-agnostic core logic, **identical across
implementations** (CLAUDE.md §2) and identical across hosts within an implementation.

The host is **not** the pager. Each core's [`pager`](pager.md) once was a concrete struct
that both *owned the `std::fs`/`os`/Node `fs` file* and *implemented the policy above it*
(chunked preallocation — pager.md §7, the bounded buffer pool, the fault-injection seam —
storage.md §7). Formalizing storage hosts **split those two jobs** (✅ done, §7): the policy
stays in the core pager (per-core, host-independent); the raw byte device is now a small
**`BlockStore`** the pager composes (§3). That split is what lets the browser/OPFS host, an
encrypting backing, and a replicating backing each be *an added `BlockStore`*, not a second
pager.

> **Status.** ✅ The **`BlockStore` extraction has landed** (§7): the file backing is lifted out
> of the per-core `Pager` into a `FileBlockStore` the pager composes (Rust `blockstore.rs`, Go
> `blockstore.go`, TS `fileblockstore.ts` — the TS `node:fs` impl was later split out of `blockstore.ts`,
> which now holds just the browser-clean interface, for the OPFS work below), with the policy — page math, the 1 MiB preallocation chunk,
> the barrier choice, the fault-injection seam — staying in the host-independent `Pager`. It was a
> pure refactor: the goldens, the conformance corpus, and the crash-recovery suites are unchanged.
> The pure **`MemoryBlockStore` host has landed** in all three cores as the first B3 building block, but
> the in-memory database path **has since been unified** (bplus-reshape.md B3) — it was a separate, fully-resident code path; unifying it
> onto that byte-buffer `BlockStore` changes observable residency/commit semantics and is the next B3
> wiring slice (§4 in-memory row). The **OPFS host has since
> landed in the TS core** (§5): an `OpfsBlockStore` over `FileSystemSyncAccessHandle`, validated by
> file-host byte parity against the Node `fs` host (the §8 cross-core round-trip, run in Node with a
> fake sync handle), plus a Web-Worker packaging + async client and a Vite/Playwright e2e harness.
> Making the TS engine browser-bundle-clean required lifting the remaining `node:*` imports out of its
> transitive graph behind seams (the Node `fs` `FileBlockStore` split into `fileblockstore.ts`; a
> `SpillSink` seam with the Node spill backing in `spillfile.ts`; Web Crypto for the entropy default) —
> the same interface/impl discipline as this `BlockStore` split. It is a TS-only host (Rust/Go have no
> browser target).
>
> **A second host-supplied-bytes path (not a `BlockStore`).** Collation / Unicode-casing data follows
> the same *host supplies bytes, the engine consumes them* principle but is **not** part of this
> byte-device seam: a host hands the engine a pinned **Unicode-data bundle** via the handle-scoped,
> privileged `db.LoadUnicodeData(bytesOrReader)` ([collation.md §4/§9](collation.md)) — bytes/reader,
> never a path, so the engine still does no I/O. It is closer to the entropy/clock seam
> ([entropy.md](entropy.md)) — a host-injected input on the `Database` handle — than to the per-page
> `BlockStore`. A bare engine with no bundle loaded is `C` collation + ASCII casing only.

## 2. The interface

The core sees storage as a flat array of fixed-size **pages** addressed by index (storage.md
§3). The host underneath is even narrower — it sees an opaque, growable **byte file**. Pages
are the pager's abstraction; the host deals only in offsets and lengths, so it has no
knowledge of page layout, meta slots, the B-tree, or even the page size (the pager owns all
of that). This is deliberate: the smaller the host surface, the cheaper every host —
including OPFS and pure memory — is to implement, and the less per-platform code can drift.

```
BlockStore (the host backing — implemented per platform)
  read_at(offset, len)   -> bytes      # read len bytes at byte offset
  write_at(offset, bytes)              # stage a write of bytes at byte offset
  sync()                               # DATA-only durability barrier (fdatasync) for in-region overwrites
  size()                 -> bytes      # current file length in bytes
  set_size(bytes)                      # DURABLY grow (allocate zeros + full fsync) / truncate — the metadata barrier
```

Five methods, all of which every host — `std::fs`, `os.File`, Node `fs`, OPFS, and an
in-memory `Vec`/slice — can implement cheaply and synchronously. Notably **absent**: any
notion of pages, allocation policy, or atomicity. Those live in the pager (page math,
preallocation) and the commit model (atomicity via the meta-slot swap, storage.md §4) — *above*
this seam, written once per core.

**Why byte-addressed, not page-addressed.** The informal seam in storage.md §2 lists
`read_block(index)` / `write_block(index, bytes)` / `allocate_block()`. Those are the
*pager's* operations and stay exactly as they are (§3). The host beneath them is byte-addressed
because (a) preallocation (pager.md §7) grows the file by a 1 MiB chunk at once, not a page at
a time, so a page-granular `allocate_block` on the host would be the wrong unit; and (b) OPFS's
`FileSystemSyncAccessHandle` is itself byte-addressed (`read`/`write` at an offset,
`truncate`, `getSize`, `flush`) — matching the host surface to it keeps that host a thin
adapter. The pager converts between page index and byte offset (`offset = index × page_size`),
which it already does.

### 2.1 Contracts

- **`read_at` / `write_at` are positioned and do not move a shared cursor.** Concurrent reads
  by lock-free readers (transactions.md §10) and the writer must not interfere through a shared
  seek position. Map to `pread`/`pwrite`-style positioned I/O (Rust's safe `FileExt` read on
  Unix/Windows, Go `ReadAt`/`WriteAt`, OPFS `read`/`write` with an `at` option). A Rust target whose
  standard library exposes no safe positioned-read trait uses `seek` + `read_exact` on the
  block store's private handle under the pager lock: still correct and serialized, with only the
  extra seek syscall. A short read past `size()` is the host's error, surfaced as `58030 io_error`
  (§4).
- **`write_at` is *staged*, not durable.** It may land in an OS page cache; only `sync()`
  guarantees durability. The commit recipe (storage.md §4) relies on this exactly: write all
  dirty body pages, `sync()`, write the meta slot, `sync()` — the two barriers are the only
  durability points.
- **There are two durability barriers, and the *caller* picks — it is not a host flavor.** The
  engine deliberately separates a **data-only** barrier from a **metadata** barrier, because on
  ext4 a write+barrier into a *growing* file drags the inode-size/extent journal into the flush
  (~4.3 ms) while the same write into already-allocated space is metadata-free (~1.4 ms; pager.md
  §7). So which barrier is needed depends on whether the file grew — a pager decision, not
  something the host chooses blindly:
  - **`sync()` is the data-only barrier (`fdatasync`).** It makes every prior `write_at` into
    already-allocated space durable, *without* flushing a file-size/inode-timestamp metadata
    journal. This is the per-commit chokepoint — called twice per commit (body pages, then the
    meta slot; storage.md §4) — and steady-state commits overwrite preallocated space, so it is
    metadata-free.
  - **`set_size` is the metadata barrier (full `fsync`)** — see the next bullet; growth is the
    only metadata change the engine makes, so the full barrier is bundled there.
  The *realization* is per-core (not a cross-core byte contract, like the buffer pool and the
  fault seam): Rust `sync_data`/`sync_all`, Go `syscall.Fdatasync` (behind a `linux` build tag,
  pure Go — no cgo, CLAUDE.md §2) / `File.Sync`, Node `fdatasyncSync`/`fsyncSync`, OPFS
  `flush()`. A host lacking a data-only barrier may implement `sync()` as a **full** `fsync` —
  **correct, just slower** (it loses the metadata-free win, not durability). The contract is
  "after `sync()` returns, every prior in-region `write_at` is durable"; full vs. data-only is
  which syscall delivers it.
- **`set_size` durably grows the file — it is the metadata barrier.** It extends the file with
  **real, durably-allocated zero blocks** *and* makes that allocation durable (a full `fsync`)
  before returning. The preallocation step (pager.md §7) is exactly a `set_size` to the next
  1 MiB chunk ahead of the committed high-water; bundling the `fsync` here is load-bearing, not
  cosmetic — the allocation **must** be durable *before* a later `sync()` overwrites the region,
  so that per-commit barrier stays data-only/metadata-free (pager.md §7) and the crash-ordering
  holds (the allocation lands before the commit that relies on it — storage.md §4/§7). The
  contract: after `set_size` returns, bytes in `[old_size, new_size)` read back as zero **and**
  the allocation is durable. A host that can only resize logically (a sparse truncate, no
  allocating write/`fsync`) stays **correct** but pays the file-growth metadata journal on the
  *next* `sync()` (the ~4.3 ms growing-file cost) — it forfeits the optimization, not
  correctness. (Truncation/shrink needs no barrier, and the engine is **grow-only today** —
  free-list reuse recycles space but never returns it to the OS, so `set_size` is effectively
  durable-grow. **File shrinking is a deferred feature** whose decided approach is `to_image`-based
  whole-image compaction — re-serialize the committed snapshot into a fresh, smaller file — which
  reaches the seam as a normal smaller write, not an in-place truncate; the lighter trailing-free
  truncation variant is the only path that would call `set_size` *down*. storage.md §6.)
- **The physical file length is `≥` the logical `page_count`.** The pager tracks
  `allocated_pages` (= `size() / page_size`, the physical high-water that preallocation runs
  ahead) distinct from the committed logical `page_count` the meta records (storage.md §9). The
  slack pages in `[page_count, allocated_pages)` are **unreferenced trailing zeros** — no
  byte-contract impact (they are past the high-water; the goldens and `create`'s from-scratch
  image are not preallocated, so they stay byte-exact). A host never needs to know about the
  distinction; it only sees `size()`/`set_size`.
- **No iteration order, no wall-clock, no allocation-order leak** (CLAUDE.md §8). The host
  returns bytes for offsets; it introduces nothing nondeterministic into the result, the cost,
  or the on-disk bytes. (Replication and encryption, which *do* ride this seam, preserve this —
  replication is outside the conformance contract like benchmarks, replication.md §6; encryption
  uses a deterministic nonce, encryption.md §3.)

## 3. The pager keeps its page-level operations

The block-level operations storage.md §2 names stay exactly where they are — they are the
**pager's** API to the rest of the core, now expressed *over* a `BlockStore`:

| pager op (storage.md §2) | becomes, over a `BlockStore` |
|---|---|
| `read_block(index)` | `store.read_at(index × page_size, page_size)` (through the buffer pool — pager.md §4) |
| `write_block(index, bytes)` | `store.write_at(index × page_size, bytes)` |
| `allocate_block()` | bump the logical high-water; `set_size` only when it crosses `allocated_pages` (preallocation, pager.md §7) |
| `sync()` | `store.sync()` |
| `block_count()` | logical `page_count` (meta), distinct from `store.size() / page_size` |

The extraction was mechanical (✅ landed, §7): the pager's former direct `File` calls became
`BlockStore` calls, and the file-specific bits moved into `FileBlockStore` — `open`, the data-only
`fdatasync` (`sync()`), and the durable-grow zero-write+`fsync` (`set_size`). The *policy* of
*when* to grow (the 1 MiB preallocation chunk, in the pager's `reserve`) and *when* to barrier
(body, then meta) stayed in the pager — host-independent, identical across cores — so the host
never decides between the two barriers; it only implements each faithfully. The buffer pool, the
preallocation policy, the page math, and the fault-injection seam (storage.md §7) **did not move**.
The fault seam keeps testing the *commit recipe*; it does not need a per-host variant. (A short-read
header check that once relied on a partial `read_exact` now precedes the read as a `size() < 12`
guard, since `read_at` surfaces a short read as `58030` — same `XX001` outcome.)

## 4. The host catalog

| host | backing | status | notes |
|---|---|---|---|
| **in-memory** | a `Vec`/slice of bytes (or pages) | ✅ `MemoryBlockStore` host + engine wiring (bplus-reshape.md B3) | the natural fit for the RAM-sized target (CLAUDE.md §9) and the default for tests/conformance — no filesystem, fully deterministic. `sync()` is a no-op at the host seam. The engine reads/writes it through the **same pager + Packed leaf path as a file** (a pinned, unbounded pool — resident by definition): `commit` packs the dirty pages into the store — the file commit minus durability — and `resident_leaves()` is a real gauge. |
| **Rust file** | `std::fs::File`, positioned read/write + `fsync`/`fdatasync` | ✅ built (`FileBlockStore`, `blockstore.rs`) | pure `std::fs`, no dependency, memory-safe (CLAUDE.md §13). Reads use safe standard-library positioned I/O on Unix/Windows (`read_exact_at` / a `seek_read` exact-read loop), with a correct serialized `seek`+`read_exact` fallback elsewhere. Closes the file on drop (RAII). |
| **Go file** | `os.File` `ReadAt`/`WriteAt`/`Truncate`/`Sync` | ✅ built (`fileBlockStore`, `blockstore.go`) | pure Go — **no cgo, no FFI** (CLAUDE.md §2). `fdatasync` via `syscall.Fdatasync` behind a `linux` build tag (`blockstore_datasync_linux.go`), full `Sync` fallback elsewhere. |
| **Node `fs`** | `openSync`/positioned `writeSync`/`fsyncSync` | ✅ built (`FileBlockStore`, `impl/ts/src/fileblockstore.ts`) | the TS core's durable backing; the `node:fs` impl is isolated in `fileblockstore.ts` (the browser-clean `BlockStore` interface is `blockstore.ts`; the host program layer is `file.ts`) precisely so OPFS is a sibling, not a reshape — and so `node:fs` never reaches a browser bundle. |
| **Browser / OPFS** | `FileSystemSyncAccessHandle` (`read`/`write`/`truncate`/`getSize`/`flush`) | ✅ built (`OpfsBlockStore`, `impl/ts/src/opfsblockstore.ts`; TS only) | the synchronous access-handle API maps one-to-one onto the §2 `BlockStore` surface (§5). Acquired async at the bootstrap edge (`opfs.ts`); the engine runs in a Web Worker driven by an async client (`src/browser/`). Validated by file-host byte parity (the existing goldens). |
| **encrypting** | wraps another `BlockStore`/the in-core codec | ⏳ design door ([encryption.md](encryption.md)) | a page codec **above** the seam, not a host (encryption.md §2); the host stays opaque-byte. |
| **replicating** | a tee wrapping another `BlockStore` | ⏳ design door ([replication.md](replication.md)) | a seam-level tee **below** the codec, so it ships ciphertext (replication.md §4). |

**File-open errors** map to the host-filesystem class-58 SQLSTATEs (api.md §2.1, §7): a
missing file on `open` is `58P01 undefined_file`; an existing file on `create` is `58P02
duplicate_file`; an underlying read/write/sync failure is `58030 io_error`; a malformed or
out-of-range header is `XX001 data_corrupted`. These are raised in the **host program** layer
(api.md), which is above the `BlockStore`; the `BlockStore` itself surfaces raw I/O failures
that the layer maps. The class is a stable category (api.md §7).

**File locking is a host-layer duty, not a `BlockStore` method ([locking.md](locking.md)).**
The host program layer joins the stable `<path>.lock/` coordination bundle **before the first content
read**; the five-method byte seam remains lock-unaware. Each host declares a tier: `os-shared`
(Rust/Go local files — whole-file `flock` on Unix, `LockFileEx` on Windows), `native-adapter-shared`
(Node, only with the explicitly approved adapter), `inherent-exclusive` (OPFS sync access handle), or
`unavailable` (wasm32-wasip1). Unsupported requested coordination fails closed `0A000`; there is no
cooperative PID/mtime fallback. See locking.md §3/§7 for the normative five-lock protocol.

## 5. The OPFS host (the build target)

The browser host backs the database on the Origin Private File System via a
`FileSystemSyncAccessHandle` — the one browser API that offers **synchronous** file I/O
(`read(buf, {at})`, `write(buf, {at})`, `truncate(n)`, `getSize()`, `flush()`), which is what
the core needs (the engine is synchronous above the seam; transactions.md §10 notes the TS
core's concurrency is isolation-across-async, not threads). The mapping is one-to-one:

| `BlockStore` (§2) | OPFS |
|---|---|
| `read_at(off, len)` | `handle.read(buf, { at: off })` |
| `write_at(off, bytes)` | `handle.write(bytes, { at: off })` |
| `sync()` | `handle.flush()` |
| `size()` | `handle.getSize()` |
| `set_size(n)` | `handle.write(zeros, { at: old }) ; handle.flush()` (grow) / `handle.truncate(n)` (shrink) |

**OPFS has only one barrier.** There is no `fdatasync`/`fsync` split — `flush()` is the sole
durability primitive — so both barriers of §2.1 collapse onto it: `sync()` *is* `flush()`, and
`set_size`'s durable grow is "write real zero blocks, then `flush()`" (not a bare `truncate(n)`,
which extends sparsely and would re-allocate-and-journal on first write — the §2.1 caveat). OPFS
thus cannot get the metadata-free per-commit win the file hosts get from the two-barrier split;
by the §2.1 contract that is **correct, just slower** — the right default for a browser host,
where the absolute commit latency matters less than on a server. (`flush()`'s exact durability
guarantee is also browser-implementation-defined; the engine's atomicity does not depend on it
beyond the meta-slot ordering — storage.md §4.)

**Parity is the contract.** The OPFS host votes on nothing new semantically — it must produce
and consume the **same bytes** as the Node `fs` host (the §8 cross-core/round-trip golden:
a file written by any core/host is byte-readable by any other). So the test is *file-host
parity*: write a database through the Node host, open it through OPFS (and vice versa), and
get identical pages and query results. The existing golden fixtures already pin the bytes;
OPFS just has to reproduce them. No new conformance capability, no format-version bump.

**Open questions — resolved when the host was built:**

- **Access-handle lifetime: one long-lived, exclusive handle** for the database's life, acquired at
  open/create and released by `closeOpfs` — mirroring the file cores' "own the file for the handle's
  life" (pager.md). OPFS's exclusive lock on a sync access handle *is* the single-writer guarantee
  (CLAUDE.md §3), enforced by the platform. Documented divergence: one jed handle per file per origin —
  two tabs cannot both open the same database, even read-only (file hosts allow concurrent OS opens).
- **`create`: write in place** — no temp-file + rename (OPFS has no POSIX rename). The rename only ever
  protected the initial whole-image write; every later commit's all-or-nothing comes from the meta-slot
  swap + per-page CRC (storage.md §4), and a torn `create` is detected as `XX001` on open (never silent
  bad data, and no committed data to lose). `FileSystemFileHandle.move()` is an optional hardening, not
  required.
- **Worker-thread requirement: engine-in-Worker + async RPC.** Sync access handles are Worker-only in
  most engines and acquisition is async (`getDirectory` → `getFileHandle` → `createSyncAccessHandle`), so
  the whole TS core runs in a dedicated Web Worker and the main thread drives it over `postMessage`
  (`src/browser/worker.ts` + `client.ts`). The engine and the `BlockStore` seam stay **synchronous**;
  only the acquisition edge (`opfs.ts`) and the client surface are async — hence the async browser entry
  points (`createOpfs`/`openOpfs`), a documented per-platform divergence from the synchronous file
  create/open (api.md §6). `db.path` is left null for OPFS (it is durable via `db.paging`, but has no
  filesystem path), which keeps disk-spill off (the `ORDER BY` external merge sort has no OPFS backing
  yet — a later enhancement; sorts stay resident, spill.md §2). The txid-advance signal was re-keyed
  from `path` to the `persistHook` so an OPFS commit still advances txid (observably identical for the
  file/in-memory hosts).

**Browser packaging + verification.** The TS engine was made browser-bundle-clean by lifting its last
`node:*` imports behind seams (the `BlockStore` interface in `blockstore.ts` with the Node impl in
`fileblockstore.ts`; a `SpillSink` seam with the Node spill backing in `spillfile.ts`; the entropy
default via global Web Crypto) — verified by an import-graph trace (`opfs.ts` reaches the engine with
zero `node:*`) and a clean Vite build of the Worker chunk. Two test layers: a **dependency-free Node
parity test** (`tests/opfs_parity.test.ts`, a fake sync handle — proves the byte contract both
directions against the goldens, the §8 "done" criterion) and a **gated real-browser e2e** (Playwright +
Vite, `e2e/opfs.spec.ts`, `npm run test:browser` — real `FileSystemSyncAccessHandle` in a real Worker,
incl. durability across a page reload). Both are **outside `rake ci`** (TS unit tests are; the browser
e2e needs a Chromium binary) — the OPFS host adds no SQL semantics, so conformance is unchanged.

## 6. The decoration layering (where encryption and replication sit)

Encryption and replication are **not** new hosts and **not** new methods on the seam — they
are two thin layers in the byte path, and their *order* relative to the seam is a deliberate
design choice with a real consequence:

```
core (pager: buffer pool, page math, preallocation)
  ↓ plaintext pages
encryption codec        ← in-core, per-core, §8 byte contract (encryption.md §2)
  ↓ ciphertext pages
the block seam (§2 BlockStore)
  ↓ ciphertext bytes
[ replication tee ]     ← captures the per-commit dirty pages + meta → stream (replication.md §4)
  ↓
base host (in-memory | file | OPFS)
```

- **Encryption is a codec *above* the seam, in the core** — not a host duty — so it is written
  once per core (3×, with shared fixtures) rather than buried in every host, which is what keeps
  the §8 byte-identity contract tractable (encryption.md §2). The host stays a dumb opaque-byte
  device, which *is* the existing "don't assume page bytes are plaintext-comparable on disk"
  requirement (storage.md §6).
- **Replication is a tee *below* the codec, at the seam** — so it captures **ciphertext**. The
  consequence: a replica stores opaque encrypted pages and needs the key only to *query*, never
  to *replicate* — a **keyless replica/backup** (replication.md §4). This property exists *only*
  because the tee is below the codec; the layering is chosen for it.

Both decorations ride the same object the commit already produces — the per-commit set of
dirty pages + the meta swap (storage.md §4). That single fact is why neither needs a new
subsystem: encryption transforms each page as it crosses the seam, replication copies the set
as it crosses. See the two docs for the full designs.

## 7. Open / deferred

- **`BlockStore` extraction** — ✅ **landed** (§1/§3): the file backing is lifted out of the
  per-core `Pager` behind the five-method interface (`FileBlockStore` — `blockstore.{rs,go}`, and TS
  `fileblockstore.ts` since the OPFS split left `blockstore.ts` interface-only), with the policy left in
  the host-independent `Pager`. A pure refactor —
  the goldens, the conformance corpus, the crash-recovery suites, and the NoREC sweep are all
  unchanged. Foundation for every host below.
  - **In-memory path — MemoryBlockStore host landed, engine routing still open.** The byte-buffer host
    exists in all three cores (`MemoryBlockStore` / `memoryBlockStore`) and is covered by direct
    contract tests. The database path still holds its data as a decoded tree (`persist` a no-op, fully
    resident, `resident_leaves() == 0`); routing it through `MemoryBlockStore` + pager + pool is the
    behavior-changing B3 follow-on.
- **OPFS host** — ✅ **landed** in the TS core (§5): `OpfsBlockStore`, a thin adapter against the
  extracted seam (not a second pager), plus the Web-Worker packaging + async client. Validated by
  file-host byte parity (Node parity test) + a gated real-browser e2e. TS-only.
  - **Deferred OPFS follow-ons** (none foreclosed): disk-spill for OPFS (the `ORDER BY` external merge
    sort currently stays resident for OPFS — `db.path` is null so the `SpillSink` is unset; an
    OPFS-backed `SpillSink` is the path); read-only multi-handle via `createSyncAccessHandle({ mode })`
    (not portable yet); and running the real-browser e2e in CI (needs a headless-Chromium binary, today
    outside `rake ci`).
- **Shared multi-process file coordination** — ⏳ **decided, spec'd
  ([locking.md](locking.md)), not built**: the five-file OS-lock bundle, append-only contended commit,
  and presence-EX uncontended lease are the first locking slice. Rust/Go need no dependency. Node's
  narrow native OS-lock adapter is an explicit §14/FFI decision gate; no package is approved yet.
- **Encryption codec** — ⏳ design door ([encryption.md](encryption.md)); not built. Crypto is a
  §14 vetted-library decision requiring explicit confirmation before any dependency lands.
- **Replication tee** — ⏳ design door ([replication.md](replication.md)); block-shipping decided,
  not built.
- **Direct sub-SQL access over the seam** — kept open, not built (storage.md §5): the key
  encoding + record format are specified independently of SQL, so a direct reader could reuse
  them over a `BlockStore`. A constraint on where the seam sits, not a commitment.
