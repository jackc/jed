# File locking & multi-process access — design

> What happens when more than one process (or more than one handle) opens the same database
> file. This doc fixes the **decided immediate implementation** — an **exclusive-by-default
> whole-file lock** acquired at `open`/`create`/`attach` (§2–§6) — and **records** the designed
> but deliberately unscheduled follow-on: **shared multi-process access with the lease
> refinement** (§7). [hosts.md](hosts.md) owns the host backing beneath the seam; [api.md](api.md)
> §2.1 owns the open-option surface; [storage.md](storage.md) §4/§6 own the commit model and the
> reclamation this doc's rules protect. When a decision here changes, update
> [CLAUDE.md](../../CLAUDE.md) §9 and those docs in the same edit.

## 1. The problem, and the staging decision

The engine assumes it is **alone with the file**. Every load-bearing storage mechanism bakes
that in: the free-list is reconstructed on open from the committed root's reachable set and
consumed all session (storage.md §6) — a second committing process would double-allocate the
same "free" pages; the buffer pool caches pages assuming their bytes never change beneath it
(pager.md §3); the reader-liveness watermark that gates page reuse is an **in-process**
registry (transactions.md §8); and two writers would both alternate the meta slots against
different bases. Two handles on one file — from two processes *or* one — is silent lost
updates at best and file corruption at worst, with no defined error. That is the gap.

**Decision (2026-07-04): stage it in two steps.**

1. **Exclusive-by-default locking** (§2–§6) is the **immediate implementation**: one handle
   owns the file, a second open fails with a defined error (or waits, bounded by a timeout).
   Corruption-by-concurrency becomes structurally impossible, at zero steady-state cost.
2. **Shared multi-process access** — concurrent processes over one file, with the **lease
   refinement** that keeps its alone-case overhead at effectively zero — is **designed and
   recorded in §7 but not scheduled**. v1 deliberately reserves the lock-state space §7 needs
   (§2.2), so the follow-on is additive, not a migration.

**Why shared mode was demoted.** The motivating scenario was the zero-downtime (blue/green)
web-app deploy: old and new versions co-resident, both possibly writing. On analysis the
scenario is served *without* co-residency: the new process opens with a generous
`lock_timeout_ms` and simply **waits for the old process to close**; it exposes a liveness
endpoint that needs no database access, so the orchestrator promotes it and requests queue for
the few seconds the old process takes to drain. No true downtime, no shared-state machinery.
Co-resident *writing* remains a real (rarer) want — §7 records the full design for when it is.

**PG/SQLite posture (CLAUDE.md §1).** PostgreSQL is no guide here (a server owns its files
outright). SQLite — the deployment-model north star — defaults to *shared* access via
rollback/WAL locking and pays for it on every transaction. jed deliberately diverges:
**exclusive by default** (the overwhelmingly common embedded case pays nothing), shared as a
recorded opt-in follow-on. Recorded here per the §1 divergence rule.

## 2. v1 semantics — the exclusive lock

### 2.1 Acquisition, lifetime, release

- **One lock per database file.** `create` and `open` acquire it on the main file;
  `db.attach` of a **file** attachment acquires an independent lock on that file
  (attached-databases.md §2 — the lock is per *file*, the sharing boundary). An in-memory
  database or attachment has no file and no lock.
- **Acquired before the first content read.** `open` takes the lock **before** reading or
  validating the header, so a torn state written by a concurrent (soon-to-be-excluded) writer
  can never be observed. `create` opens the fresh file (`O_EXCL` — it never clobbers, api.md
  §2.1) and locks it before writing the initial image; if another process wins the
  vanishingly-rare race between file creation and lock acquisition, the loser fails `55006`
  like any contended open.
- **Held for the handle's lifetime**, released at `close`/`detach` (and by the OS at process
  death — the lock is kernel-owned state on Unix/Windows, so a crash leaves **no stale lock**;
  the TS side-car is the one exception, §4).
- **Read-only opens take the same exclusive lock.** A read-only handle (api.md §2.1) still
  assumes the file is static beneath it — its buffer pool, catalog, and snapshot are all
  invalidated by a foreign commit — so it must exclude writers, and in v1 that means excluding
  everyone. This is also deliberate forward-compatibility: v1 never takes a *shared* OS lock,
  reserving that state for §7.4 (where `LOCK_SH` must unambiguously mean "a shared-protocol
  participant is present"). Multi-reader coexistence arrives with the follow-on, not before.

### 2.2 The open-option surface (api.md §2.1)

Two open-time options, on `create`, `open`, and `attach` alike (idiomatic per core, the
`cache_bytes` shapes):

- **`locking`** — `exclusive` (**default**) | `none`. `none` skips lock acquisition entirely:
  for hosts that coordinate exclusivity externally, for read-only inspection of a file known
  static, and for filesystems where the primitive is unreliable (NFS, §3). With `none` the
  pre-lock guarantees are the caller's problem — the v1 engine still *assumes* aloneness; the
  option changes who enforces it. (Go: the zero value of the `Locking` field is `exclusive`,
  so the uninitialized-struct default is the safe one.)
- **`lock_timeout_ms`** — non-negative integer, default **0**. `0` = fail immediately if the
  lock is held; `N` = wait up to `N` ms for the holder to release, then fail. This is the
  deploy knob (§1): the incoming process opens with, say, `lock_timeout_ms: 60_000` and
  acquires the instant the outgoing process closes. (Implementations poll a non-blocking
  acquire — interval non-normative, ~10 ms — because `flock` has no native timeout.)

**Errors.** A lock that cannot be acquired — immediately at `lock_timeout_ms = 0`, or on
timeout expiry — is **`55006 object_in_use`** (already registered, `spec/errors/registry.toml`;
the code detach-in-use uses, and PostgreSQL's code for "in use by another"), with `{detail}`
naming the path. A host with **no locking mechanism at all** (§4: wasm32-wasip1) **fails
closed**: `locking = exclusive` (the default) is `0A000 feature_not_supported`
("file locking unavailable on this host; open with locking = none") — an explicit one-line
opt-out beats silently unenforced exclusivity (the CLAUDE.md §13 legibility rule).

## 3. What the lock does — and does not — protect

- **jed-vs-jed, everywhere the OS allows.** On **Unix** the lock is **advisory** (`flock`):
  every jed core respects it, but a non-jed process (`cp`, a backup tool, a rogue writer) is
  not stopped. On **Windows** v1 uses **share-mode exclusion** (§4), which is **mandatory** —
  no other process can open the file at all while a jed handle holds it.
- **Same-process double-open becomes a defined error.** `flock` locks belong to the *open file
  description*, and Windows share modes to the *handle*, so a second `open` of the same path
  **within one process** also fails `55006`. This closes a real existing hazard (two handles =
  two pagers = corruption) that previously had no defined behavior, and makes the contract
  testable in a single process (§6).
- **Local filesystems only.** `flock` over NFS/CIFS ranges from unreliable to lying;
  networked filesystems are **unsupported** for locking (the SQLite posture). Use
  `locking = none` there and coordinate externally.
- **Mixed-core caveat.** The TS core cannot take or observe OS locks (§4); its side-car is
  cooperative among TS processes only. A deployment must not point a TS-core process and a
  Go/Rust-core process at one live file and expect either to exclude the other — declared
  unsupported rather than papered over (the realistic co-residency case, one app's old/new
  versions, is same-core by construction).

## 4. Mechanism per host (normative)

Cross-core interop is the point: **on a given OS, every lock-capable core MUST use exactly
the mechanism specified here**, or a Rust process and a Go process would not exclude each
other. This table is the contract (hosts.md §4 carries the capability column):

| host / core | mechanism | tier | notes |
|---|---|---|---|
| **Rust file, Unix** | `flock(LOCK_EX \| LOCK_NB)` via std `File::try_lock` (stable since 1.89) | `os` | **zero dependencies** — one reason v1 needs no §14 proposal. Timeout = poll `try_lock`. |
| **Go file, Unix** | `syscall.Flock(fd, LOCK_EX \| LOCK_NB)` | `os` | std `syscall`, pure Go, no cgo (CLAUDE.md §2). Same lock kind as Rust ⇒ mutual exclusion holds. |
| **Rust file, Windows** | open with **share mode 0** (`OpenOptionsExt::share_mode(0)`) | `os` (mandatory) | exclusion via the open itself — no `LockFileEx` range to keep in cross-core agreement, and it is OS-enforced against non-jed processes too. |
| **Go file, Windows** | `syscall.CreateFile` with `dwShareMode = 0` | `os` (mandatory) | must match Rust's choice (share mode, not `LockFileEx`) — symmetric conflicts, same semantics. |
| **TS / Node** | side-car lock file `<path>.lock` (`O_EXCL` create; contents: ASCII pid) | `cooperative` | Node has no `flock`/`fcntl` surface. On `EEXIST`: read pid, probe liveness (`process.kill(pid, 0)`), remove-and-retry once if dead, else `55006`. **Best-effort**: a crash leaves a stale file until pid-liveness recovery, pid reuse and pid namespaces (two containers sharing a volume) can defeat the probe. Removed on `close`. |
| **Browser / OPFS** | `createSyncAccessHandle` **is** the lock | `inherent` | the browser grants one sync access handle per file — exclusivity by construction (hosts.md §5); a second open fails at acquisition. `locking = none` is meaningless here (the exclusivity is not jed's to waive). |
| **wasm32-wasip1 (impl/wasm)** | none available | `unavailable` | WASI preview1 has no lock primitive: default-exclusive opens fail `0A000` (§2.2); callers pass `locking = none`. |
| **Ruby gem** | inherits Rust | `os` | wraps the Rust core over the C ABI — conforms by construction (CLAUDE.md §2). |
| **in-memory** | n/a | — | no file; a `MemoryBlockStore` is process-private by nature. |

**Decided against: a universal side-car.** Having Go/Rust *also* create/check `<path>.lock`
would let them cooperate with TS processes — but it imports the side-car's failure mode
(stale files needing heuristic recovery) into the cores whose OS locks are otherwise
crash-clean, to serve a mixed-core co-residency we declare unsupported anyway (§3). The
side-car stays TS-only; the mixed-core rule is documented instead.

**Where it lives.** Locking is part of the **host program layer's file open** (api.md), like
the class-58 open errors (hosts.md §4) — *not* a `BlockStore` method. The five-method byte
seam stays untouched; a `BlockStore` never knows whether its file is locked.

## 5. Interaction with reclamation, compaction, and the pager

The exclusive lock **enforces what the storage layer already assumes** — it adds no new
gating and relaxes none:

- **Free-list reconstruct-on-open** (storage.md §6) is sound precisely because no other
  process can commit between the walk and this handle's own commits. Under the lock that
  precondition is guaranteed rather than hoped.
- **The deferred within-session continuous reclamation and on-disk free-list persistence**
  (the P6.2 follow-ons) stay gated only on the **in-process** watermark
  (transactions.md §8) — correct under v1, since no cross-process reader can exist.
- **Compaction / shrink** (storage.md §6, decided `to_image` mechanism) and the lighter
  trailing-free truncation are writer operations that replace or shorten the file under
  readers; under v1 all readers are in-process, so the watermark remains the only gate.
  (In §7's shared mode, every one of these acquires a *cross-process* aloneness proof —
  that, not v1, is where the reclamation/locking coupling gets real.)
- **Cost & determinism.** Lock acquisition is host-level work, unmetered (like `fsync` —
  cost.md §3); nothing SQL-visible changes. `lock_timeout_ms` is wall-clock and therefore
  nondeterministic, but it is **host-API-surface** nondeterminism outside the conformance
  corpus (the corpus never opens a contended file), the same posture as host-initiated
  `57014` cancellation — no `determinism_exceptions.toml` entry is needed.

## 6. Testing

Locking is **host-API surface + host-state introspection** — structurally out of the corpus's
reach, so per-core unit tests are the right home (the CLAUDE.md §10 criteria):

- second `open` of a held path fails `55006`; succeeds after `close` (both orders);
  `flock`'s per-open-file-description semantics make this testable **within one process** on
  every OS, no child-process harness needed.
- `lock_timeout_ms`: contended open waits, acquires when the holder closes within the
  timeout, fails `55006` past it.
- `locking = none` bypasses acquisition (two handles open — caller's problem, documented).
- read-only open is excluded by, and excludes, a writable holder.
- file `attach` locks the attached file independently; `detach` releases it.
- TS: stale side-car with a dead pid is recovered; a live pid's side-car is `55006`.
- crash-release needs no test on Unix/Windows (kernel-owned); the TS side-car's stale path
  is the test above.

A two-real-process stress lane (spawn, contend, assert `55006`) can join the bench-family
harness later if wanted; it is not required for the contract.

## 7. Follow-on (recorded, NOT scheduled): shared mode with the lease refinement

Everything below is a **design record** so the follow-on starts from decisions, not
archaeology. None of it is committed work; the trigger would be a real workload needing
co-resident writers (the case §1's wait-on-open pattern cannot serve).

### 7.1 Requirements carried forward

Shared access must be **opt-in per handle**, must preserve the §3 single-writer semantics
*globally* (writers in different processes queue; no merge/retry model), and must cost
**effectively nothing when only one process is actually present** — the co-resident state is
a rare, minutes-long window, not the steady state. Per-core asymmetry is acceptable
(Go/Rust support it; TS/OPFS never will — a declared host capability, conformance-tier
style).

### 7.2 The protocol

Two logical locks (byte ranges, §7.4) plus the existing meta discipline:

- **Presence** — held for the handle's lifetime: SH by every shared-mode handle.
- **Write gate** — EX for the duration of each write transaction: the transactions.md §10
  in-process `write_lock` extended across processes. Gate **before** snapshot: on acquiring
  it the writer `pread`s the meta and adopts the newest committed root as its base, so
  cross-process writes are sequentially consistent with no new transaction semantics.
  Blocking bounded by a `lock_timeout` → **`55P03 lock_not_available`** (register with that
  slice; v1 deliberately leaves it unregistered).
- **Read/txn begin freshness** — no lock: `pread` both meta slots (~64 B), CRC-validate,
  adopt the newest valid root. A torn read of the slot a writer is mid-writing fails its CRC
  and falls back to the other slot — the newest *committed* state; this is exactly the
  existing open-time slot arbitration (format.md *Opening*), reused per transaction.
- **Commit** — while holding the gate, **atomically try-convert presence SH→EX**:
  - **success ⇒ provably alone** (no other process has the file open): reuse free-list
    pages, truncate, persist the free-list — full v1 behavior; downgrade after publish.
  - **failure ⇒ co-resident**: commit **append-only** (allocate past the high-water only;
    never reuse, never truncate). Any root any other process could have pinned stays intact,
    so readers need no per-transaction registration at all — and page content becomes
    immutable-once-written for the duration, which is what keeps every process's buffer
    pool valid across foreign commits (only the root moves).

Orphans accumulated during a co-resident window are recovered by the next reconstruct-on-open
(and better by §7.5). A process's *own* tracked frees stay reusable once it is alone again —
a page dead in the committed root can never be resurrected by a serialized successor.

### 7.3 The lease refinement (the alone-case tax killer)

The base protocol costs an alone shared-mode process ~1 µs of meta `pread` per transaction
begin. The lease removes it: when alone, **keep holding presence-EX between commits**, and
toggle EX→SH→EX on a short period (~10–50 ms; two fcntls, amortized nothing). While EX is
held, foreign commits are impossible, so readers **skip the freshness pread entirely** —
behavior and cost identical to exclusive mode. The toggle is the entry window: a newcomer's
SH acquisition lands inside it (bounded open latency, tens of ms — irrelevant for a deploy),
and the holder's failed re-upgrade *is* the arrival signal, degrading it to §7.2 co-resident
behavior. Conversion atomicity (§7.4) makes the failed re-upgrade safe: the holder keeps SH;
correctness never depends on winning the race.

### 7.4 Lock primitives, and why v1 reserved the state space

Three findings from the design analysis, recorded so they are not re-derived:

- **`flock` cannot carry the shared protocol.** Its lock conversions are documented
  non-atomic (drop-then-reacquire — fatal for the lease's downgrade/upgrade), it is
  whole-file (no second range for the write gate), and one lock per open file description.
  The follow-on needs **OFD `fcntl` range locks** on Unix (`F_OFD_SETLK` — atomic
  conversion, per-OFD ownership, immune to the classic close-any-fd-drops-locks hazard;
  Linux ≥ 3.15, macOS too) and **`LockFileEx` ranges** on Windows, at **sentinel offsets
  past any real page** (the SQLite locking-page trick — Windows range locks are mandatory
  and must not sit on bytes readers actually `pread`).
- **Layering against v1 binaries.** `flock` and `fcntl` locks do not interact, so a
  shared-mode process must *also* hold the v1-layer lock to be visible to old binaries:
  it takes **`flock LOCK_SH`** for its lifetime (never converted — presence/lease/gate all
  live on the fcntl ranges). Because **v1 is exclusive-only and never takes `LOCK_SH`**
  (§2.1), SH on the v1 layer unambiguously means "shared-protocol participant": an old
  exclusive binary's `LOCK_EX` and any shared cohort mutually exclude, in both directions.
  On Windows the same falls out of share modes: v1's share-mode-0 conflicts symmetrically
  with the shared cohort's `read|write` sharing. This is the forward-compatibility §2.1
  bought by keeping v1 EX-only.
- **Dependencies (§14 — explicit confirmation required before building).** Rust std has no
  `fcntl`: the shared slice needs a small edge dependency (`rustix` or `libc`) in the host
  layer — clause-1 territory, but it is a named proposal awaiting a yes, not a default. Go
  can stay dependency-free (`syscall.FcntlFlock` + hardcoded per-OS `F_OFD_*` constants,
  or `x/sys` — also a §14 call).

### 7.5 Synergies to bundle

- **On-disk free-list persistence** (the P6.2 follow-on; meta offset 28 is reserved for it)
  should land **with or before** shared mode: it fixes the known O(file-size) open — which
  bites hardest exactly when a process opens mid-deploy — and, because writers serialize on
  the gate, each commit can extend its predecessor's persisted list, so pages freed by a
  departed process's commits are *known* (not leaked until reopen) to the survivor. No
  per-page txid tags needed: a listed page is dead w.r.t. the committed root; *reuse* is
  separately gated on aloneness + the in-process watermark.
- **A `catalog_gen` meta field** (bump on DDL), so a foreign commit that touched no schema
  doesn't force a catalog reload / plan-cache flush (the prepared-statement cache's `catGen`
  invalidation extends across processes). Bundle both meta additions into **one**
  `format_version` bump.
- **Reclamation/compaction gating generalizes cleanly**: every "structural" file operation
  (free-page reuse, truncation, compaction, free-list persist) becomes a privilege of the
  **proven-alone** state — one rule covering v1 (trivially alone) and shared mode (§7.2's
  try-convert). The watermark stays the in-process half of the same predicate.

### 7.6 Explicitly deferred doors (beyond even the follow-on)

- **LMDB-style reader table** (per-snapshot txid pins in a shared side-car, so a writer can
  reclaim *while* co-resident with readers): strictly more precise, real machinery (slot
  allocation, dead-pid sweeping, shared mmap — TS excluded), only worth it if long-lived
  many-process operation becomes a real workload. The sentinel-range space reserves room.
- **mmap of the meta page** as a freshness fast-path: saves only the ~1 µs pread the lease
  already eliminates; costs a Rust `unsafe`/dependency question (§13/§14), has no TS/OPFS
  analog, and punches through the `BlockStore` seam. Recorded as rejected-for-now with the
  reasoning, so it is not re-proposed cold.
