# Validated copy-on-write commits

The v33 durable commit protocol writes each dirty body page once, publishes its root
with a manifest of the expected page contents, then executes **one durability barrier**.
Recovery validates the manifest before accepting the root. This replaces the former
body-sync/meta-sync ordering protocol without introducing a WAL or redo copies.
The byte contract is [fileformat/format.md](../fileformat/format.md); the motivating
measurements remain in [durable-commit-experiments.md](durable-commit-experiments.md).

## 1. Commit and recovery

An unchanged database domain never enters this protocol, including recovered-generation
stabilization. A no-op writable transaction does no writes or barriers; the next actual write
still performs every required stabilization and commit barrier (transactions.md §2.1).

An ordinary incremental commit performs:

1. Allocate and encode dirty tree, overflow, catalog, and free-list pages while preserving
   the previous snapshot and its recovery dependencies.
2. Write those pages to their final locations. Compute a CRC-64/ECMA-182 digest of each
   complete encoded page, including its existing CRC-32 field. Sort the `(page, digest)`
   entries by increasing page number.
3. Write any manifest overflow pages. Small manifests fit entirely in the unused part of
   the existing meta page; large transactions use a page chain, without a fixed manifest
   capacity or a reserved region outside the ordinary allocator.
4. Write the alternate meta slot, including the new catalog root, logical high-water,
   free-list head, inline entries, and an identity digest of the overflow chain.
5. Execute the host's existing durable `sync`. Only after success publish the in-process
   snapshot and acknowledge the commit.

The host barrier retains its existing durable-completion semantics. This change does not
replace a macOS full device-cache flush with ordinary `fsync` or `fdatasync`. Preallocation
can still require an extra amortized flush when the file grows. Stabilizing a recovered or
foreign generation (§3) can also add a flush; **one flush describes steady-state commits**.

Opening validates the two meta candidates in descending transaction-id order (slot 0
wins a tie). A candidate is usable only when its entire meta checksum, descriptor shape,
overflow chain identity, and every listed page digest match. Inherited pages need not be
rehashed: they were durable before this writer began. Only the current dirty set is new.
Reading and hashing happens through the pager, one page at a time, rather than loading the
database into a recovery buffer. Recovery does not replay or copy body pages.
After selecting a complete candidate, ordinary catalog/free-list decoding can still reject
invalid logical contents with `XX001`, without retrying an older generation. For example,
a checksummed free list that names a protected manifest dependency is corrupt. Recovery
validation proves write completeness, not full semantic database consistency.

| Surviving state after a crash | Decision |
|---|---|
| Torn meta or incomplete descriptor chain | Try the preceding candidate |
| Complete descriptor, missing/torn/stale dirty page | Try the preceding candidate |
| Complete descriptor and all expected pages | Accept this generation |
| Neither candidate validates | `XX001` |
| Host read fails with an I/O error | Propagate the I/O error |

A readable but unacknowledged complete transaction can be recovered as committed. This
is the ordinary indeterminate outcome of a crash during commit. Corruption of a latest
commit's dependency after acknowledgment is indistinguishable from incomplete persistence:
recovery can select the preceding snapshot in that case too. The checksum guarantee is
accidental corruption/torn-write detection, not authentication or immunity to collisions.

Whole-file `create` and `to_image` write **bootstrap meta** with zero manifest fields;
the same rule applies to atomic file compaction ([api.md](api.md) §2.6), which writes the image at `txid + 1`. They already publish a complete independently durable image; they need no
incremental dependency manifest. Both slots carry the same bootstrap root. The exact-version
format break rejects all pre-v33 readers/writers; there is no mixed-version migration.

## 2. Reuse proof and deterministic allocation

A root alone does not identify every dependency of its manifest. Dirty pages that became
orphans during serialization, free-list pages, and manifest overflow pages must survive
until another generation succeeds, even if no live tree references them.

The committed free list therefore excludes **every page written by that commit**, including
its free-list and manifest pages. The existing reclamation walk already treats all freshly
written tree/catalog/overflow pages as live. Manifest allocation extends that invariant.
The next commit allocates only from that persisted free list or the high-water, subject to
the existing reader watermark and process-presence rules. It cannot overwrite its fallback's
recovery dependencies. It may put old dependencies into its *new* free list; they become
reusable only by a subsequent commit, when the meta slot that required them is replaced.
No metadata checkpoint is required to permit normal reuse.

Manifest pages use safe free pages before extending the file. Always appending them would
grow even a repeatedly updated tiny database. Free-list serialization and manifest size
depend on one another, so all cores use this monotone planning algorithm:

1. Begin with `M = 0` reserved manifest pages.
2. Reserve `M` page ids from the safe remaining free list in ascending order, then from the
   high-water if necessary. Exclude those ids from the prospective persisted free list and
   the safe pool used to serialize that list.
3. Run ordinary free-list serialization. Let `N` be the number of dirty body pages plus
   the resulting free-list pages, `I = floor((page_size - 64) / 12)`, and
   `O = floor((page_size - 32) / 12)`.
4. Set `required = ceil(max(0, N - I) / O)`. If `required > M`, retry the plan with
   `M = required`. Otherwise keep the plan. Do not shrink `M`.

Reservation may reduce the free-list page count. Consequently a final overflow page may
have zero entries; the reserved chain still has exactly `M` pages, maximal prefix filling,
and valid headers/digests. Checked arithmetic must reject page-count/size overflow before
issuing writes. A page client plans its commit with this same algorithm, using the persisted
free list only under the owner's reuse grant ([page-service.md](page-service.md) §6).

The durable algorithm produces the same allocation and bytes with fsync disabled, and
can be exercised over a MemoryBlockStore in tests. Ephemeral in-memory databases and temporary
domains may retain their existing bootstrap-meta path; they have no crash-recovery obligation.
Durability is intentionally absent when the host disables synchronization; logical SQL results,
types, and cost remain unchanged. Physical checksums and recovery reads are not SQL cost units.

## 3. Process crashes, failure, and multiple processes

Validation establishes readable completeness, not durable completeness. After a process
crash, the kernel cache can contain all of a failed commit even though a machine crash would
lose some of it. A writable opener therefore stabilizes the adopted generation with a
durable sync **before the first new write**. Otherwise a later transaction could overwrite
the only actually durable fallback, then a power failure could lose both generations.

This rule also applies to a process that becomes owner, by election or promotion
([locking.md](locking.md) §4), and adopts a generation a previous owner wrote. It must
stabilize that generation before writing again. A read-only adoption does not require a sync; adopting it cannot silently mark it
safe for a future writer. A locally acknowledged generation is already durable and does
not need a stabilization flush on every transaction.

The same reasoning covers the file's **directory entry**. Compaction and `create` replace the
file by renaming a synced temp file over it and then syncing the directory
([api.md](api.md) §2.6). A process that dies between the rename and the directory sync leaves
the new file readable but its entry not durable: the next opener adopts it, and a later power
failure can revert the rename and bring back the older file, losing every commit made since —
syncing the file itself never makes its directory entry durable. So the first durable commit
after each open syncs the database's directory, once, before any write (best effort, like
every directory sync; skipped under `fsync=off`). This applies to every commit path: an
owner's own, a page client's (which the owner writes), and an attachment's. The deterministic simulator found this gap
([simulation-testing.md](simulation-testing.md) §14).

Any write/sync failure poisons the storage handle: no later commit may continue from an
assumed prior root. Close and reopen through recovery. An owner whose storage fails stops
serving and closes before releasing `presence EX` ([locking.md](locking.md) §2). This applies
to the owner, uncoordinated, and attachment commit paths alike.

Writer admission rejects an already failed handle before serialization can read inherited
pages. Serialization and manifest planning run before starting the storage commit:
an encoding/size error discards the working snapshot without poisoning the pager.
Only after preparation succeeds does the writer arm the commit guard and stabilize any adopted
generation, before its first allocation or write. A failed stabilization or any later failure
keeps the guard armed until reopen. Ordinary SQL transaction rollback rules still apply.

Multiple processes do not change the commit recipe: pages 0 and 1 remain the discovery
location, and every commit to the file is written by its owner, so no transaction begin
re-reads meta. A page client's commit is validated by the owner before it is written
([page-service.md](page-service.md) §5.6). The owner's fast path retains zero foreground
coordination syscalls and zero per-transaction meta reads.

## 4. Batched durability (`synchronous = off`)

Under the relaxed `synchronous` mode ([transactions.md](transactions.md) §9) a commit is atomic and
visible as soon as its meta is written, but its durable barrier is deferred: one later barrier, a
**flush**, makes a whole run of commits durable at once. The protocol above needs four extra rules,
because the meta of a commit that is not yet durable must not overwrite the only durable fallback,
and the manifest of the newest meta must vouch for every page written since that fallback.

Each durable file domain (the main file, each writable file attachment) keeps two facts in memory:

- the **floor**: the newest generation known durable, and the meta slot that holds it;
- the **pending set**: the page id and CRC-64 digest of every body page (tree, catalog, overflow,
  GiST, and free-list pages — never meta or manifest pages) written by a commit after the floor.

The rules:

1. **Slot.** A commit writes its meta to the slot that does **not** hold the floor. When nothing is
   pending, the floor is the previous generation, so this alternates exactly as `txid & 1` does under
   `synchronous = on`; while commits are pending, every one of them overwrites the same non-floor
   slot. At open the floor slot is the slot of the selected candidate, or slot `txid & 1` when both
   slots carry the selected `txid` (`create`, compaction). An adopted generation is stabilized before
   the first write (§3), so it is the floor by then.
2. **Cumulative manifest.** A commit's manifest lists the pending set plus its own dirty pages, in
   page order. A meta that validates therefore proves every page written since the floor that it
   could depend on, not only its own.
3. **No reuse while pending.** A commit that begins with a non-empty pending set allocates every
   page — tree, catalog, free-list chain, and manifest — from the high-water, never from the free
   list. So no page is written twice between two flushes: each pending digest stays exact, and no
   dependency of the floor, nor of any pending generation, is ever overwritten. The first commit
   after a flush plans exactly as under `synchronous = on`.
4. **Free-list exclusion.** A commit's persisted free list excludes the whole pending set, since
   every pending page is a dependency of its manifest. Reclamation still runs; a page orphaned
   during a batch is listed by the first reclamation after the floor moves past it.

A relaxed commit writes its pages and meta as usual and publishes **without** the barrier. A
**flush** executes the host's ordinary durable `sync`; on success the floor becomes the newest
generation (in the slot it was written to) and the pending set empties. The flush points:

- **The batch cap.** When a commit leaves the pending set with at least
  `I = floor((page_size − 64) / 12)` entries — the inline manifest capacity (§2) — the batch ends
  with it and a flush follows. The cap bounds a pending commit's manifest, the pages recovery
  validates, and the growth that rule 3 defers, to about one page's worth of entries; at 4 KiB pages
  a one-row commit dirties three or four pages, so one barrier serves about a hundred commits.
- **Close** (and detaching a file attachment) flushes before releasing the file; a failed flush is
  the error `close` reports.
- **A page client's `begin_write`.** The owner flushes before granting the gate
  ([page-service.md](page-service.md) §6), so a client plans against a durable floor with its own
  manifest only (the client cannot see the owner's pending set).
- **Compaction** writes a new, already durable image and empties the pending set; any trailing
  truncation of the file likewise runs only when nothing is pending.
- **Stabilization** of an adopted generation (§3) is a flush.

The cap flush may run **in the background** (a goroutine in Go, a thread in Rust): the commit that
reached the cap returns at once, and the domain's next commit, compaction, flush, or close first waits
for that barrier to finish. TypeScript runs it inline, since its engine has no second thread, and so
does the deterministic simulator in every core. Because every later commit plans only after the
flush completes, which commits a flush covers, which slot each meta lands in, and every page id are
functions of the commit sequence alone — the same in every core and with or without the background
thread. Background flushing changes only *when* the barrier runs.

A flush that fails poisons the handle like any failed barrier: the next commit and `close` report it.
The commit that reached the cap is already published, so a failed cap flush does not fail it. A
failed barrier may also have dropped pending pages from the operating system's cache (Linux
"fsyncgate"), so pages the published snapshot needs could now read stale: the cap flush therefore
**revokes** the storage, and every later read fails `58030` too until the database is reopened through
recovery — never `XX001` or wrong rows.

**Guarantee.** After a power loss or an OS crash, recovery selects a generation between the floor —
the newest generation a flush, a close, a stabilization, or an acknowledged `synchronous = on` commit
made durable — and the newest commit attempted. Acknowledged commits after the floor may be lost,
newest first, but a recovered generation is always one complete committed snapshot: the rules above
keep the floor's own dependencies intact and reject any newer meta whose pending pages did not all
land, so atomicity is never lost and recovery never fails with `XX001`. Every relaxed commit has
issued all of its writes to the operating system, meta last, before it returns, so a crash of the
process alone, with the kernel surviving, loses no acknowledged commit. (The simulator's process-crash
model lets each write of the crashing step be issued or not independently, which is more pessimistic;
it checks only the floor.)

## 5. Integration and verification

The manifest hashes encoded logical pages at the existing pager seam. A future encryption
decorator must continue exposing exactly those bytes after decryption; the protocol makes
no assumption that the underlying stored bytes are plaintext-comparable. A block-delta
replication sink must ship manifest pages and meta alongside body pages and preserve the
same committed-batch durability rule. This remains block replication, not WAL replay.

The validation obligations are shared byte fixtures (including torn/stale dependencies),
shared SQL recovery-and-continue corpus entries, real mixed-core process handoffs, and
per-core fault-device tests for arbitrary persistence order and partial page writes.
Tests must cover overflow manifests, free-list reuse, orphan dependencies, repeated crashes
with stabilization, poisoned handles, attachments, and bounded file growth. Existing
query/concurrency/cost corpus entries continue to constrain behavior. Benchmark the real
production commit path with synchronization enabled; proof-of-concept timings alone do not
establish production performance or macOS results.

The implemented production path is measured in the
[durable Go benchmark](../../bench/durability/README.md#production-validated-cow-benchmark).
Six paired Linux/ext4 samples against master `4f1a3b27` reduced median end-to-end
commit latency from 3.677 to 2.292 ms for one-row updates and from 3.880 to 2.349 ms
for 64-row batches. These inline-manifest workloads wrote identical application
byte counts; including allocation flushes, barriers fell from 2.004 to 1.004 per
commit. The checked-in CSV retains every sample. On an Apple M3 Max with APFS, the same
paired benchmark measured 8.998 → 5.228 ms (1.72×) and 9.109 → 5.185 ms (1.76×); there
each commit's one remaining `F_FULLFSYNC` costs ~4 ms
([macOS measurements](durable-commit-experiments.md#macos-measurements-2026-10-13)).

Open-time work is proportional to the selected commit's dirty set, so reopening
after a large transaction reads more pages than the previous spine-only loader.
A retained shared pager caches its last validated metadata identity and dependency
set; an unchanged metadata page does not trigger repeated dirty-page reads.
