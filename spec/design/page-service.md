# Page service — one owner process, page-level clients — design

> **Status: RATIFIED DESIGN (2026-10-16); phases 1–6 landed, 7 to come (§16).** This document
> replaced the earlier shared multi-process protocol with a single **owner** process that alone
> touches the database file and serves **pages** to every other process over a local endpoint, and
> extends the same service over SSH for remote access, backup, and replication. The message catalog,
> framing, and byte fixtures are [spec/pageservice/](../pageservice/README.md).
>
> `locking = auto` selects the page service; [locking.md](locking.md) owns the coordination bundle,
> its locks, and the exclusive (non-serving) owner.

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
| **Coordinator with direct file reads** (owner tracks pins and the writer gate; clients `pread` the file) | Fixes the environment problem but keeps two processes touching one file. When the coordinator dies, a client may still be reading pages the next coordinator could reuse; closing that race needs per-incarnation generation locks and an append-only interregnum. Its one advantage, recorded as a cost of this design (§4.1): a live but stuck coordinator stalls only new pins and commits, while clients reading the file directly keep reading. |
| **Proxy the five-method `BlockStore`** over a socket | The seam has no transactions. Each client's pager would allocate and publish independently: the multi-process problem moved onto a pipe. |
| **A write-ahead log** | Copy-on-write plus the meta swap already provide atomic durable commits and non-blocking readers. A WAL would add a second file at rest that recovery depends on, checkpointing, and (for multiple processes) a shared-memory index, for the one benefit, a single ordered change history, that the owner's commit stream provides here. Replication does add a retained delta log (§10), also a second at-rest artifact with location, retention, and crash-ordering obligations. The difference is that the database never depends on it: it exists only while replication is configured, and losing it breaks the stream (§10), never the database. |

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
- Opens the file the way its own handle asks. A `read_only` handle ([api.md](api.md) §2.1) that
  becomes owner opens the file without write access, as today, so the OS still enforces that the
  process cannot write. Such an owner cannot accept commits, so it hands ownership to the first
  client that wants to write (§8.2), whatever the preferences involved.
- Must hold the database's encryption key when the database is encrypted. Recovery, commit
  validation, adoption, reclamation, and backup all read logical page bytes (§13).
- **A live but stuck owner stalls every client.** Ownership ends only at process death, so an owner
  that is stopped (`SIGSTOP`), paused (a long GC), or blocked in a durability barrier delays every
  client's `begin_read`, `read_pages`, and `commit` until it resumes. Shared mode's already-joined
  peers keep reading the file directly in that case. This design accepts the cost in exchange for
  one process touching the file. **There is no client-side request timeout** (decided at
  ratification): a client waits for a live owner as a reader of a local file waits for a slow disk.
  Only the owner's death (EOF) or the client's own cancellation ends the wait; the open-time
  `file_lock_timeout_ms` and the writer-gate `lock_timeout_ms` keep their meanings.

### 4.2 Client

- Never opens the database file. Its pager is backed by a page-service connection.
- Holds no bundle lock while connected. Its liveness is the connection: EOF releases everything it
  held (pins, the writer gate).

### 4.3 Election at open

`open(path)` with a coordinated `locking` mode:

1. Prepare the bundle as today (marker validation, §12).
2. Try `presence EX` without waiting. On success, become the owner (§4.1).
3. Otherwise connect to the endpoint (§14). If the endpoint is absent or refuses because the owner
   has not finished starting, retry until `file_lock_timeout_ms` expires, then fail `55006` with the
   path in detail. An owner that runs with `serve = false` binds nothing and answers nothing. To make
   that case distinguishable from a serving owner that has not bound yet, every owner writes a small
   `owner` record in the bundle (whether it serves) after acquiring `presence EX` and before binding.
   The record also names the owner's user and its endpoint path (§14). Beyond finding the endpoint,
   the opener uses it only to word the timeout error: "owned by a non-serving process" or "owned by
   another user's process" (§13). A stale record from a crashed owner can only mislabel that error,
   never change an outcome.
4. A stale endpoint left by a crashed owner is harmless: only a `presence EX` holder may replace it.

A client learns that the owner left from its connection: the owner releases `presence EX` only after
closing every connection (or by dying, which closes them). So a client polls `presence EX`, with
non-blocking try-locks, only after its connection ends: it re-elects, promoting itself unless it is
configured `ownership = never` (§8.2), or reconnecting to whichever process took ownership. The polls
pause during a yield handoff (§8.2). The exact rules are
[spec/pageservice/README.md](../pageservice/README.md) §6.5.

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
single-process commit would write.

The **owner still needs the key.** In an encrypted file the manifest digests are over the on-disk
(ciphertext) bytes and the meta slot is plaintext, so validating a client's delta is keyless work;
but every body page, including catalog, interior, free-list, and manifest-overflow pages, is
encrypted ([encryption.md](encryption.md) §4), so recovery's logical checks at open, adoption (§5.6),
the free-list rebuild (§6), and reachable-page backup (§9) all decrypt. A keyless process can still
be a replication subscriber or a replica's apply loop (§10), which store deltas as bytes at their
indices without interpreting them. There is no keyless owner. An owner answers `read_pages` with the
**stored bytes** of a page, read below its codec, never a page re-sealed from its plaintext cache:
every pointer in an encrypted file binds its child's AEAD tag, so a re-sealed page would fail the
client's binding check (encryption.md §4).

### 5.2 Operations

All requests carry a request id; a client may pipeline. Responses return in request order, except
that `begin_write`'s reply arrives when its wait for the writer gate ends, while the owner keeps
answering the connection's later requests, so a writer waiting for the gate never stalls its process's
readers.

| Operation | Effect |
|---|---|
| `hello(versions, format_version, page_size, caps)` | Handshake: pick a protocol version; confirm the client supports the file's `format_version` and page size; report connection limits (read-only, §11.2). With the encryption format, over SSH it also carries the database credential to a server that may become owner — in the stream, never `argv` or the environment ([encryption.md](encryption.md) §6). |
| `begin_read(incarnation, last_txid)` | Pin the owner's current committed snapshot. Returns the meta (root, `txid`, `page_count`, free-list head, live count), the owner's **incarnation id**, and the **invalidation set**: page indices written by commits after `last_txid`, or "invalidate all" (§5.4). |
| `read_pages(txid, [index…])` | Return page bytes. `txid` must be pinned by this connection. Batched. |
| `end_read(txid)` | Release one pin. Asynchronous: no reply. |
| `begin_write(last_txid)` | Acquire the writer gate (§5.5), then the same refresh as `begin_read`, plus the **reuse grant** (§6). |
| `commit(CommitDelta)` | Owner validates and persists (§5.6). Reply: the new `txid` once durable. |
| `abort()` | Release the writer gate; discard any received delta pages. |
| `commit_status(txid, manifest_digest)` | Resolve an unacknowledged commit after a lost connection (§8.4). |
| `snapshot_stream(txid, form)` | Reserved. The ratified design streamed a pinned snapshot for backup from the owner; backup instead reads pages with `read_pages` (§9). |
| `subscribe(from_txid)` | Stream retained commit deltas, then the live tail (§10). |
| `read_key_block()` | Encrypted databases, with the encryption format: return a stable image of the key block (both copies, one generation), served under the lock a credential change holds. How a page client unlocks at open and re-checks after a compaction reload or owner change, and what a backup copies instead of reading pages 2–3 ([encryption.md](encryption.md) §6). |
| `rewrap(block)` | Encrypted databases, with the encryption format: a page client's credential change, executed by the owner with the two-barrier protocol while the client holds the writer gate; acknowledged after the log entry is durable (encryption.md §6). |
| `request_ownership()` | Ask a yielding owner to step aside (§8.2). |

The message catalog, framing, and field encodings are data-shaped and belong in a shared spec table
(`spec/pageservice/`), hand-written per core like the file format and verified by byte fixtures.

### 5.3 Read path

A read transaction is one round trip: `begin_read`, then queries run over the pin, fetching pages only
on a cache miss. `end_read` needs no reply. In an encrypted database the owner answers `read_pages`
(and SSH read-ahead) from **below its codec** — the stored bytes, read from the file through the OS
page cache, as a TS owner already serves clients today (§14) — never from its plaintext buffer
pool, because every pointer binds its child's tag and a re-sealed page would fail the client's
check (encryption.md §4). The cost is a file read per client miss the owner's pool would otherwise
have absorbed; a ciphertext side-cache at the owner is an option, not decided. This keeps today's guarantee that a transaction begun
after another process's commit returned sees that commit, the freshness the earlier shared protocol
got from its meta re-read at transaction begin.

**`begin_read` is always a round trip** (decided at ratification). There is no relaxed mode in
which owner-pushed commit notices let a client begin locally: that would make a client's freshness
depend on notice delivery timing, a second consistency mode for every test to cover, for a saving of
one local round trip.

The `begin_read` reply carries the **committed meta page itself** — the exact bytes of the meta slot
that published the pinned snapshot. The client's pager decodes it with the ordinary meta parser, so
the root, `txid`, `page_count`, free-list head, and live count reach it with no second encoding. A
client does not re-validate the manifest the meta names: the owner validated it at recovery or
wrote it itself, and each page's own CRC-32 still catches transfer corruption on decode (`XX001`).

When the returned `txid` advanced, or the reply is "invalidate all", the client reloads its
catalog/interior skeleton and invalidates persistent plan caches for that database, as shared mode
does today on a foreign `txid`.

Fetches are batched. The client pager requests read-ahead on leaf chains (the next K leaves named by
an interior already in hand) and, for scans, can request every page under a subtree root in one
message. Read-ahead is a pager policy, invisible to cost: page reads remain logical units
([cost.md](cost.md)), so a query's cost is identical through the service and on a local file.

**As built (phase 5).** Read-ahead is entirely client-side and uses the ordinary `read_pages`, so the
protocol is unchanged. When a scan descends into a demand-paged leaf of an interior node it holds
(every interior is resident, so the leaves an interior names are known without a fetch), it hints
that leaf together with the following leaves of the same interior that the scan will visit, in visit
order and inside the scan's key bounds. The hint window starts at **one** leaf, so a scan that stops
after its first leaf (a `LIMIT`) fetches nothing extra, and doubles with each hint up to **64**, so a
long scan fetches leaves 64 to a request: every leaf under an interior with at most 64 children (the
subtree fetch) in one message. The pool takes the fetched pages through the same decode a fault
uses, skipping pages already resident or loading, at most a quarter of the pool at a time; its
invalidation is therefore the pool's (§5.4), and a failed read-ahead is ignored (the fault that
follows reads the page and reports any error). A request names at most the `hello_ok`
`max_read_pages`. Read-ahead is on for clients over the stream transport (the SSH case,
[spec/pageservice/README.md](../pageservice/README.md) §5.3), whose round trips cross a network; a
local-socket client still fetches one leaf per fault, which keeps the deterministic simulator's frame
counts and the cross-core local behavior unchanged. Rust implements it; Go and TS have no stream
transport yet. Point lookups fetch only their path (one leaf), as before.

### 5.4 Client cache coherence

Under copy-on-write a page index is written only when it is allocated, fresh from the high-water or
reused from the free list. That includes free-list chain and manifest-overflow pages, which are
allocated like any other page and never rewritten in place ([validated-cow.md](validated-cow.md) §2).
Every such write is listed in that commit's manifest. So the set of indices written after `last_txid`
is exactly the set whose cached contents may be stale, and dropping just those keeps a client cache
correct.

The owner retains a bounded window of per-commit written-index sets. A client further behind than the
window receives "invalidate all". Steady-state reads from a warm client cache cost no IPC beyond the
`begin_read` round trip.

That argument holds only within one history. Under relaxed `synchronous` (§10) an owner crash can
recover generation N−1 after a client cached generation N, or a later owner can reach the same `txid`
along a different history, so a `txid` alone cannot say whether a cache is current. Each owner
therefore draws a random **incarnation id** when it becomes owner. A client sends the incarnation its
cache came from; when it differs from the owner's, the owner replies "invalidate all", and the client
drops its page cache, catalog skeleton, and plan caches regardless of `txid`.

Consequently every promotion (§8.1) starts every client cold, at the moment traffic shifts to the new
owner. Seeding the new owner's window from the two meta slots' manifests would be sound only for a
client whose cache is known to be from the recovered history; the first implementation does not
attempt it.

### 5.5 Writer gate

The owner's existing in-process writer gate is the single global writer gate. `begin_write` waits on
it under the session `lock_timeout_ms` (`55P03` on expiry, as today). A client holds it from
`begin_write` until its `commit` is acknowledged or it aborts. The owner enforces an
idle-in-transaction timeout on client write transactions (§11.2); a revoked transaction fails
`25P03 idle_in_transaction_session_timeout` at its next request. A client whose connection drops
while holding the gate is aborted at EOF unless its commit has passed the **point of no return**:
the owner has received the complete `CommitDelta` and it passed validation (§5.6). After that point
the owner finishes the commit whether or not the connection survives, then releases the gate; the
client resolves the outcome with `commit_status` (§8.4).

**Attachments: one global acquisition order across owners** (decided at ratification). Each
attached file has its own owner, so one process may own `main` and be a client of an attachment, or
the reverse. The one-durable-writer-per-transaction rule ([attached-databases.md](attached-databases.md))
is unchanged, and so is the conservative rule that every writable transaction takes every writer gate,
whatever it writes: it acquires the writer
gate of `main` and of every read-write file attachment, **in normalized (real-path) UTF-8 byte
order**, whether a given gate is local (this process owns that file: its in-process gate) or remote
(another process owns it: `begin_write` to that owner). Every process uses the same total order over
the same real paths, so no wait cycle can form across owners. Each wait is also bounded by the session
`lock_timeout_ms` (`55P03`) as a backstop, and an acquisition failure releases the gates already held
in reverse order. A future target-declaring transaction API may narrow the set. **Landed:** each
attachment under `locking = auto` is elected owner or page client of its file exactly as an open is,
with its own role and handoff; read begins pin it through that role, and attaching or detaching waits
for no other process. The exact rules are [spec/pageservice/README.md](../pageservice/README.md) §6.6.

### 5.6 Commit

The client builds the `CommitDelta` with the ordinary v33 recipe ([validated-cow.md](validated-cow.md)):
allocation under the reuse grant (§6), dirty body pages in deterministic order, free-list pages,
manifest, and the alternate meta. It **sends the meta and manifest first**, then the pages. Large
deltas stream in chunks; the owner may write each page as it arrives once that page passes the
per-page checks below, because every legal target is either beyond the committed high-water or a
granted free page, so none is reachable from any pinned snapshot or the fallback meta.

**Owner validation** (cheap, structural; it catches client bugs, not malice — §13):

- before any page is written: `txid == committed + 1`, `page_size` and `format_version` match, the
  meta's CRC and bounds are valid, and every index the manifest lists is beyond the previous
  `page_count` or in the granted reusable set;
- before each page is written: its index is listed in the manifest and not yet received, and its
  CRC-64 identity matches the manifest's;
- at the end: every page the manifest lists was received.

A failed check rejects the delta with `08P01` and aborts the transaction. Pages already written are
unreachable orphans of the rejected delta, beyond the high-water or in the free list, and the next
commit reuses or overwrites them; the fallback meta and every pinned snapshot are untouched.

Then, holding the gate: write the meta, execute the single durable barrier, adopt the new committed
snapshot locally (reload the skeleton; drop the owner's buffer-pool entries for written indices),
append the delta to the replication tee, record `(txid, manifest digest)` in the resolution window
(§8.4), acknowledge, and release the gate (after a maintenance commit, if one is due, §6). A write
or sync failure poisons the owner's storage handle exactly as an alone process's does ([validated-cow.md](validated-cow.md)): the
owner stops serving and closes, and the commit's outcome is resolved by recovery under the next
owner (§8.4).

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
- **Free-list rebuilds are owner-only.** The periodic in-commit reclamation (the
  reachability walk once the pages off the free list pass about twice the live count, format.md
  *Reclamation*) is O(live
  pages). Run by a remote client it would fetch the whole database. Instead a client commit never
  rebuilds; when a client commit crosses the rebuild threshold the owner runs a storage-maintenance
  commit immediately after it. The client's commit is acknowledged first, so the maintenance commit
  adds no latency to it, but the owner does not release the writer gate between the two, so no other
  writer starts in between. This makes a file's page layout depend on which
  process committed, which is fine for the cross-core contract (each role's rule is specified and
  identical in every core) but must be modeled by simulation testing (§15). Letting clients rebuild
  too was rejected at ratification: a remote client would fetch the whole database to walk it.
- **The maintenance commit is a new commit kind.** It changes no table, yet it writes pages (the
  rebuilt free list, and the catalog chain, which every commit's manifest must name — format.md), issues
  a barrier, and advances `txid`. [transactions.md](transactions.md) §2.1 (no mutation,
  no barrier, no `txid` advance) needs an explicit carve-out for it, and it is a distinct delta kind
  in the replication log (§10) and in simulation histories (§15).
- **Relaxed durability.** Under `synchronous = off` the owner's own commits, its maintenance commits
  included, batch their barrier ([validated-cow.md](validated-cow.md) §4). A client cannot see the
  owner's pending set, which a cumulative manifest would have to name, so the owner flushes it before
  granting a `begin_write`, and every client commit keeps its own barrier at the point of no return:
  a client's `synchronous` setting does not reach the owner, so the owner treats it as `on`. Close,
  yield, and shutdown flush before ownership moves; a successor after an owner crash stabilizes the
  adopted generation as always (validated-cow.md §3).
- **Trailing truncation is owner-only too** ([../fileformat/format.md](../fileformat/format.md)
  *Trailing truncation*). A client's delta never lowers the page count (the owner rejects one that
  does), so a client plans exactly as before. The owner's own commits truncate as an alone process
  does, and the post-client-commit maintenance commit also runs when the persisted free list ends at
  the high-water (its top page is `page_count − 1`) and no pin holds the watermark back, so the space
  a client's commits leave at the top is returned without waiting for the next rebuild. At most one
  maintenance commit follows a client commit. Under `synchronous = off` the owner's flush before granting a
  `begin_write` shrinks nothing, in every core (the Node core runs it on the service worker, outside
  the engine's pager): a truncation an owner commit deferred waits for the owner's next barrier over a
  commit of its own ([validated-cow.md](validated-cow.md) §4).
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

**Compatibility has three parts; `hello` checks the first two:**

- **Page protocol version.** Negotiated; each release states the range it serves and speaks.
- **On-disk format.** A client must read and write the file's current `format_version`, or it fails
  `0A000` with the required version in detail. Every core reads and writes exactly one
  `format_version`, and there is no in-place upgrade or mixed-version migration
  ([format.md](../fileformat/format.md); [validated-cow.md](validated-cow.md) §1). So a release that
  changes `format_version` cannot overlap with its predecessor at all. A deploy across a format change
  is stop-then-start with whatever migration exists outside this design (such as a logical dump and
  restore, §9). Designing a format upgrade path is out of scope here.
- **Persisted SQL.** Catalog loading reparses stored SQL text: view definitions, expression
  defaults, CHECK constraints, partial-index predicates, and expression index keys. A newer release
  can add syntax without changing `format_version` (`SEARCH`/`CYCLE` did). An older binary that meets
  such text keeps the object unparsed and fails only the statements that need it, with `XX003`
  ([unparsable-definitions.md](unparsable-definitions.md)); this holds standalone and under the page
  service alike. The page service adds two rules of its own. The owner's service duties (validation,
  adoption, reclamation, backup — §5.6, §6, §9) are structural and never parse stored SQL, so a
  newer client's DDL cannot stop the service. And the owner reloads its own sessions' catalog at
  their next transaction begin, not in the commit path, so nothing a client commits can fail after it
  is durable.

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
cache holds byte-identical page contents, but the first implementation drops it; reusing it is an
optimization. The new owner has a new incarnation id, so every reconnecting client is also told
"invalidate all" (§5.4).

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
with `08006`, then releases `presence EX`. The requester is polling the OS lock and acquires it;
the yielding tool becomes a client of the new owner. An interactive user sees, at worst, one aborted
transaction. Two `prefer` processes never yield to each other.

**Handoff ordering.** An OS lock cannot be handed to a chosen process, so other pollers must stand
aside. When a yield begins, the owner tells every connected client which connection requested
ownership. Every other client, including the yielding tool once it is a client, pauses its
`presence EX` polls for a handoff window of 2 seconds after its connection ends; only the requester
keeps polling. If the requester's
connection drops before the owner releases, the owner cancels the yield and keeps ownership. If the
requester dies after the release, the handoff window expires, every client resumes polling, and
ordinary promotion (§8.1) picks an owner, possibly the yielding tool again, which a later `prefer`
opener can ask to yield in turn. Only a `prefer` process, or a writer facing a read-only owner (below),
requests ownership, so two `yield` tools never pass it back and forth.

**Read-only owner.** An owner whose handle is `read_only` (§4.1) yields to the first client that
calls `begin_write`, regardless of either side's preference. The client sends `request_ownership`,
waits for the handoff under its `lock_timeout_ms` (`55P03` on expiry), promotes, and begins the write
transaction as the new owner, so its write is delayed rather than refused. The read-only owner's running
transactions get the yield grace period, then `08006`, and it continues as a client. Two exceptions
fail `begin_write` with `25006` instead: a client opened with `ownership = never`, which never owns,
and a file this user cannot open read-write (no write permission, or a read-only filesystem). The
client checks that before requesting by opening the file read-write and closing it unused, so the
read-only owner never holds write access. Since every participant runs as one user (§13), no one
could write such a file anyway.

### 8.3 Blue-green walkthrough

1. Blue owns the file and serves.
2. Green opens with `prefer`, finds blue (also `prefer`), and becomes a client. Green serves traffic
   with its own engine; reads fetch pages through blue, writes commit through blue.
3. Blue begins graceful shutdown: it stops admitting new transactions, drains within its grace
   period, closes, and releases `presence EX`.
4. Green acquires `presence EX`, becomes owner, and switches to the local file. Green requests that
   straddled the moment fail `08006` or `08007` and are retried.

Within one `format_version`, no environment or engine-version compromise is needed during the
overlap. A release that changes `format_version` cannot overlap (§7). Green may store SQL that blue
cannot parse; blue then fails only the statements that need it (§7), and blue is draining anyway.

### 8.4 Resolving an unacknowledged commit

A client that sent `commit` and lost the connection knows its delta's `txid` and manifest digest.
After reconnecting to (or becoming) an owner, it asks `commit_status(txid, digest)`. The owner may be
the same one (the connection dropped but the owner survived) or a new one, and other writers may
have committed in between, so the committed `txid` alone cannot answer.

Each owner keeps a bounded **resolution window** of `(txid, manifest digest)` pairs: one per commit
it publishes (§5.6), seeded at startup with the pairs from both recovered meta slots, so the window
covers a contiguous `txid` range. The owner first waits for any commit already past its point of no
return (§5.5) to finish, then answers:

- the window holds `txid` with the same digest: **committed**;
- the window holds `txid` with a different digest: **not committed** (the delta was aborted, or
  recovery fell back past it, and another commit took that `txid`);
- `txid` is above the owner's committed `txid`: **not committed**;
- `txid` is below the window: **unknown**, and the application sees
  `08007 transaction_resolution_unknown`.

## 9. Backup

Both backup forms are page-service clients, so they work locally and over SSH alike. Each pins one
committed snapshot the way a reader does (`begin_read`) and copies it into a new file; neither takes
the writer gate, so a backup never waits for a writer and never sees an uncommitted write. Over the
stream transport (Rust, phase 5) a backup through `Database::open_remote` produces both forms
byte-identical to the local handle's, whether `jed pages` relays or owns (`tests/remote_stream.rs`).

- **Page-identical** (the `pg_basebackup` analogue). The backup walks the pinned snapshot with its own
  engine and copies every page reachable from it — the catalog chain, every table and index B+tree,
  every GiST R-tree, every live overflow chain — plus its free-list chain, each at its own index,
  reading them with batched `read_pages`. The writer zero-fills every other index below the
  snapshot's page count (its dead pages and its last commit's manifest pages) and writes **bootstrap
  meta** (zero manifest fields, both slots on the same root — validated-cow.md) for that root, with
  the snapshot's `txid`, page count, free-list head, and live count, so the copy opens without the
  original's last manifest. The result has the original's page numbering, so it can seed a replica
  (§10). In an encrypted database the backup also copies the **key block** (pages 2–3), which no
  pointer reaches — taken **before** the page walk through the dedicated `read_key_block`
  operation, which the owner serves under the lock a credential change holds, so the image is a
  complete pair at one generation rather than two possibly-torn page reads; a backup without it
  would hold no wrapped key ([encryption.md](encryption.md) §6, *the key block in backups*).
- **Compacted** (the SQLite `VACUUM INTO` analogue). A client pins a snapshot and runs the existing
  streaming `compact` image writer locally against it, writing a fresh, renumbered file at the
  snapshot's `txid + 1`: byte-for-byte the file host compaction ([api.md](api.md) §2.6) would leave
  for that snapshot (for an encrypted database, identical up to the fresh salts every seal draws,
  encryption.md §3.1). Smallest output; cannot seed a replica. It is also the one form an in-memory
  database has (a page-identical backup of one is `0A000`).
- **Logical dump** (the `pg_dump` analogue) — SQL text or similar, the only form that survives a
  `format_version` change or is readable without jed. It is a CLI feature on top of an ordinary
  client and is out of scope here.

**The client walks** (decided when phase 6 landed). The ratified text had the owner walk the pinned
snapshot and send its pages with `snapshot_stream(txid, page_identical)`. Finding the reachable pages
means decoding the catalog — column types for the overflow chains, index kinds for the GiST
R-trees — which takes an engine and an environment that can decode it. The backup process has both,
for the same reasons every client runs its own engine (§3); the owner would decode an older pinned
version it never otherwise interprets, with its own environment, and a TS owner's service worker has
no engine at all (§14). Every copied page crosses the connection once either way. The cost of the
client walk is extra round trips for the walk's own reads (each catalog page, overflow-chain hop, and
GiST level); over SSH, phase 5's read-ahead and subtree fetch are what shorten them, so
`snapshot_stream` stays a reserved code.

The same walk serves every role: an owner, or a handle outside the page service, reads its own file
through its pager. Common rules ([spec/pageservice/README.md](../pageservice/README.md) §6.6):

- The destination must not exist (`58P02`, as `create`). The backup writes `<dest>.jedtmp`, syncs it
  (unless the handle skips fsync), renames it into place, and syncs the directory; a failure removes
  the temp file and leaves no destination.
- The backup's pin is an ordinary reader's: it holds back the reuse of pages freed after it and makes
  compaction fail `55006` while it runs. A role change during a client's backup fails it `08006`.
- Every copied page's CRC-32 is checked (`XX001`), and the reachable count must equal the snapshot
  meta's live count (`XX001`).
- Under `synchronous = off` (validated-cow.md §4) the pinned snapshot may be newer than the durable
  floor: its pages and meta are written but not yet synced. The backup copies it all the same, since
  a published generation is complete in the file, and syncs its own file as above, so the copy is
  durable even while the source batch is not. Every commit of a batch writes the same non-floor meta
  slot, so the next one may overwrite the pinned version's meta before the backup reads it; the
  backup then pins the newest version and walks that one instead.
- Trailing truncation (format.md *Trailing truncation*) cuts only free pages the reader watermark
  lets a commit reuse, so the pin keeps every page the snapshot reaches, and its free-list chain, in
  the file while the backup reads.
- An overflow chain kept on rewrite (large-values.md §15) is referenced by exactly one record of the
  snapshot, so the walk reaches it once and counts it as the live count does. The walk reads the
  catalog chain whole, so a catalog entry that spans several pages (format v48) is covered, and a GIN
  index's posting runs (format v47) are records of its entry B+tree, so its tree walk covers them.
- A backup copies the main database only, also when main has file attachments under the page
  service. It is a host act: not metered, not reachable from SQL.
- **The host API is not yet named.** Each core exposes a test-only entry point (`BackupForTesting`,
  `backup_for_testing`, `backupForTesting`); the public name and shape (a destination path and a
  form; attachments; progress) are a product decision, as is the CLI command.

## 10. Replication (amends replication.md)

The replication design in [replication.md](replication.md) stands — block-shipping, no WAL — with
these consequences of the owner model:

- **The tee is at the owner's commit point**, after the durable barrier, before the gate is released.
  Owner commits and client `CommitDelta`s pass through the same point, so the log is complete and in
  `txid` order with no cross-process merge. Retained deltas live outside the bundle, which holds no
  database bytes ([locking.md](locking.md) §3.1); the location is open.
- **Log durability ordering.** Syncing the database file does not make the separate log durable.
  The invariant: **the log entry for commit N is durable before any page write of a later commit
  that may reuse a page commit N wrote.** Under v33 a commit never reuses its predecessor's pages,
  so commit N+2 is the first that can; the simple rule that satisfies it is to sync the log before
  the next commit's first page write. The rule is stated over page writes, not database barriers,
  so it holds unchanged under relaxed `synchronous`, where it costs a log sync the deferred database
  barrier would otherwise have batched.
- **The crash gap is repairable only under that invariant.** If the owner dies after the barrier but
  before the newest entry is durable, only that newest commit is missing; its dirty-page set is in
  the meta's v33 manifest, protected until the next commit. The next owner rebuilds the entry before
  serving. If recovery finds the log more than one commit behind the recovered meta (the invariant
  was violated, or the log was damaged), the gap cannot be rebuilt: the owner records a stream break
  (a new timeline), as for a log-append failure below, rather than claim a repair. A key block one
  generation ahead of the log is rebuilt the same way — its bytes are durable at pages 2–3 — and
  only a wider gap breaks the stream (encryption.md §6).
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
- **Key-block changes are stream items.** In an encrypted database a credential change rewrites
  the key block (pages 2–3) outside any commit. It enters the stream as a key-block delta ordered
  after the commit it follows, under the same lineage. Several changes may follow one `txid`, so
  the stream cursor is `(txid, key_generation)`, and the primary makes the entry durable before
  acknowledging the change. A replica applies it with the primary's own copy–barrier–copy–barrier
  protocol, so a replica always unlocks with the primary's current credential
  ([encryption.md](encryption.md) §6).
- **Asynchronous by construction.** Without a server, no primary waits on a remote acknowledgment, so
  replication can lose the commits not yet shipped. Off-host shipping is a pull (`subscribe` over SSH)
  or a push by a local sidecar subscriber to object storage (the Litestream model).
- **Log-append failure** (disk full, I/O error) happens after the commit is durable. The owner records
  an explicit stream break (a new timeline) and keeps accepting writes; a broken stream is surfaced to
  the host, never silent. Refusing writes until the log is healthy was rejected at ratification: it
  would turn a backup-side fault into an application outage.

## 11. Serving

### 11.1 Options

- `serve`: `true` (default where supported, §14 below) | `false`. A non-serving owner is today's
  `exclusive` mode.
- `ownership`: `prefer` | `yield` | `never` (§8.2).
- Owner-side limits, with these defaults (decided at ratification):

  | Limit | Default | Meaning |
  |---|---|---|
  | yield grace period | 5 s | how long a yielding owner (§8.2) lets running transactions finish before aborting them with `08006` |
  | shutdown grace period | 30 s | the same, for a graceful close (§8.3 step 3) |
  | idle-session timeout | off (`0`) | a connection with no open transaction is never closed for idleness, as PostgreSQL's `idle_session_timeout` default |
  | idle-in-transaction timeout | 60 s, **client write transactions only** | a client holding the global writer gate (§5.5) with no request in flight for this long is revoked: `25P03` at its next request. Read pins have no idle timeout: a pin stalls only reuse, never another writer |
  | maximum concurrent connections | unlimited (`0`) | when set, a further connection is refused at `hello` with PostgreSQL's `53300 too_many_connections` (registered with the phase that implements the limit) |

  `0` disables a timeout. These are owner settings; a client cannot change them.

### 11.2 Connection capabilities

An owner may restrict a connection to reads (`begin_write` fails `25006`), set by the owner's policy
or requested by the client (`--read-only` on the SSH helper). A read-only *owner* is different: it
hands ownership to a writer instead of refusing it (§8.2). That is the only privilege the page
service has: SQL privileges, the session capability envelope, and cost budgets are enforced by the
**client's** engine, because the client executes the queries. A process that can write pages can
corrupt the database regardless (§13).

## 12. Changes to the lock bundle and the shared protocol

Landed with phase 4 ([locking.md](locking.md) is the current bundle and ownership spec):

- Removed: shared mode — the join window and its `arrival` doorbell, alone↔shared transitions and the
  `transition` lock, meta refresh at transaction begin, the cross-process `writer` and `commit` gates,
  and append-only co-resident commits — and the `shared` value of `locking`. The option is `auto`
  (owner/client) | `exclusive` (a non-serving owner) | `none` (unchanged escape hatch). The test-only
  `service` value used during phases 2–3 is gone with it.
- Kept: the stable bundle and its identity rules, `presence` as the ownership lock, `arrival` as the
  marker-validation barrier, the hard-link rejection, the per-path in-process registry, fork
  invalidation, the open timeout, and fail-closed behavior on unsupported hosts.

**No migration.** Shared mode had no production users, so the page service replaced it as a clean
break, the way a `format_version` change does. The bundle has a new protocol marker, `protocol-v2`,
which a shared-mode binary rejects with `0A000`; a leftover `protocol-v1` marker is ignored. No
mixed-fleet rollout is designed or tested.

## 13. Security and trust

- **Same user only.** An owner serves only processes running as its own operating-system user. The
  endpoint sits in a directory private to that user (mode `0700`; on Windows, an endpoint whose ACL
  grants only that user). Directory search permission is the control every Unix enforces for
  sockets, and the kernel checks it on connect. So processes sharing a database at the same time
  must all run as one user: the owner's, which need not be the database file's owner. A process
  of another user can open the database only while no one else holds it, and it then becomes an owner
  that other users cannot reach. Root can connect as a client, but root as owner locks out the
  application's user, so tools should run as that user (`sudo -u app jed …`), not as root.
- **Why the endpoint does not mirror the file's permissions.** Unix permission bits are relative to
  the file's owner, and a process cannot create an endpoint owned by another user. Connecting to a
  socket also needs write permission on Linux. Any copy of the file's mode onto an endpoint the
  current owner created therefore locks out legitimate users (the file's owner, when another user
  owns; anyone with read-only access) and ignores ACLs and mandatory access control.
- **Cross-user access is not supported.** If a deployment needs it later, the exact mechanism known
  is for the client to open the database file itself and pass the open handle over the socket
  (`SCM_RIGHTS`); the owner checks with `fstat` that it is the same file and grants a read-write or
  read-only connection from the handle's access mode, so the kernel's own permission check
  decides. That needs FFI or a crate in Rust and a larger Node addon, and on Windows the equivalent
  is copying the file's ACL onto the endpoint.
- **Clients are trusted the way file openers are trusted.** A client writes raw pages; the owner's
  validation (§5.6) catches client bugs and protocol violations (`08P01 protocol_violation`), not a
  malicious client that constructs valid-looking pages. This is unchanged from shared mode, where any
  participant writes the file directly.
- **Remote access is SSH.** SSH provides authentication and encryption; `authorized_keys`
  `command="jed pages --read-only /srv/app.jed"` restricts a key to read-only page service on one
  database. jed never listens on a network port.
- **Untrusted SQL** (CLAUDE.md §13) is unaffected: it executes in some process's engine under that
  process's session envelope, exactly as today.
- **Ciphertext on the wire, key at the owner.** Encrypted pages cross the service as ciphertext
  (§5.1), but an owner, including an SSH helper or backup host that becomes owner, must hold the key.
  Only replication subscribers and replica apply loops (§10) can run keyless.

## 14. Platforms and cores

| Core / host | Serving owner | Non-serving owner | Client |
|---|---|---|---|
| Rust (and the Node and Ruby bindings over it) | yes | yes | yes |
| Go | yes | yes | yes |
| Native TS on Node | yes (service on a worker thread) | yes | yes (synchronous fetch through a worker thread) |
| Native TS in the browser (OPFS) | no | yes (today's exclusive access) | no |
| wasm32-wasip1 | no | no (explicit `none` only, as today) | no |

Native TS on Node keeps the place in multi-process access it had through shared mode
([locking.md](locking.md) §8), so the Layer 4 corpus keeps all three cores and production Node
does not depend on the Rust bindings.

**Native TS on Node.** Node's embedding API is synchronous (locking.md §8) and the engine runs on
one thread, so both roles move the socket work to a worker thread, in pure TypeScript with no new
native code (the OS locks still use `impl/ts/native-lock`):

- **Client.** A worker thread owns the connection. On a cache miss, or at `begin_read` and `commit`,
  the engine thread posts the request into a `SharedArrayBuffer` and blocks in `Atomics.wait` until
  the worker writes the reply. Node permits `Atomics.wait` on the main thread, and the TS core
  already uses it to sleep. The cost is one cross-thread handoff per round trip, and the calling
  thread blocks for the round trip, as it already does on a synchronous file read.
- **Serving owner.** The service cannot run on the engine thread. A client's request would wait for
  every synchronous owner query to finish, and an owner session waiting on the writer gate held by a
  client would block the very event loop that must receive that client's `commit`. So the service
  runs on a worker thread with its own file handle (still one process touching the file). The pin
  table, the writer gate, the committed meta, and the written-index window live in a
  `SharedArrayBuffer` that both threads update with `Atomics`. The engine thread waits on the gate
  with `Atomics.wait`, and at each transaction begin it compares the shared committed `txid` with its
  own (an atomic load, no syscall, so the alone fast path is unchanged). It adopts a client's commit
  by reloading its catalog from the committed meta, as for any foreign `txid`.
- **Performance caveat.** The service worker reads pages through its own file handle, not the engine
  thread's buffer pool, so clients of a TS owner are served from the OS page cache. Client page
  fetches pay the thread handoff above. Both are slower than the Rust and Go owners and clients,
  never different in results or cost. The benchmarks (§15) measure them.

Not every core needs to serve: the browser has one process, and WASI has no OS file locks.

**The stream transport (SSH, phase 5)** is Rust-only for now: `Database::open_remote` (the client)
and `Database::serve_page_stream` (`jed pages`, the serving end) — so the CLI, and Rust programs,
reach remote databases. The owner a `jed pages` relays to may be any core, and a `jed pages` that
owns serves local clients of every core (the `service_stream` process scenario). Go and TS clients
over a stream are not required by any use case yet (§1 use cases 1–4 are the CLI's); their local
clients already speak the identical protocol, so adding one is a transport, not a protocol, change.
Unix only, like serving.

**Endpoint mechanics (decided at ratification; exact layout in
[spec/pageservice/README.md](../pageservice/README.md) §5):**

- **Location.** Unix socket paths are limited to about 104–108 bytes, so a socket inside a deep
  bundle path can fail to bind. The endpoint is a stream socket inside a `0700` per-user directory in
  the bundle (`<path>.lock/uid-<uid>/endpoint`) when that path fits in 100 bytes, otherwise a short
  path in the user's private runtime directory (`$XDG_RUNTIME_DIR` on Linux, the per-user `$TMPDIR`
  on macOS, else a `0700` `/tmp/jed-<uid>` directory the owner creates and verifies), named from the
  owner's incarnation id. Either way the path is recorded in the owner record (§4.3), which is how
  clients find it. Linux's abstract namespace is rejected (no file permissions). Always using a
  runtime directory was rejected: the bundle keeps the endpoint beside the database it serves, and
  the runtime directory exists only to work around the path-length limit.
- **Windows.** Go supports `AF_UNIX` on Windows. Rust's standard library does not, and the choice
  between a named-pipe crate (a CLAUDE.md §14 dependency) and `unsafe` FFI (a CLAUDE.md §13 decision)
  is deferred until Windows serving is needed. **Until then the Rust core on Windows is a
  non-serving owner only** (`serve = false`): it can own a database, but it neither serves clients
  nor connects to another owner, so a second process opening the same database waits and fails
  `55006` exactly as `exclusive` does today. The table above reads "yes" for Rust only on Unix.

## 15. Verification

- **Process corpus rewrite.** The release-blocking Layer 4 corpus (locking.md §9) moves from shared
  mode to election, read and write through the service, promotion, yield, owner kill mid-commit,
  client kill holding a pin or the gate, and `commit_status` resolution, in every
  supported core pairing.
- **The conformance corpus through a client.** Run the entire sqllogictest corpus with the harness
  as a page client of another core's owner (for example a Go client against a Rust owner). Rows,
  errors, plans, and **costs** must be identical to a local run, because page reads are logical.
  This tests the protocol as hard as the corpus tests the engine, and exercises the cross-core
  byte-identity of every page both cores write.
- **Simulation testing.** Simulated processes become an owner plus clients over a simulated channel
  whose deliveries, disconnects, and owner crashes are seeded; the storage oracles are unchanged and
  the commit-role rule (§6) is part of the history.
- **Validation fuzzing.** Mutated `CommitDelta`s must fail `08P01` without writing any page outside
  the grant (§5.6), without changing a page reachable from a pinned snapshot or the fallback meta,
  and without publishing a meta.
- **Benchmarks** ([benchmarks.md](benchmarks.md)): alone-owner fast path (no regression), client
  warm-cache and cold-cache reads, client commit latency, and SSH-latency scans with and without
  read-ahead, for each core as owner and as client (the TS worker-thread costs, §14, included).

## 16. Phases

1. ✅ **Spec** (landed): message catalog and framing (`spec/pageservice/`), byte fixtures, error
   registrations (`08006`, `08007`, `08P01`, `25P03`).
2. ✅ **Read path** in Rust, Go, and TS (landed, under the test-only `locking = service`): election,
   endpoint, `hello`/`begin_read`/`read_pages`/`end_read`, client cache coherence, read-only clients
   ([spec/pageservice/README.md](../pageservice/README.md) §6–§7).
3. ✅ **Write path** in Rust, Go, and TS (landed, under the test-only `locking = service`): writer
   gate, reuse grant, `commit` with validation, owner adoption, the reclamation rule, `commit_status`,
   the idle-in-transaction timeout ([spec/pageservice/README.md](../pageservice/README.md) §3.5, §6.4).
   File attachments under the service, with the cross-owner gate order of §5.5, landed after phase 4
   ([spec/pageservice/README.md](../pageservice/README.md) §6.6).
4. ✅ **Handoff** (landed): promotion, yield, graceful close, timeouts
   ([spec/pageservice/README.md](../pageservice/README.md) §6.5); the process-corpus rewrite;
   simulation testing of an owner and its clients ([simulation-testing.md](simulation-testing.md) §14
   S8); retiring shared mode and the `shared` option; the new bundle marker.
5. ✅ **SSH** (landed, Rust and the CLI): the stream transport and `jed pages`
   ([spec/pageservice/README.md](../pageservice/README.md) §5.3, [cli.md](cli.md) §3), the CLI
   opening `ssh://[user@]host[:port]/path` through the system `ssh`, client-side read-ahead and
   subtree fetch over the existing `read_pages` (§5.3; no wire change). Proven by the
   `service_stream` process scenario (a Rust stream client against owners of every core), Rust
   host-API tests, and the CLI's e2e tests with a fake `ssh`; `bench:page_service` adds the
   `stream_cold_scan` lanes. **Open question:** how a deployment configures the remote binary
   (anything but `jed` on the remote `PATH`) and the ssh command — not decided, so nothing is
   configurable yet. Go and TS gain a stream transport when a program needs remote access from them.
6. ✅ **Backup** (landed): page-identical and compacted forms, the client walking its own pinned
   snapshot (§9, [spec/pageservice/README.md](../pageservice/README.md) §6.6); the host API's public
   name is open.
7. **Replication**: the tee, retained log, `subscribe`, lineage and timelines (format change),
   replicas and read replicas; amend replication.md.

**The page service replaced shared mode; the two never ran as alternatives.** During phases 2–3 the
service was reachable only through a test-only `locking = service` value, and shared mode remained what
`auto` selected; phase 4 switched `auto` to owner/client and removed shared mode in one step.

## 17. Decisions at ratification

The proposal's open questions were settled when this design was ratified (2026-10-16). Each decision
now lives in the body; this table is the index.

| Question | Decision | Where |
|---|---|---|
| Endpoint location | a `0700` per-user directory in the bundle, falling back to the user's runtime directory when the path is too long | §14, [spec/pageservice/](../pageservice/README.md) §5 |
| Rust on Windows | non-serving owner only until Windows serving is needed; no dependency now | §14 |
| Reclamation | owner-only free-list rebuilds, by maintenance commits | §6 |
| Grace periods and defaults | yield 5 s, shutdown 30 s, idle session off, idle in transaction 60 s (client write transactions only) | §11.1 |
| Freshness | `begin_read` is always a round trip | §5.3 |
| Log-append failure | break the stream and keep accepting writes | §10 |
| Attachments | every writer gate, local or remote, in real-path byte order; `lock_timeout_ms` as a backstop | §5.5 |
| Client request timeout | none | §4.1 |

**Deferred, not decided:** a query-level protocol over SSH. It would only win for very large scans or
extractions where shipping rows beats shipping pages; the RAM-sized design target suggests batched
page fetches cover most remote use.
