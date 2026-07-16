# File locking & shared multi-process access — design

> What happens when more than one process opens the same database file. This document fixes the
> **shared-by-default protocol**: every lock-capable file host may admit multiple processes, while the
> engine preserves one global writer, immutable pinned snapshots, crash-safe publication, and
> corruption-free reclamation. The common one-process case holds an OS-backed fast-path lease and
> performs **no locking or meta-read work on foreground transaction paths**. Contention deliberately
> takes the slower path. [hosts.md](hosts.md) owns the host backing; [api.md](api.md) §2.1 owns the
> options; [storage.md](storage.md) §4/§6 own persistence and reclamation. When a decision here changes,
> update those docs, [CLAUDE.md](../../CLAUDE.md) §9, [AGENTS.md](../../AGENTS.md), and
> [TODO.md](../../TODO.md) together.

## 1. Decision and status

The engine currently coordinates concurrent sessions only when they share one in-process `Database`.
Separate opens have separate buffer pools, free-list state, reader watermarks, and writer gates. Without
cross-process coordination, two handles can reuse the same page, overwrite a page cached by the other,
or publish meta slots from different bases. That is undefined corruption today.

**Decision (revised 2026-07-16): build shared multi-process access as the first locking slice.**

- `locking = auto` is the zero-value/default. On a capable local file host it selects `shared`; on a
  host whose storage primitive is inherently exclusive (OPFS), it selects `exclusive`.
- `shared` admits concurrent processes and enforces **one writer globally**. Readers keep pinned
  snapshots and block only during the short meta-publication window, not for the writer's transaction.
- `exclusive` keeps one process on the file and never opens a join window. It remains useful for a
  caller that wants immediate contention errors or cannot tolerate a coordination sidecar.
- `none` skips jed coordination. It is an unsafe expert escape hatch for a host that enforces the same
  invariant externally; the caller owns every consequence.

The earlier exclusive-first proposal is superseded. The motivating deploy no longer has to wait for
the old process to close: old and new processes may overlap, with writes serialized by the protocol.

The protocol is designed against the **current format (v29)**. On-disk free-list persistence and
continuous within-session reclamation already landed in v25. No format bump and no persisted
`catalog_gen` are required for correctness: after a foreign `txid`, the deliberately-slow contended path
reloads the snapshot and invalidates persistent plan caches wholesale. A persistent catalog generation
is only a later multi-process performance optimization.

## 2. Safety invariants

These are normative. An implementation that cannot uphold them fails closed (`0A000`); it never falls
back to a PID file, mtime lease, or best-effort stale-lock recovery.

1. **Join before read.** A process becomes a participant before reading a meta slot or body page.
2. **One global writer.** A write transaction owns the cross-process writer gate from snapshot refresh
   through commit/rollback.
3. **Publication is gated.** A transaction begin cannot adopt a meta slot while a writer is publishing
   it. The visible cross-process commit point remains the completed meta publication.
4. **Pages are immutable while co-resident.** A shared-mode commit is append-only: no free-page reuse,
   no truncation, no free-list rewrite, and no compaction while another process may have a snapshot.
5. **Destructive storage work requires proof of aloneness.** Reuse, free-list persistence/rebuild,
   truncation, and whole-file replacement require the presence-exclusive lease **and** the existing
   in-process reader watermark.
6. **The OS lock is the authority.** Leases never expire and are never stolen. Process death releases
   the locks. Wall-clock time controls only how long a caller waits, never ownership.
7. **The uncontended foreground path is unchanged.** Once one process holds the presence-exclusive
   lease, reads do not pread meta, writes do not acquire OS gates, and commits use the existing v29
   allocator and durability recipe.
8. **Only protocol participants may overlap.** A pre-protocol jed binary or any other process that
   ignores advisory locks is outside the safety boundary. The first rollout must drain every such
   opener before a protocol-aware process starts; later compatible versions may overlap normally.

## 3. Stable coordination bundle

### 3.1 Identity and lifetime

Every file database has a persistent, non-data coordination directory beside it:

```text
<database-path>.lock/
  protocol-v1
  presence
  arrival
  transition
  writer
  commit
```

The files are empty. `protocol-v1` is an immutable format marker; the other five files' **OS lock
state**, not contents or timestamps, is authoritative. jed creates missing bundle entries safely
before joining, never unlinks the bundle automatically, and backs up or replicates only the database
file. Keeping the bundle stable solves the inode-replacement problem: `to_image` compaction may
atomically replace the database path without moving the locks to an unlinked old inode.

The durable database state remains one file: the bundle contains no database bytes, recovery facts,
owner identity, or generation counter, and copying the `.jed` file to a new path is sufficient to copy
the database. The tradeoff is one version marker and five persistent empty local lock files beside an
opened database. They are deliberately not auto-removed: unlink-and-recreate would let an opener on the
old inode and an opener on the new inode enter different lock domains.

Bundle creation is idempotent and no database byte is read until the marker and all five regular lock
files exist and have been opened without following a final-component symlink. A process takes
`arrival` before its final marker validation, so a future protocol migration can exclude new joiners.
An unknown `protocol-v*` marker fails `0A000`. An incompatible future version must migrate only while
holding `arrival EX`, `transition EX`, and `presence EX`, and must continue honoring the v1 lock names;
it must never create a second lock domain. A v1 opener revalidates the marker after acquiring
`arrival SH`.

`open` resolves symlinks and keys the in-process coordinator registry by the normalized real path.
Hard-linked database files are rejected in coordinated modes (`55006` with detail): path-adjacent lock
bundles cannot prove that two hard-link names share an inode, and silently accepting them would split
the lock domain. External rename/replacement of a live database and deletion/replacement of its lock
bundle are unsupported non-cooperating filesystem mutations, just like a non-jed writer ignoring an
advisory Unix lock.

For `create`, the parent directory is resolved and the absent target is normalized as
`real_parent/basename`. Creation initializes the bundle and uses the **exclusive acquisition loop**
(§4.1) before publishing the initial file. It then rechecks nonexistence and keeps the existing
temp-file + fsync + no-clobber atomic-rename recipe; the lock never depends on the temporary or final
database inode. A competing creator therefore serializes on the bundle and receives `58P02` after the
winner publishes instead of clobbering it. After creation, `auto` may enter the ordinary shared
fast-path lease without releasing presence.

The bundle is local-filesystem coordination. NFS/CIFS/network filesystems are unsupported unless the
caller uses `locking = none` and supplies an external coordinator.

The first coordinated open of a pre-locking database may need to create the bundle. If the database is
on a read-only directory and the bundle is absent, open fails `58030` with remediation detail rather
than proceeding uncoordinated. `create` always leaves a complete bundle for later read-only opens.

### 3.2 Lock operations

Each file is locked as a whole in shared (`SH`) or exclusive (`EX`) mode:

| lock | long-lived owner | purpose |
|---|---|---|
| `presence` | every participant: normally SH; the alone lease: EX | participant liveness and proof of aloneness |
| `arrival` | a joining process: SH until it obtains presence | crash-clean notification that an EX lease must open a join window |
| `transition` | one process briefly: EX | serializes presence SH↔EX transitions |
| `writer` | a contended write transaction: EX | the global single-writer gate |
| `commit` | txn begin: SH briefly; publishing writer: EX briefly | prevents a begin from observing an in-progress meta publication |

On Unix the primitive is `flock` on each dedicated file. On Windows it is `LockFileEx` over the same
whole-file range. Separate files avoid byte-range/OFD portability, keep Rust on safe `std::fs::File`
locking, keep Go pure (`syscall.Flock` / Windows syscall), and give Node one narrow native host
operation to bind. Every core on a given OS MUST use these same primitives.

The bundle is opened once per database handle and held until `close`; open failures close every acquired
file. A process-local registry rejects a second independent open of the same real path (`55006`) rather
than relying on platform-specific same-process lock ownership. Local callers share sessions from one
`Database`, which already supplies the intended in-process concurrency. A handle inherited across
`fork` is invalid: every operation verifies the opener PID, closes the child's inherited descriptor
copies, clears the inherited registry entry, and requires the child to reopen. Fork-without-exec code
must not retain an inherited handle indefinitely; ordinary spawn/exec paths inherit no bundle
descriptors (`CLOEXEC`).

## 4. The uncontended lease

The “lease” is a performance state, not a time lease. It is backed continuously by `presence EX`; it
cannot expire or be stolen. The periodic work only gives newcomers a bounded opportunity to join.

### 4.1 Join

A shared opener, before reading the database:

1. takes `arrival SH`;
2. waits for `presence SH` (bounded by the open timeout);
3. releases `arrival SH`;
4. takes `commit SH`, reads and validates both meta slots, adopts the newest committed root, then
   releases `commit SH`.

If an existing process holds `presence EX`, step 2 waits safely. Its held `arrival SH` is the
crash-clean doorbell the lease holder observes. If the opener dies, the OS removes both locks.

An **exclusive** opener also holds `arrival SH`, but it must not wait on `presence EX` while holding
`transition`: that would prevent an alone shared holder from downgrading. Instead it loops: take
`transition EX`, try `presence EX` once, release `transition` on conflict, and wait before retrying.
Holding `transition` only for the nonblocking attempt prevents it from stealing the temporary
presence-unlocked gap of another process's SH↔EX conversion. It releases `arrival` only after it owns
`presence EX`. Exclusive mode then never runs the background join-window probe.

### 4.2 Alone → shared

An alone coordinator polls in the background. Foreground queries and commits do not poll. The
no-arrival poll touches only the separate `arrival` OS lock; it does not take the local writer gate or
reader registry. Polling backs off aggressively while no joiner appears (the target steady state is at
most one probe per second; exact intervals are non-normative) and resets to a short interval after a
join/leave transition. The resulting join delay is an intentional multi-process performance tradeoff.

1. It tries `arrival EX`. If that succeeds, no joiner is waiting; it releases it and remains alone.
2. On conflict, it takes the **local transition barrier**: the existing local writer gate plus the
   existing reader-pin registry lock. This prevents a local writer or transaction begin from
   straddling the mode change; already-pinned readers continue normally.
3. It takes `transition EX` and retries `arrival EX` (the original joiner may have timed out).
4. If the retry succeeds, it releases the locks and remains alone.
5. If it still conflicts, it marks the local coordinator `shared`, then changes
   `presence EX → SH`, and releases `transition` and the local barrier. Waiting joiners acquire
   `presence SH` and proceed.

The conversion need not be atomic. `transition EX` prevents another participant from making itself
temporarily invisible during a competing conversion; the waiting joiner's `arrival SH` keeps the
holder from concluding that nobody is waiting. A read that pinned the local root before the barrier
remains safe because every later co-resident commit is append-only; a begin after it sees `shared` and
refreshes meta. The writer half of the barrier prevents two fast-path writers from straddling the
downgrade. Because pinning and the mode check share the already-existing registry lock, the alone path
adds no new foreground synchronization primitive.

### 4.3 Shared → alone

Shared coordinators periodically try to regain the fast path:

1. take the local transition barrier (writer gate + reader-pin registry lock);
2. take `transition EX`;
3. try `arrival EX`; if it conflicts, stop and remain shared;
4. while holding `arrival EX`, release this process's `presence SH` and try `presence EX`;
5. on conflict, reacquire `presence SH` before releasing the other locks;
6. on success, refresh the newest meta/root under `commit SH`, reconcile pager high-water and
   invalidate persistent plan caches if `txid` advanced, then mark the coordinator `alone` and release
   the local barrier.

Holding `arrival EX` prevents an entrant from becoming invisible between the SH release and EX
acquire. `transition EX` prevents two existing participants from dropping their SH locks at once. A
successful `presence EX` is therefore a real proof that no foreign process has a pinned snapshot.

## 5. Transactions while shared

### 5.1 Read begin

An alone process pins its current in-process committed snapshot exactly as today. A shared process:

1. takes `commit SH`;
2. positioned-reads both meta slots directly (never through the buffer pool), validates CRCs, and picks
   the highest valid `txid`;
3. if `txid` advanced, reloads the catalog/interior skeleton, updates pager `page_count`, and
   invalidates persistent plans for that database;
4. pins that snapshot in the existing in-process reader registry;
5. releases `commit SH`.

The query then runs lock-free over the pin. `commit SH` protects only the meta adoption, not the query.
Since co-resident writers never overwrite body pages, the referenced pages remain immutable.

### 5.2 Write begin and rollback

An alone write uses only the existing local writer gate. A shared write holds `writer EX` for the whole
transaction. After acquiring it, and before making the working snapshot, it performs the same
`commit SH` meta refresh as a read begin. This produces one global serialization order for writers with
no merge/retry semantics. Rollback discards staging and releases `writer EX`.

Writer-gate waiting uses the session `lock_timeout_ms` (default `0` = wait without a deadline) and
fails `55P03 lock_not_available` on expiry. Waiting is host work and uses a monotonic clock; it is not
part of deterministic SQL cost.

### 5.3 Append-only shared commit

While `presence` is SH, a writer must use the deliberately conservative path:

- allocate every dirty tree/catalog/overflow/index page at or beyond the latest committed
  `page_count`; never consume `free_pages`;
- do not rebuild or rewrite the v25 free-list chain;
- publish the new meta with `free_list_head = 0`; old free-list and orphan pages remain available to
  the fallback meta or become reclaimable when a process is later proven alone;
- never truncate or replace the file.

The writer may write and sync its appended body pages without `commit EX`. For the short publish
window it takes `commit EX`, writes and syncs the alternate meta slot, publishes the same snapshot to
its local core, then releases `commit EX` and `writer EX`. A begin that won `commit SH` first adopts the
old meta; one that wins afterward adopts the new meta. This preserves the existing “readers block only
during commit” model across processes.

An I/O error after meta write has an indeterminate commit outcome, as with any durable database. The
handle becomes poisoned and retains its gates until close; recovery on the next open chooses the
highest CRC-valid meta. It must never continue writing from an assumed prior root.

## 6. Reclamation, compaction, and buffers

- **Free-page reuse and free-list persistence** require `presence EX` plus the existing local
  watermark. After a co-resident interval, the first alone commit reconstructs/replans the free list
  from the current committed root and lets the watermark defer reuse of pages reachable by local old
  pins. Shared-mode file growth is intentional; aloneness recovers it.
- **Buffer pools remain valid while shared** because foreign processes append body pages. Meta is read
  directly. When an alone writer later reuses a page, the ordinary commit invalidation removes any
  stale local decode of that page before the new root is published.
- **Whole-file compaction** requires `presence EX`, the local watermark drained, and the local writer
  gate. The stable lock bundle remains locked while the database file handle is closed, a fresh image
  is atomically renamed into place, and the pager reopens the replacement. A new process cannot join
  through the rename because `presence EX` lives on the unchanged bundle.
- **Trailing truncation** has the same aloneness rule. Shared commits never lower `page_count`.
- **Attachments** coordinate independently per file. The existing one-durable-writer-per-transaction
  rule remains until the super-journal slice; acquiring several file writer gates is not introduced by
  this protocol.

## 7. Host/API surface

### 7.1 Options

File `create`, `open`, and `AttachSource::file` carry:

- `locking`: `auto` (default) | `shared` | `exclusive` | `none`;
- `file_lock_timeout_ms`: nonnegative integer, default **5000**; `0` fails immediately. It bounds join
  and exclusive-open acquisition only. Implementations poll nonblocking OS acquisition with a
  monotonic deadline so cancellation/timeout behavior is uniform.

The session setting `lock_timeout_ms` separately bounds the shared writer gate; `0` means no deadline,
matching PostgreSQL's `lock_timeout` default. These names deliberately distinguish open/join liveness
from transaction lock waiting.

`AttachSource::file(path, options)` owns attachment options; the boolean attachment mode still decides
read-only vs read-write. `create` stores only applicable file-lock options from `CreateOptions`; an
in-memory create ignores them.

### 7.2 Errors

- open/join or exclusive acquisition deadline: `55006 object_in_use`, with the real path in detail;
- writer-gate deadline: `55P03 lock_not_available` (registered with this slice);
- requested protocol unavailable on the host: `0A000 feature_not_supported`;
- unknown coordination protocol marker: `0A000` with supported-version detail;
- unsupported OS/filesystem lock operation (`ENOSYS`/`EOPNOTSUPP` equivalent): `0A000`;
- other lock/bundle I/O failure: `58030 io_error`;
- hard-linked database in a coordinated mode: `55006` with remediation detail.

All acquired locks and descriptors are released on every failed open path. Timeout measurement and
background lease polling are unmetered host work and never enter SQL cost.

### 7.3 Host capability matrix

| host | `auto` | mechanism / status |
|---|---|---|
| Rust local file, Unix | shared | five `std::fs::File` whole-file locks (`flock`); no dependency |
| Go local file, Unix | shared | five `syscall.Flock` locks; pure Go, no dependency |
| Rust/Go local file, Windows | shared | `LockFileEx` whole-file locks; Rust std / pure Go syscall |
| Node file | shared when native lock host is installed | the exact Unix `flock` / Windows `LockFileEx` bundle; §8 |
| Browser/OPFS | exclusive | the sync access handle is inherently exclusive; explicit `shared` is `0A000` |
| wasm32-wasip1 | unavailable | `auto`/`shared`/`exclusive` fail `0A000`; only explicit `none` proceeds |
| Ruby gem | shared | inherits the Rust host |
| in-memory | n/a | process-private; no coordination bundle |

## 8. TypeScript / Node decision gate

Node v26 still exposes no standard `flock`/`LockFileEx` API. A PID sidecar, `mkdir`+mtime lease, or
automatic “stale” deletion cannot meet §2: PID reuse/namespaces and stop-the-world/event-loop pauses can
let an old owner resume after another process has stolen its lease. Adding fencing would require an
atomic compare-and-swap primitive the filesystem API does not provide. Those approaches are rejected.

Therefore a corruption-safe Node shared host requires a **narrow native OS-lock adapter** over an
already-open coordination file. Its complete primitive surface is nonblocking `try_lock_shared`,
nonblocking `try_lock_exclusive`, and `unlock`, plus stable busy/unsupported/I/O error classification.
Timeout and cancellation loops stay in the language host, so no native call can become an
uncancellable wait. The adapter performs no database I/O and cannot affect values, costs, or bytes.

The preferred direction is a **first-party minimal Rust Node-API addon** whose source lives with jed,
owns only coordination-file handles, and delegates locking to safe `std::fs::File` methods. It is a
host adapter, not a wrapper around the Rust database core; SQL/storage behavior remains independently
implemented in TypeScript. Stable Node-API plus reproducible prebuilt artifacts preserve the package's
no-consumer-build-step goal.

The dependency candidate, audited on 2026-07-16 but **not approved or added**, is
`napi = 3.10.5`, `napi-derive = 3.5.10`, and build-only `napi-build = 2.3.2`, all exact-pinned with the
transitive lockfile committed. No npm build CLI is needed; Rake drives the ordinary Cargo builds. The
proposed initial artifact matrix is Linux glibc x64/arm64, Linux musl x64/arm64, macOS x64/arm64, and
Windows x64, built against stable Node-API 8. Artifacts ship inside the package with a SHA-256 manifest
and provenance; installation runs no script and downloads nothing. A platform without a matching
artifact fails closed. `fs-ext-extra-prebuilt@2.2.9` was also evaluated and is not recommended: it uses
NAN rather than stable Node-API, runs an install script, ships a much broader filesystem surface, and
has a narrow maintainer base.

The Node file host loads the addon conditionally; browser/OPFS bundles never import it. The alone probe
uses an unreferenced timer so an idle database does not keep the process alive. Because jed's current
Node embedding API is synchronous, a contended open or writer wait blocks the calling thread (and hence
the event loop if called there) while the host repeats nonblocking attempts with a monotonic deadline.
That does not weaken safety, but it is a real ergonomics cost: applications that permit long
cross-process write transactions should run jed in a worker thread or choose finite timeouts. An async
embedding surface is a separate API slice, not hidden inside the lock protocol.

This is an explicit architecture/dependency gate: CLAUDE.md §14 currently forbids dependencies that
introduce FFI/unsafe code. Before the Node slice, a human must approve both (a) a bounded exception for
this host-only Node-API adapter and (b) the exact versions and artifact matrix above. If that exception
is rejected, Node `shared` fails closed with `0A000`; it does not silently weaken the protocol. OPFS
remains inherently exclusive and does not need the adapter.

## 9. Verification contract

This surface is host state, so focused per-core tests are necessary, but a real-process suite is
**required**, not optional.

### 9.1 Deterministic protocol tests

- modeled state-machine tests for join, downgrade, upgrade, reader begin, writer handoff, timeouts, and
  every acquisition failure cleanup;
- legacy first-rollout and unknown-marker tests prove the implementation fails closed rather than
  claiming coordination with a nonparticipant;
- a joining process holds `arrival` while blocked and cannot be starved by an alone lease;
- two transition attempts cannot both become presence-EX;
- meta refresh invalidates plans only after a foreign `txid` and adopts the correct page high-water;
- shared commits allocate append-only and write `free_list_head = 0`; later alone reclamation recovers
  the orphans without touching a pinned local snapshot;
- hard links fail and symlink aliases share one coordinator identity.

### 9.2 Cross-process/interoperability tests

- Rust↔Go, Rust↔Node, and Go↔Node join/read/write handoff over one file on every supported OS lane;
- concurrent readers plus serialized writers produce a confluent final checksum;
- kill a participant at each join/write/body-sync/meta-publish phase; surviving processes either
  continue from the old meta or recover the new valid meta, never corrupt;
- process death releases presence/writer/commit locks without stale recovery;
- compaction replacement while alone retains exclusion through the stable bundle;
- real timeout and cancellation paths use the documented SQLSTATEs.

### 9.3 Fast-path performance gate

The resident single-process benchmarks run before/after. Once presence-EX is acquired, instrumentation
must show **zero foreground coordination syscalls and zero per-transaction meta preads**. Point lookup,
short read transaction, and durable commit throughput must remain within the repository's normal noise
threshold; a regression blocks the slice. Background lease-probe CPU/syscall rate is reported
separately and bounded by the polling interval.

## 10. Implementation slices

1. **Coordinator foundation (all native cores):** lock bundle, canonical identity/hard-link rejection,
   modes/options/errors, join/close, local duplicate-open guard, and the lease state machine. Node uses
   a test fake until §8's decision is approved; its real `shared` path fails closed.
2. **Shared reads and global writer:** commit gate, direct meta freshness, full snapshot/plan invalidation,
   writer gate + `55P03`, and concurrency tests.
3. **Append-only shared commit:** no-reuse allocator mode, zero free-list head, publish gate, crash/fault
   matrix, and alone recovery/reclamation.
4. **Node OS-lock host:** only after the explicit §8 approval; add Rust↔Go↔Node real-process lanes.
5. **Compaction integration and public docs:** stable-bundle handoff for the later compaction slice;
   update website examples/status when shared locking becomes user-visible.

The shared feature is not “landed” until slices 1–4 are green together. Rust/Go-only shared behavior is
an intermediate branch state, not a released cross-core capability.
