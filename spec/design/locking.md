# File locking & multi-process ownership — design

> What happens when more than one process opens the same database file. This document fixes the
> **coordination bundle** beside every file database, the OS locks in it, and **ownership**: exactly one
> process at a time holds `presence EX` and alone reads and writes the file. Every other process reaches
> the database through the owner's **page service** ([page-service.md](page-service.md), wire contract
> [spec/pageservice/](../pageservice/README.md)). [hosts.md](hosts.md) owns the host backing;
> [api.md](api.md) §2.1 owns the options; [storage.md](storage.md) §4/§6 own persistence and
> reclamation. When a decision here changes, update those docs, [CLAUDE.md](../../CLAUDE.md) §9, and
> [TODO.md](../../TODO.md) together.

## 1. Decision and status

**Status: landed on the Rust, Go, and Node/TypeScript local-file hosts.** The page service replaced
the earlier shared-by-default protocol (shared mode, 2026-07-16 to page-service phase 4) as a clean
break: shared mode had no production users, so there is no migration or mixed-fleet rollout. A new
bundle marker (`protocol-v2`, §3.1) makes a shared-mode binary refuse a bundle the page service uses.

- `locking = auto` is the zero-value default. On a host that can serve (Unix local files) it selects
  the **page service**: the opener that takes `presence EX` owns the file and serves it; every other
  opener is a page client of it. Ownership hands off when the owner leaves
  ([page-service.md](page-service.md) §8). On a host that cannot serve, `auto` selects `exclusive`.
- `exclusive` is a **non-serving owner**: it takes `presence EX` and serves nobody, so a second opener
  waits up to its `file_lock_timeout_ms` and fails `55006`.
- `none` skips jed coordination. It is an unsafe expert escape hatch for a host that enforces the same
  invariant externally; the caller owns every consequence.

## 2. Safety invariants

These are normative. An implementation that cannot uphold them fails closed (`0A000`); it never falls
back to a PID file, mtime lease, or best-effort stale-lock recovery.

1. **Own before read.** A process reads or writes the database file only while it holds
   `presence EX` on the file's bundle. A page client never opens the file.
2. **One owner.** `presence EX` is the only authority over the file: one holder at a time.
3. **The OS lock is the authority.** Ownership never expires and is never stolen. Process death
   releases the locks. Wall-clock time controls only how long a caller waits, never ownership.
4. **Release after letting go.** An owner stops serving, closes every client connection, and stops
   all access to the file before it releases `presence EX` (page-service.md §8;
   spec/pageservice/README.md §6.5), so no page it read can be reused under it by the next owner.
5. **The owner's foreground path is unchanged.** An owner's own reads and writes perform no
   coordination syscall and no meta re-read; its commits use the v33 allocator and validated-COW
   durability recipe. Reuse and reclamation are gated only by the reader watermark, which counts its
   clients' pins.
6. **Only protocol participants may overlap.** A jed binary that does not take these locks, or any
   other process that ignores advisory locks, is outside the safety boundary.

## 3. Stable coordination bundle

### 3.1 Identity and lifetime

Every file database has a persistent, non-data coordination directory beside it:

```text
<database-path>.lock/
  protocol-v2
  presence
  arrival
  owner          (the owner record, spec/pageservice/README.md §5.1)
  uid-<uid>/     (a 0700 per-user directory holding a serving owner's endpoint, README.md §5.2)
```

`protocol-v2` is an immutable format marker; `presence` and `arrival` are empty files whose **OS lock
state**, not contents or timestamps, is authoritative. The owner record and the endpoint never decide
an outcome: they tell a client where to connect and word its timeout error. jed creates missing bundle
entries safely before electing, never unlinks the bundle automatically, and backs up or replicates only
the database file. Keeping the bundle stable solves the inode-replacement problem: compaction may
atomically replace the database path without moving the locks to an unlinked old inode.

The durable database state remains one file: the bundle contains no database bytes or recovery facts,
and copying the `.jed` file to a new path is sufficient to copy the database. The bundle entries are
deliberately not auto-removed: unlink-and-recreate would let an opener on the old inode and an opener on
the new inode enter different lock domains.

Bundle creation is idempotent and no database byte is read until the marker and both lock files exist
and have been opened without following a final-component symlink. A process takes `arrival SH` before
its final marker validation, so a future protocol migration can exclude new openers by holding
`arrival EX`. A bundle may still hold the shared-mode marker `protocol-v1` and its stale `transition`,
`writer`, and `commit` files; they are ignored. Any other `protocol-v*` marker fails `0A000`. A
shared-mode binary sees `protocol-v2` as unknown and fails `0A000`, so the two never run on one file.

`open` resolves symlinks and keys the in-process registry by the normalized real path. Hard-linked
database files are rejected in coordinated modes (`55006` with detail): path-adjacent lock bundles
cannot prove that two hard-link names share an inode, and silently accepting them would split the lock
domain. External rename/replacement of a live database and deletion/replacement of its lock bundle are
unsupported non-cooperating filesystem mutations, just like a non-jed writer ignoring an advisory lock.

For `create`, the parent directory is resolved and the absent target is normalized as
`real_parent/basename`. Creation initializes the bundle and takes `presence EX` with the exclusive
acquisition loop (§4) before publishing the initial file. It then rechecks nonexistence and keeps the
temp-file + fsync + no-clobber atomic-rename recipe; the lock never depends on the temporary or final
database inode. A competing creator therefore serializes on the bundle and receives `58P02` after the
winner publishes instead of clobbering it. The creator is the owner.

The bundle is local-filesystem coordination. NFS/CIFS/network filesystems are unsupported unless the
caller uses `locking = none` and supplies an external coordinator.

The first coordinated open of a database without a bundle creates it. If the database is on a
read-only directory and the bundle is absent, open fails `58030` with remediation detail rather than
proceeding uncoordinated. `create` always leaves a complete bundle for later read-only opens.

### 3.2 Lock operations

Each file is locked as a whole in shared (`SH`) or exclusive (`EX`) mode:

| lock | holder | purpose |
|---|---|---|
| `presence` | the owner: EX for its whole tenure | ownership of the file |
| `arrival` | an opener: SH while it validates the marker | the protocol-migration barrier |

On Unix the primitive is `flock` on each dedicated file. On Windows it is `LockFileEx` over the same
whole-file range. Separate files avoid byte-range/OFD portability, keep Rust on safe `std::fs::File`
locking, keep Go pure (`syscall.Flock` / Windows syscall), and give Node one narrow native host
operation to bind. Every core on a given OS MUST use these same primitives.

A process-local registry rejects a second independent open of the same real path (`55006`) rather than
relying on platform-specific same-process lock ownership. Local callers share sessions from one
`Database`, which already supplies the intended in-process concurrency. The claim belongs to the handle
for its whole life, through every page-service role change. A handle inherited across `fork` is
invalid: every operation that can write verifies the opener PID and requires the child to reopen.
Ordinary spawn/exec paths inherit no bundle descriptors (`CLOEXEC`).

## 4. Ownership

- **Page service (`auto`).** The election at open and every re-election are
  [spec/pageservice/README.md](../pageservice/README.md) §6.1 and §6.5: try `presence EX` without
  waiting (unless the opener's preference is `never`, or a handoff pauses it); otherwise connect to the
  owner the record names; repeat until `file_lock_timeout_ms`, then `55006`.
- **Exclusive acquisition loop** (`exclusive`, `create`, and file attachments): poll `presence EX` with
  nonblocking tries until it succeeds or `file_lock_timeout_ms` passes (`55006`, with "owned by another
  user's process" or "owned by a non-serving process" from the owner record when it says so). A
  non-serving owner writes an owner record with the serving flag clear, so page-service openers word
  their timeout correctly, and removes it at close before releasing `presence`.
- **File attachments** are non-serving owners of their files until attachments join the page service
  (page-service.md §5.5; TODO.md): attaching a file that another process owns waits, then fails
  `55006`. A transaction still writes at most one durable database.

## 5. Transactions across processes

A page client's transactions run through the owner (page-service.md §5; README.md §6.3–§6.4): a read
begin pins the owner's snapshot with `begin_read`, a writer takes the owner's writer gate with
`begin_write`, and a commit is a validated `CommitDelta` the owner writes. The owner's own transactions
never refresh meta: every commit to the file passes through it.

## 6. Reclamation, compaction, and buffers

- **Free-page reuse, free-list persistence, and in-commit reclamation** run on the owner, gated by its
  reader watermark, which counts every client pin. Clients never reclaim: free-list rebuilds after
  client commits are owner maintenance commits (page-service.md §6).
- **Whole-file compaction** ([api.md](api.md) §2.6) is an owner operation: it requires the writer gate
  (never waited for) and a drained watermark (no local reader and no client pin), and fails `55006`
  otherwise; a client's compaction fails `55006`. The stable lock bundle stays locked while the new
  image is renamed into place; connected clients see a reset invalidation window and reload.
- **Trailing truncation** has the same rules.

## 7. Host/API surface

### 7.1 Options

File `create`, `open`, and `AttachSource::file` carry:

- `locking`: `auto` (default) | `exclusive` | `none`;
- `file_lock_timeout_ms`: nonnegative integer, default **5000**; `0` makes one attempt. It bounds the
  election and the exclusive acquisition loop, and a page client's re-election. Implementations poll
  nonblocking OS acquisition with a monotonic deadline so cancellation/timeout behavior is uniform.

The session setting `lock_timeout_ms` separately bounds the writer gate (local or the owner's); `0`
means no deadline, matching PostgreSQL's `lock_timeout` default. The page-service options
(`ownership`, the grace periods, the owner limits) are [api.md](api.md) §2.1.

File attachment sources own attachment options; the boolean attachment mode still decides read-only vs
read-write. The idiomatic spellings are Rust `AttachSource::file(path).locking(...)
.file_lock_timeout_ms(...)`, Go `AttachFileWithOptions(path, AttachFileOptions{...})`, and TypeScript
`attachFile(path, { locking, fileLockTimeoutMs })`. An in-memory create ignores file-lock options.

Go represents `file_lock_timeout_ms` as `*uint64`: `nil` is the 5000 ms default and a non-nil pointer
to `0` is the explicit single attempt.

### 7.2 Errors

- election or exclusive acquisition deadline: `55006 object_in_use`, with the real path in detail;
- writer-gate deadline: `55P03 lock_not_available`;
- requested protocol unavailable on the host: `0A000 feature_not_supported`;
- unknown coordination protocol marker: `0A000` with supported-version detail;
- unsupported OS/filesystem lock operation (`ENOSYS`/`EOPNOTSUPP` equivalent): `0A000`;
- other lock/bundle I/O failure: `58030 io_error`;
- hard-linked database in a coordinated mode: `55006` with remediation detail;
- page-service connection failures: `08006`, `08007`, `08P01`, `25P03`, `53300` (README.md §3, §6).

All acquired locks and descriptors are released on every failed open path. Timeout measurement and
polling are unmetered host work and never enter SQL cost.

### 7.3 Host capability matrix

| host | `auto` | mechanism / status |
|---|---|---|
| Rust local file, Unix | page service | `std::fs::File` whole-file locks (`flock`) and a Unix-socket endpoint; no dependency |
| Go local file, Unix | page service | `syscall.Flock` locks and a Unix-socket endpoint; pure Go, no dependency |
| Rust/Go local file, Windows | exclusive | `LockFileEx` whole-file locks; Windows serving is deferred (page-service.md §14) |
| Node/TS file, Unix | page service | the narrow `flock` adapter (§8); the service and the client socket run on worker threads |
| Node/Rust wrap | page service | inherits the Rust local-file host; experimental delivery choice, §8 |
| Browser/OPFS | exclusive | the sync access handle is inherently exclusive |
| wasm32-wasip1 | unavailable | `auto`/`exclusive` fail `0A000`; only explicit `none` proceeds. The `impl/wasm` C ABI takes the mode as an explicit `jed_open`/`jed_create` argument with no wrap default; the single-process benchmark harness passes `none` as its own external coordination |
| Ruby gem | page service | inherits the Rust host |
| in-memory | n/a | process-private; no coordination bundle |

## 8. TypeScript / Node delivery decision

Node exposes no standard `flock`/`LockFileEx` API. A PID sidecar, `mkdir`+mtime lease, or automatic
"stale" deletion cannot meet §2: PID reuse/namespaces and stop-the-world/event-loop pauses can let an
old owner resume after another process has stolen its lease. Those approaches are rejected.

Therefore a corruption-safe Node host needs native code. There were two honest delivery choices:

1. keep SQL/storage in the independent TypeScript core and add a **narrow native OS-lock adapter** over
   already-open coordination files; or
2. ship a Node package that wraps the whole Rust core and inherits Rust's lock host.

The narrow adapter's complete primitive surface is nonblocking `try_lock_shared`, nonblocking
`try_lock_exclusive`, and `unlock`, plus stable busy/unsupported/I/O error classification. Timeout and
cancellation loops stay in the language host, so no native call can become an uncancellable wait. It
performs no database I/O and cannot affect values, costs, or bytes, and an owner's foreground path never
calls it.

The full-wrapper alternative was built as `impl/node` before choosing. On the 2026-07-16 same-host run
all 159 pure-TS/wrapper/native-Rust results agreed on checksums. The wrapper was 1.38× faster by
geometric mean over 49 single-process lanes and 3.76× over the four concurrent lanes, but the 13
sub-100 µs lanes were effectively tied (0.98×; TypeScript won 8) and pure TypeScript was 2.04× faster
by geometric mean on the five write lanes ([benchmarks.md §7.3](benchmarks.md)).

**Decision:** use the narrow adapter for the TypeScript Node host, keeping independent TS conformance
and in-language host functions. Keep `impl/node` as an experimental reach artifact; wrapping Rust must
not silently count as an independent TypeScript conformance result.

The exact dependencies approved for the narrow production host and the separate experiment are
`napi = 3.10.5`, `napi-derive = 3.5.10`, and build-only `napi-build = 2.3.2`, all exact-pinned with
transitive lockfiles committed. The remaining production distribution matrix is Linux glibc
x64/arm64, Linux musl x64/arm64, macOS x64/arm64, and Windows x64, built against stable Node-API 8.
Artifacts ship inside the package with a SHA-256 manifest and provenance; installation runs no script
and downloads nothing. A platform without a matching artifact fails coordinated modes closed with
`0A000`. The bounded Node-API host exception is recorded in CLAUDE.md §14. The browser/OPFS bundle
never imports the addon.

## 9. Verification contract

This surface is host state, so the release-blocking real-process corpus
(`spec/conformance/process/`, `mise run concurrency:process`, part of `mise run ci`) is the authority:
election, reads and writes through the service, promotion, yield, graceful close, owner and client
kills, commit resolution, owner limits, exclusive owners, and owner-only file attachments, in every
Rust/Go/Node pairing ([concurrency-testing.md](concurrency-testing.md) §10). Unknown-marker failure,
hard-link rejection, and symlink identity stay per-core unit tests. Simulation testing of an owner and
its clients is [simulation-testing.md](simulation-testing.md).
