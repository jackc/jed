# Page service — one owner process, page-level clients — design

> **Status: DESIGN PROPOSAL — not ratified, not built.** This document proposes replacing the
> shared multi-process protocol ([locking.md](locking.md)) with a single **owner** process that alone
> touches the database file and serves **pages** to every other process over a local endpoint, and
> extending the same service over SSH for remote access, backup, and replication. Nothing here binds
> an implementation until it is ratified. When it is, update [locking.md](locking.md),
> [replication.md](replication.md), [hosts.md](hosts.md) §6, [api.md](api.md) §2.1,
> [CLAUDE.md](../../CLAUDE.md) §9, and [TODO.md](../../TODO.md) together.

## 1. What this is for

jed is embedded and stays embedded: no listening network port, no auth system, no daemon to run. The
use cases this design serves are all "more than one process wants the database" without a server:

1. **Interactive access to a database on another machine** from a workstation.
2. **Extraction** — run a query against a remote database and keep the result (e.g. CSV).
3. **Remote backup** — take a consistent copy of a remote database.
4. **Replication** — continuous off-host copies, point-in-time recovery, standbys, read replicas.
5. **Program access** — a program reaches a remote database (e.g. over SSH) without a server.
   Performance is secondary, but easy wins must not be missed.
6. **Overlapping local processes** — a blue-green or zero-downtime deploy runs the old and new
   application processes against one file for a brief window, and both must be able to write.

Many concurrent remote application servers using jed like PostgreSQL is **not** a goal. A deployment
that needs that should use PostgreSQL.

## 2. Decision summary

- **One owner per database file.** The owner holds an exclusive OS lock in the existing coordination
  bundle and is the only process that reads or writes the database's bytes. It runs its own engine
  directly on the file, on today's single-process fast path.
- **Every other process is a page client.** A client runs its **own engine** — its own parser,
  planner, executor, environment, and jed version — but its pager reads pages from, and commits
  through, the owner's **page service** instead of a file.
- **The page service sits at the pager/commit boundary**, not at the five-method `BlockStore` seam.
  It speaks snapshots, pins, the writer gate, page reads, and whole-commit deltas.
- **A client's commit is a `CommitDelta`** ([replication.md](replication.md) §2): the dirty pages,
  manifest, and meta its own engine produced. The owner validates, writes, syncs, and publishes it.
  Because every commit from every process passes through the owner, the replication log is the
  owner's commit stream, complete and totally ordered by construction.
- **The transport is a local stream socket** for same-host clients and **SSH stdio** for remote
  ones. It is the same protocol on both.
- **Ownership hands off** when the owner exits: a waiting client promotes itself to owner between
  transactions. Blue-green is exactly this.

## 3. Alternatives considered

| Approach | Why not |
|---|---|
| **Keep shared mode** ([locking.md](locking.md)) | Co-resident commits are append-only and free-page reuse, truncation, and compaction stop while more than one process is open; a long-lived second process (a remote helper, a monitoring tool) keeps the file growing. Replication from several writing processes needs every writer to append to a common log, with gaps on crash. |
| **Query-level relay** (clients send SQL, the owner executes) | Queries run in the owner's `Environment` and engine version. A client's host functions are missing or run with the owner's behavior; Unicode/time-zone data, custom clocks, and seeded entropy are the owner's; during a deploy overlap the new application runs on the old jed version, which forces a two-step deploy discipline for new SQL features. Patching each piece (callbacks, data upload) never reaches the engine version. It also gives up per-process resource isolation. |
| **Coordinator with direct file reads** (owner tracks pins and the writer gate; clients `pread` the file) | Fixes the environment problem but keeps two processes touching one file. When the coordinator dies, a client may still be reading pages the next coordinator could reuse; closing that race needs per-incarnation generation locks and an append-only interregnum. |
| **Proxy the five-method `BlockStore`** over a socket | The seam has no transactions. Each client's pager would allocate and publish independently: the multi-process problem moved onto a pipe. |
| **A write-ahead log** | Copy-on-write plus the meta swap already provide atomic durable commits and non-blocking readers. A WAL would add a second file at rest, checkpointing, and (for multiple processes) a shared-memory index, for the one benefit, a single ordered change history, that the owner's commit stream provides here. |

The page service keeps the relay's clean property (one process touches the file) and the
coordinator's (each process runs its own engine), and turns out to be the same mechanism replication
needs. Neon's pageserver and Amazon Aurora are architectural precedent (full engines fetching pages
from a page service), though both are servers.

## 4. Roles and election

### 4.1 Owner

- Holds `presence EX` in the coordination bundle ([locking.md](locking.md) §3) for its whole tenure.
  The OS lock is the only authority: ownership never expires and is never stolen, and process death
  releases it (locking.md §2 invariant 6, unchanged).
- Opens the database file, runs recovery, and serves its own sessions exactly as an alone process
  does today. Its foreground path performs no coordination work.
- If serving (§11.1), binds the endpoint and answers page-service requests on owner-side workers
  (goroutines in Go, threads in Rust). A client request never blocks the owner's own readers; the
  writer gate is shared by owner sessions and client transactions.

### 4.2 Client

- Never opens the database file. Its pager is backed by a page-service connection.
- Holds no bundle lock while connected. Its liveness is the connection: EOF releases everything it
  held (pins, the writer gate).

### 4.3 Election at open

`open(path)` with a coordinated `locking` mode:

1. Prepare the bundle as today (marker validation, the `arrival` doorbell for v1 exclusion, §12).
2. Try `presence EX` without waiting. On success, become the owner (§4.1).
3. Otherwise connect to the endpoint (§10.1). If the endpoint is absent or refuses because the owner
   has not finished starting, retry until `file_lock_timeout_ms` expires, then fail `55006` with the
   path in detail. An owner that runs with `serve = false` answers nothing; the opener fails `55006`
   with "owned by a non-serving process".
4. A stale endpoint left by a crashed owner is harmless: only a `presence EX` holder may replace it.

A client that is not configured `ownership = never` (§8.2) also waits on `presence EX` in the
background, so it can promote when the owner leaves.

## 5. The page service

### 5.1 Where it sits

```
client engine (planner / executor)            owner engine
  ↓                                              ↓
pager: buffer pool, page math, lazy decode      pager
  ↓ plaintext pages                              ↓
encryption codec (client holds the key)         encryption codec
  ↓ ciphertext pages                             ↓
page-service client  ── socket / SSH stdio ──►  page service ──► replication tee ──► BlockStore (file)
```

Pages cross the service in their **on-disk (ciphertext) form**, below the encryption codec. A client
decrypts and encrypts with its own key; the owner's file and replication tee see exactly the bytes a
single-process commit would write. A **keyless owner** — a pure page server that runs no queries of
its own, such as an SSH helper or a backup host — is therefore possible (§13).

### 5.2 Operations

All requests carry a request id; a client may pipeline. Responses return in request order.

| Operation | Effect |
|---|---|
| `hello(versions, format_version, page_size, caps)` | Handshake: pick a protocol version; confirm the client supports the file's `format_version` and page size; report connection limits (read-only, §11.2). |
| `begin_read(last_txid)` | Pin the owner's current committed snapshot. Returns the meta (root, `txid`, `page_count`, free-list head, live count) and the **invalidation set**: page indices written by commits after `last_txid` (§5.4). |
| `read_pages(txid, [index…])` | Return page bytes. `txid` must be pinned by this connection. Batched. |
| `end_read(txid)` | Release one pin. Asynchronous: no reply. |
| `begin_write(last_txid)` | Acquire the writer gate (§5.5), then the same refresh as `begin_read`, plus the **reuse grant** (§6). |
| `commit(CommitDelta)` | Owner validates and persists (§5.6). Reply: the new `txid` once durable. |
| `abort()` | Release the writer gate; discard any received delta pages. |
| `commit_status(txid, manifest_digest)` | Resolve an unacknowledged commit after a lost connection (§8.4). |
| `snapshot_stream(txid, form)` | Stream a pinned snapshot for backup (§9). |
| `subscribe(from_txid)` | Stream retained commit deltas, then the live tail (§10). |
| `request_ownership()` | Ask a yielding owner to step aside (§8.2). |

The message catalog, framing, and field encodings are data-shaped and belong in a shared spec table
(`spec/pageservice/`), hand-written per core like the file format and verified by byte fixtures.

### 5.3 Read path

A read transaction is one round trip: `begin_read`, then queries run over the pin, fetching pages only
on a cache miss. `end_read` needs no reply. This keeps today's guarantee that a transaction begun
after another process's commit returned sees that commit (locking.md §5.1), the same freshness the
shared protocol gets from its meta re-read.

When the returned `txid` advanced, the client reloads its catalog/interior skeleton and invalidates
persistent plan caches for that database, as shared mode does today on a foreign `txid`.

Fetches are batched. The client pager requests read-ahead on leaf chains (the next K leaves named by
an interior already in hand) and, for scans, can request every page under a subtree root in one
message. Read-ahead is a pager policy, invisible to cost: page reads remain logical units
([cost.md](cost.md)), so a query's cost is identical through the service and on a local file.

### 5.4 Client cache coherence

Under copy-on-write a page index is written only when it is allocated (fresh from the high-water or
reused from the free list) or when it is a free-list chain page rewritten in place. Every such write
is listed in that commit's manifest. So the set of indices written after `last_txid` is exactly the
set whose cached contents may be stale, and dropping just those keeps a client cache correct.

The owner retains a bounded window of per-commit written-index sets. A client further behind than the
window receives "invalidate all". Steady-state reads from a warm client cache cost no IPC beyond the
`begin_read` round trip.

### 5.5 Writer gate

The owner's existing in-process writer gate is the single global writer gate. `begin_write` waits on
it under the session `lock_timeout_ms` (`55P03` on expiry, as today). A client holds it from
`begin_write` until its `commit` is acknowledged or it aborts. The owner enforces an
idle-in-transaction timeout on client write transactions (§11.2); a revoked transaction fails
`25P03 idle_in_transaction_session_timeout` at its next request. A client whose connection drops
while holding the gate is aborted at EOF.

### 5.6 Commit

The client builds the `CommitDelta` with the ordinary v33 recipe ([validated-cow.md](validated-cow.md)):
allocation under the reuse grant (§6), dirty body pages in deterministic order, free-list pages,
manifest, and the alternate meta. Large deltas stream in chunks; the owner may write body pages as
they arrive, because every target page is either beyond the committed high-water or a granted free
page, so none is reachable from any pinned snapshot or the fallback meta.

**Owner validation** (cheap, structural; it catches client bugs, not malice — §13):

- `txid == committed + 1`, `page_size` and `format_version` match;
- every written index is beyond the previous `page_count`, in the granted reusable set, or a
  free-list chain page the grant permits rewriting;
- the manifest covers exactly the written pages and their CRC-64 identities match;
- the meta's CRC and bounds are valid.

Then, holding the gate: write the meta, execute the single durable barrier, adopt the new committed
snapshot locally (reload the skeleton; drop the owner's buffer-pool entries for written indices),
append the delta to the replication tee, release the gate, and acknowledge. A write or sync failure
poisons the owner's storage handle exactly as it does today (locking.md §5.3): the owner stops serving
and closes, and the commit's outcome is resolved by recovery under the next owner (§8.4).

## 6. Page reuse and reclamation

Today's reuse rule is in-process: a commit may reuse the free list only when
`oldest_live_txid ≥ free_gen_txid` ([transactions.md](transactions.md) §8). The owner sees every pin
— its own sessions' and every client's — so it computes the same `oldest_live_txid` over the whole
database. **No format change is needed**: the persisted free list and the generation gate already
carry everything required.

- **The reuse grant.** `begin_write` returns whether the gate passes (`oldest_live_txid ≥
  free_gen_txid`) for the snapshot the writer starts from. With it, the client allocator uses the
  persisted free list exactly as an alone process would; without it, the client allocates from the
  high-water, as a deferred in-process commit does today. Pins taken after `begin_write` are on the
  writer's base snapshot or later, so they cannot invalidate the grant.
- **Free-list rebuilds are owner-only (proposed).** The periodic in-commit reclamation (the
  reachability walk once the high-water passes about twice the live count, storage.md §6) is O(live
  pages). Run by a remote client it would fetch the whole database. Instead a client commit never
  rebuilds; when a client commit crosses the rebuild threshold the owner runs a storage-maintenance
  commit immediately after it, under the same gate. This makes a file's page layout depend on which
  process committed, which is fine for the cross-core contract (each role's rule is specified and
  identical in every core) but must be modeled by simulation testing (§15). Open question §17.3.
- **Compaction** is an ordinary owner operation: it requires the owner's watermark drained (now
  including client pins) and the writer gate, and fails `55006` rather than waiting, as today. After
  the rename, clients' next `begin_read` receives "invalidate all"; a pinned client blocks compaction.

Shared mode's append-only steady state disappears. Its bounded-growth argument becomes the alone
argument for every process count.

## 7. Environment and versions

Each process runs its own engine, so every query runs in the `Environment` ([environment.md](environment.md))
of the process that issued it: its host functions, Unicode and time-zone data, clock, entropy, and jed
version. A new host function in the green deployment works from green's first query.

Persisted facts are file facts and are unaffected: collations baked into the file, index-dependency
pins ([index-dependencies.md](index-dependencies.md)), and the `XX002` rule for a file that needs host
code to reopen bind every participant equally.

**Compatibility reduces to two version checks at `hello`:**

- **Page protocol version.** Negotiated; each release states the range it serves and speaks.
- **On-disk format.** A client must read and write the file's current `format_version`. A format
  upgrade happens only on an owner with no connected clients, and only after it promotes; an older
  client connecting afterwards fails `0A000` with the required version in detail. In a blue-green
  deploy the newer green therefore upgrades the format only after blue has gone.

## 8. Ownership handoff

### 8.1 Promotion

When the owner exits, connected clients see EOF. One waiting client acquires `presence EX`, recovers,
opens the file, and binds the endpoint; the others reconnect to it. A client whose transactions were
all between requests continues transparently. A transaction that was open fails:

- a read or uncommitted write transaction fails `08006 connection_failure`, and the application
  retries, as it would after a PostgreSQL failover;
- a commit whose acknowledgment was lost fails `08007 transaction_resolution_unknown` unless it can be
  resolved (§8.4).

The promoted process switches its pager from the service to the local file between transactions. Its
cache holds byte-identical page contents, but v1 drops it; reusing it is an optimization.

### 8.2 Ownership preference and yield

Raw promotion leaves one bad case: a tool (the CLI, an SSH helper) became owner while the application
was down and is left open; the application would run as a client indefinitely. `open` therefore takes
an ownership preference:

| Preference | Default for | Behavior |
|---|---|---|
| `prefer` | applications | Become owner when free; when the owner is yielding, ask it to yield. |
| `yield` | tools (CLI, SSH helper, backup) | Become owner when free; hand ownership to a `prefer` requester. |
| `never` | monitoring, explicit client-only use | Never own; fail `55006` when there is no owner. |

**Yield.** A yielding owner receiving `request_ownership` stops admitting new transactions (owner
sessions and clients), waits up to a yield grace period for running ones to finish, aborts the rest
with `08006`, then releases `presence EX`. The requester is waiting on the OS lock and acquires it;
the yielding tool becomes a client of the new owner. An interactive user sees, at worst, one aborted
transaction. Two `prefer` processes never yield to each other.

### 8.3 Blue-green walkthrough

1. Blue owns the file and serves.
2. Green opens with `prefer`, finds blue (also `prefer`), and becomes a client. Green serves traffic
   with its own engine; reads fetch pages through blue, writes commit through blue.
3. Blue begins graceful shutdown: it stops admitting new transactions, drains within its grace
   period, closes, and releases `presence EX`.
4. Green acquires `presence EX`, becomes owner, and switches to the local file. Green requests that
   straddled the moment fail `08006` or `08007` and are retried.

No format, environment, or engine-version compromise is needed during the overlap.

### 8.4 Resolving an unacknowledged commit

A client that sent `commit` and lost the connection knows its delta's `txid` and manifest digest.
After reconnecting to (or becoming) the next owner, it asks `commit_status`: the new owner's
recovered meta either has that `txid` with a matching manifest (committed), or a lower `txid`
(not committed; the gate was lost, so nothing else can have committed in between). Only if
resolution is impossible does the application see `08007`.

## 9. Backup

Both backup forms are page-service clients, so they work locally and over SSH alike.

- **Page-identical** (the `pg_basebackup` analogue). `snapshot_stream(txid, page_identical)` sends
  every page reachable from the pinned snapshot, plus its free-list chain, at its own index, and a meta
  for that root. The writer zero-fills unreachable indices and writes **bootstrap meta** (zero manifest
  fields, both slots on the same root — validated-cow.md) so the copy opens without the original's
  last manifest. The result has the original's page numbering, so it can seed a replica (§10).
- **Compacted** (the SQLite `VACUUM INTO` analogue). A client pins a snapshot and runs the existing
  streaming `compact` image writer locally against it, writing a fresh, renumbered file. Smallest
  output; cannot seed a replica.
- **Logical dump** (the `pg_dump` analogue) — SQL text or similar, the only form that survives a
  `format_version` change or is readable without jed. It is a CLI feature on top of an ordinary
  client and is out of scope here.

## 10. Replication (amends replication.md)

The replication design in [replication.md](replication.md) stands — block-shipping, no WAL — with
these consequences of the owner model:

- **The tee is at the owner's commit point**, after the durable barrier, before the gate is released.
  Owner commits and client `CommitDelta`s pass through the same point, so the log is complete and in
  `txid` order with no cross-process merge. Retained deltas live beside the bundle (location open).
- **The crash gap is repairable.** If the owner dies after the barrier but before appending, only the
  newest commit is missing, and its dirty-page set is in the meta's v33 manifest, protected until the
  next commit. The next owner rebuilds the entry before serving.
- **Relaxed `synchronous`.** The log records the durability floor; `subscribe` ships only entries at
  or below it, and recovery truncates entries above the recovered `txid`.
- **`subscribe(from_txid)`** streams retained entries then the live tail. A subscriber older than
  retention is told to re-seed from a page-identical backup (§9).
- **A replica is a client whose cache is a persistent, page-identical local file** kept current by
  the delta stream. The replica's process owns that file; queries on the replica are its own sessions
  or page clients of it, so its watermark covers every reader and applying a delta that reuses a page
  waits only on real pins. This resolves the hot-standby conflict without query cancellation.
- **Lineage.** Each delta carries `(database id, timeline, txid)`. Promoting a replica, compacting,
  or breaking the stream starts a new timeline; a subscriber on another timeline must re-seed.
  Persisting a database id is a format change, scheduled with the replication slice.
- **Asynchronous by construction.** Without a server, no primary waits on a remote acknowledgment, so
  replication can lose the commits not yet shipped. Off-host shipping is a pull (`subscribe` over SSH)
  or a push by a local sidecar subscriber to object storage (the Litestream model).
- **Log-append failure** (disk full, I/O error) happens after the commit is durable. The owner records
  an explicit stream break (a new timeline) and keeps accepting writes; a broken stream is surfaced to
  the host, never silent. Refusing writes until the log is healthy is an alternative policy (§17).

## 11. Serving

### 11.1 Options

- `serve`: `true` (default where supported, §14) | `false`. A non-serving owner is today's
  `exclusive` mode.
- `ownership`: `prefer` | `yield` | `never` (§8.2).
- Owner-side limits: maximum concurrent connections, idle-session timeout, idle-in-transaction
  timeout, yield and shutdown grace periods.

### 11.2 Connection capabilities

An owner may restrict a connection to reads (`begin_write` fails `25006`), set by the owner's policy
or requested by the client (`--read-only` on the SSH helper). That is the only privilege the page
service has: SQL privileges, the session capability envelope, and cost budgets are enforced by the
**client's** engine, because the client executes the queries. A process that can write pages can
corrupt the database regardless (§13).

## 12. Changes to the lock bundle and the shared protocol

Superseded when ratified:

- shared mode: the join window, alone↔shared transitions (locking.md §4.2–§4.3), meta refresh at
  transaction begin (§5.1), the cross-process `writer` and `commit` gates (§5.2–§5.3), and append-only
  co-resident commits;
- the `shared` value of `locking`. The option becomes `auto` (owner/client) | `exclusive` (owner,
  `serve = false`) | `none` (unchanged escape hatch).

Kept: the stable bundle and its identity rules (locking.md §3.1), `presence` as the ownership lock,
the hard-link rejection, the per-path in-process registry, fork invalidation, the open timeout, and
fail-closed behavior on unsupported hosts.

**Migration from v1.** A v2 marker is introduced under locking.md §3.1's migration rule (holding
`arrival EX`, `transition EX`, and `presence EX`, honoring the v1 lock names). A v2 owner holds
`presence EX`, so a v1 shared-mode process times out with `55006`; a v2 opener that finds a v1 group
holding `presence SH` cannot own or connect and fails `55006`. Mixed fleets fail closed, never
corrupt. The rollout drains v1 processes once, as the v1 rollout drained pre-protocol binaries.

## 13. Security and trust

- **Local endpoint permissions equal database-file permissions.** The endpoint lives in (or is
  referenced from) the bundle directory, created with the database file's ownership and mode. Any
  process able to connect could already open the file.
- **Clients are trusted the way file openers are trusted.** A client writes raw pages; the owner's
  validation (§5.6) catches client bugs and protocol violations (`08P01 protocol_violation`), not a
  malicious client that constructs valid-looking pages. This is unchanged from shared mode, where any
  participant writes the file directly.
- **Remote access is SSH.** SSH provides authentication and encryption; `authorized_keys`
  `command="jed pages --read-only /srv/app.jed"` restricts a key to read-only page service on one
  database. jed never listens on a network port.
- **Untrusted SQL** (CLAUDE.md §13) is unaffected: it executes in some process's engine under that
  process's session envelope, exactly as today.
- **Keyless serving.** Encrypted pages cross the service as ciphertext (§5.1), so a backup host or SSH
  helper can serve, back up, or replicate an encrypted database without the key.

## 14. Platforms and cores

| Core / host | Serving owner | Non-serving owner | Client |
|---|---|---|---|
| Rust (and the Node and Ruby bindings over it) | yes | yes | yes |
| Go | yes | yes | yes |
| Native TS on Node | no | yes | optional (§17.7) |
| Native TS in the browser (OPFS) | no | yes (today's exclusive access) | no |
| wasm32-wasip1 | no | no (explicit `none` only, as today) | no |

Production server-side JavaScript is expected to use the Rust bindings; native TS matters mainly in
the browser, where there is one process. Not every core needs to serve.

**Endpoint mechanics (open):**

- **Path length.** Unix socket paths are limited to about 104–108 bytes, so a socket inside a deep
  bundle path can fail to bind. Proposed: bind `<path>.lock/endpoint` when it fits, otherwise a short
  per-user runtime path (e.g. `$XDG_RUNTIME_DIR/jed/<hash>.sock`) recorded in a pointer file in the
  bundle. Linux's abstract namespace is rejected (no file permissions).
- **Windows.** Go supports `AF_UNIX` on Windows. Rust's standard library does not, so the Rust core
  needs named pipes or `AF_UNIX` through either a crate (a §14 dependency decision) or `unsafe` FFI
  (a §13 decision). Until decided, Rust on Windows offers non-serving ownership only.

## 15. Verification

- **Process corpus rewrite.** The release-blocking Layer 4 corpus (locking.md §9) moves from shared
  mode to election, read and write through the service, promotion, yield, owner kill mid-commit,
  client kill holding a pin or the gate, `commit_status` resolution, and v1/v2 fail-closed exclusion,
  in every supported core pairing.
- **The conformance corpus through a client.** Run the entire sqllogictest corpus with the harness
  as a page client of another core's owner (for example a Go client against a Rust owner). Rows,
  errors, plans, and **costs** must be identical to a local run, because page reads are logical.
  This tests the protocol as hard as the corpus tests the engine, and exercises the cross-core
  byte-identity of every page both cores write.
- **Simulation testing.** Simulated processes become an owner plus clients over a simulated channel
  whose deliveries, disconnects, and owner crashes are seeded; the storage oracles are unchanged and
  the commit-role rule (§6) is part of the history.
- **Validation fuzzing.** Mutated `CommitDelta`s must fail `08P01` without touching the file.
- **Benchmarks** ([benchmarks.md](benchmarks.md)): alone-owner fast path (no regression), client
  warm-cache and cold-cache reads, client commit latency, and SSH-latency scans with and without
  read-ahead.

## 16. Phases

1. **Spec**: message catalog and framing (`spec/pageservice/`), byte fixtures, error registrations
   (`08006`, `08007`, `08P01`, `25P03`).
2. **Read path** in Rust and Go: election, endpoint, `hello`/`begin_read`/`read_pages`/`end_read`,
   client cache coherence, read-only clients.
3. **Write path**: writer gate, reuse grant, `commit` with validation, owner adoption, the
   reclamation rule, `commit_status`.
4. **Handoff**: promotion, yield, graceful close, timeouts; the process-corpus rewrite; simulation
   testing; retire shared mode and the `shared` option; v2 bundle migration.
5. **SSH**: `jed pages` stdio transport, the CLI opening `ssh://host/path`, read-ahead and subtree
   fetch.
6. **Backup**: page-identical and compacted forms.
7. **Replication**: the tee, retained log, `subscribe`, lineage and timelines (format change),
   replicas and read replicas; amend replication.md.

Shared mode stays in place until phase 4 lands, so blue-green deploys keep working throughout.

## 17. Open questions

1. **Endpoint location**: bundle with runtime-directory fallback (proposed), or always a runtime
   directory?
2. **Rust on Windows**: a named-pipe crate (needs §14 approval) or non-serving only at first?
3. **Reclamation**: owner-only rebuilds with maintenance commits (proposed), or clients rebuild too?
4. **Grace periods and defaults** for yield, shutdown, idle sessions, and idle-in-transaction.
5. **Freshness**: keep `begin_read` a round trip always (proposed), or allow an opt-in relaxed mode
   in which owner-pushed commit notices let a client begin locally?
6. **Log-append failure policy**: break the stream and keep writing (proposed), or refuse writes?
7. **Native TS client** on Node: worth building, or not until asked?
8. **Query-level protocol over SSH**: deferred. It would only win for very large scans or
   extractions where shipping rows beats shipping pages; the RAM-sized design target suggests batched
   page fetches cover most remote use.
9. **Attachments**: each attached file has its own owner, so one process may own `main` and be a
   client of an attachment. The one-durable-writer-per-transaction rule is unchanged; confirm the
   writer-gate acquisition order across owners avoids deadlock (locking.md §6 orders by path today).
