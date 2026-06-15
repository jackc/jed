# Replication — block-shipping the commit delta

> How a database's committed changes are shipped to another copy. **Decided: physical
> block-shipping — a stream of per-commit page-deltas — not a write-ahead log.** This is a
> *design* doc for a **deferred, not-yet-built** capability; it fixes the architecture so
> nearer-term work (the storage-host seam, [hosts.md](hosts.md); encryption, [encryption.md](encryption.md))
> does not foreclose it, and records *why* blocks suffice and a WAL is not needed. The change
> object it ships is the one [storage.md](storage.md) §4 already produces. When a decision here
> changes, update [CLAUDE.md](../../CLAUDE.md) §9 and [storage.md](storage.md) §6 in the same edit.

## 1. The decision, and why it falls out of the existing design

**Replication ships physical page-deltas, one record per commit, in `txid` order. There is no
write-ahead log.** This is not a reluctant simplification — it is the natural shape of an
engine whose commit model is already copy-on-write (storage.md §4).

The two classic reasons an embedded engine grows a WAL are **crash atomicity** and
**reader/writer concurrency**. jed has *both already*, for free, from the copy-on-write +
immutable-snapshot model:

- **Atomicity** comes from writing dirty pages to fresh slots and publishing the new root with
  a single atomic meta-slot swap (storage.md §4, [TODO.md](../../TODO.md) "WAL stays deferred").
  A crash recovers to a valid snapshot — prior or new — never a torn mix (storage.md §7).
- **Concurrency** comes from immutable snapshots: lock-free readers run concurrently with the
  single writer and never block, not even during the commit swap (transactions.md §10).

So the usual motivation for a WAL is **already discharged**. The only live reason a change-log
would exist in jed is *replication itself* — and for that, blocks are not merely sufficient,
they are what the engine hands you at every commit.

## 2. The change record is already produced at commit

Every commit produces a **self-contained physical delta** (storage.md §4): the copy-on-write
path the mutation touched (root→leaf of each modified table's B-tree) plus the rewritten
catalog chain, written to fresh appended or free-list slots, followed by the atomic meta-slot
write that publishes the new root. Capture that set and you have a complete description of how
to transform snapshot *N* into snapshot *N+1*:

```
CommitDelta {
  txid:    u64                       # the new committed version (storage.md §4)
  pages:   [ (page_index, bytes) ]   # every body page this commit wrote, post-codec (§4)
  meta:    (slot_index, bytes)       # the meta-slot write that publishes the new root
}
```

This is a **physical, per-commit** change record by construction. No separate log format, no
extra write path, no logical decoding — the delta is exactly the bytes the commit already
streams across the block seam ([hosts.md](hosts.md) §2). Producing the replication stream is
therefore a **tee at the seam** (hosts.md §6): copy the dirty-page writes and the meta write
into a `CommitDelta` as they go to the file.

## 3. Why blocks suffice (the WAL-vs-blocks axis, decomposed)

"WAL vs. block-shipping" conflates two independent axes. Naming them shows the choice is
already made on one and deliberately deferred on the other:

| axis | the two ends | jed's position |
|---|---|---|
| **representation** | physical (pages) ↔ logical (row-ops / SQL) | **physical**, free from copy-on-write (§2) |
| **cadence** | continuous log (pre-commit) ↔ per-commit snapshot-delta | **per-commit**, the only observable granularity (§5) |

A WAL is a *continuous, often-physical* log. jed's copy-on-write commit already occupies the
**physical + per-commit** quadrant — so a WAL would add the *continuous* (sub-commit) cadence
and nothing else of value for an embedded single-writer engine, where commits are the unit of
visibility anyway (a reader can never observe a sub-commit state, transactions.md §5). The only
genuinely *new* capability a WAL-style log could add is the **logical** representation — and
that is the kept-open door (§7), a separate higher-layer feature, **not** a competitor to
block-shipping.

What block-shipping inherits *for free* that a logical log would have to re-earn:

- **Cross-core byte-identity.** The on-disk format is a §8 byte contract (rust == go == ts ==
  ruby produce byte-identical files). A page-delta captured on any core therefore applies
  byte-identically on any other core's file. A logical stream would instead re-run the executor
  on the replica — correct, but redundant work, and it reintroduces every divergence hotspot
  (§8) the byte contract closes.
- **Trivial ordering.** Single writer + total `txid` order (transactions.md) means the stream is
  just `CommitDelta`s in ascending `txid`. No log-sequence-number scheme, no reordering, no
  conflict resolution (there are no write-write conflicts — transactions.md §5).
- **Replica crash-safety = primary crash-safety.** The replica applies a delta with the *same*
  recipe as a local commit (§4), so it inherits the same atomic-snapshot guarantee (storage.md
  §7) with no new reasoning.

## 4. The stream and the replica

**Producing.** The primary's replication tee (hosts.md §6) accumulates each commit's writes
into a `CommitDelta` and emits it when the commit's meta `sync()` completes — i.e. only
**durably-committed** deltas are ever shipped (a rolled-back or crashed-mid-commit transaction
never produced a published meta slot, so it never emits). Deltas emit in `txid` order because
commits are serialized by the single writer.

**Applying.** A replica is an ordinary database file plus an apply loop:

1. Receive `CommitDelta{txid, pages, meta}`; require `txid == replica.committed_txid + 1`
   (gap → the replica is behind; request from the last applied `txid`, or re-seed from a base
   image — §5).
2. `write_at` every body page; `sync()`.
3. `write_at` the meta slot; `sync()`.

Steps 2–3 are **the local commit recipe verbatim** (storage.md §4): body pages durable, *then*
the meta publish. So a crash mid-apply recovers exactly as a crash mid-commit does — the
replica is left on the prior valid snapshot until the meta is durable, then atomically on the
new one (storage.md §7). The replica needs no bespoke recovery logic.

**The keyless-replica property.** Because the replication tee sits **below** the encryption
codec (hosts.md §6, encryption.md §2), the `pages` bytes are **ciphertext**. The replica stores
them directly — it never decrypts to apply. So a pure backup/standby replica **holds no key**;
it can mirror an encrypted database without the ability to read it, and needs the key only if
and when it is promoted to serve queries. This falls out of the layering and is a reason to
keep encryption a codec above the seam rather than a host duty.

**Base image + tail.** A replica is bootstrapped from a **base image** (the whole-image
serializer `create` uses, storage.md §4 — a single `CommitDelta`-equivalent covering every
page at some `txid`) and then kept current by the tail of per-commit deltas. Re-seeding after
an unrecoverable gap is "ship a fresh base image, resume the tail."

## 5. Point-in-time recovery, at commit granularity

Retaining the delta stream (rather than only applying it) *is* point-in-time recovery: a base
image at `txid T₀` plus deltas `T₀+1 … Tₙ` reconstructs the committed state at **any** `txid`
in that range by applying up to that point. The granularity is **per commit** — which is the
only granularity that means anything, since no sub-commit state is ever observable
(transactions.md §5). So jed gets PITR with **no separate WAL archive format**: the archive is
just the retained `CommitDelta` stream. (This is a *consequence* available if the stream is
retained; the retention policy/tooling is out of scope here.)

## 6. Determinism & conformance status

Replication is **outside the conformance contract**, like benchmarks (CLAUDE.md §10): *when* a
delta is shipped or applied is timing, not SQL-level determinism. But its *content* is fully
deterministic — the `CommitDelta` for a given `(committed state, transaction)` is byte-identical
across cores (it is the §8 file bytes), so a cross-core/cross-host **apply-equals-original**
check is a natural per-core test (apply a captured delta stream on each core, assert the
resulting file is byte-identical to the primary's — the same spirit as the file-host parity
test, hosts.md §5). No corpus capability; no determinism-ledger exception (the delta carries no
new nondeterminism — replication does not change what any query observes).

## 7. The honest cost, and the logical door left open

Block-shipping has one real cost, to record plainly:

- **Write/replication amplification.** A one-byte change dirties the whole leaf page it lives in
  (default 8 KiB), plus the copy-on-write interior path root→leaf, plus the rewritten catalog
  chain. Block-shipping replicates *all* of those pages, not the one changed value. A logical
  changeset (`UPDATE t SET x = … WHERE pk = …`) would be far smaller on the wire. For the
  primary replication use case of an embedded engine — a warm standby / read replica / backup
  **mirror** that is byte-identical to the primary — this amplification is an acceptable,
  well-understood trade (it is the same physical-replication trade PostgreSQL streaming
  replication and bbolt-style mirroring make). It would matter for a bandwidth-constrained or
  fan-out-heavy deployment.

- **Homogeneity requirement.** Physical replication requires the replica to share the primary's
  `format_version` and `page_size` (it applies raw pages). A replica on a different schema,
  engine version, or a non-jed consumer cannot be served by block-shipping.

Both of those — compact wire format and heterogeneous/CDC consumers — are exactly what a
**logical changeset stream** would address: a row/operation-level record emitted at the
mutation layer (above storage), in the spirit of SQLite's session extension. It is a **door
kept open, not built**: it sits at a *different* seam than block-shipping (the row-mutation
layer, not the page layer), so it composes with rather than replaces this design, and it would
re-incur the divergence-hotspot work (§8) that the physical path avoids. Nothing here forecloses
it; nothing here schedules it.

## 8. Open / deferred

- **The replication tee + `CommitDelta` wire form** — ⏳ designed here, **not built**. The tee
  point is fixed (hosts.md §6, below the codec); the on-wire framing, transport, and
  flow-control are unspecified (transport is the host's concern, like the storage host itself).
- **Replica apply loop + base-image seeding** — ⏳ designed (§4), not built.
- **PITR retention/tooling** — ⏳ a consequence of retaining the stream (§5); policy/tooling out
  of scope.
- **Logical changeset stream** — kept-open door (§7), a separate higher-layer feature; not
  scheduled.
- **Multi-writer / bidirectional / conflict resolution** — out of scope by construction:
  single writer, one committed version (CLAUDE.md §3). Replication is one-directional
  primary→replica.
