# Validated copy-on-write commits

The v33 durable commit protocol writes each dirty body page once, publishes its root
with a manifest of the expected page contents, then executes **one durability barrier**.
Recovery validates the manifest before accepting the root. This replaces the former
body-sync/meta-sync ordering protocol without introducing a WAL or redo copies.
The byte contract is [fileformat/format.md](../fileformat/format.md); the motivating
measurements remain in [durable-commit-experiments.md](durable-commit-experiments.md).

## 1. Commit and recovery

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
issuing writes. Shared-process mode supplies no reusable pages and continues append-only.

The durable algorithm produces the same allocation and bytes with fsync disabled, and
can be exercised over a MemoryBlockStore in tests. Ephemeral in-memory databases and temporary
domains may retain their existing bootstrap-meta path; they have no crash-recovery obligation.
Durability is intentionally absent when the host disables synchronization; logical SQL results,
types, and cost remain unchanged. Physical checksums and recovery reads are not SQL cost units.

## 3. Process crashes, failure, and shared access

Validation establishes readable completeness, not durable completeness. After a process
crash, the kernel cache can contain all of a failed commit even though a machine crash would
lose some of it. A writable opener therefore stabilizes the adopted generation with a
durable sync **before the first new write**. Otherwise a later transaction could overwrite
the only actually durable fallback, then a power failure could lose both generations.

This rule also applies to a shared process that adopts a generation written by another
process. It must stabilize that generation under the global writer gate before writing
again. A read-only adoption does not require a sync; adopting it cannot silently mark it
safe for a future writer. A locally acknowledged generation is already durable and does
not need a stabilization flush on every transaction.

Any write/sync failure poisons the storage handle: no later commit may continue from an
assumed prior root. Close and reopen through recovery. Shared-file failure retains the
coordinator gates until close, as [locking.md](locking.md) requires. This applies to the
alone, uncoordinated, attachment, and shared commit paths, not just co-resident writers.

Writer admission rejects an already failed handle before serialization can read inherited
pages. Serialization and shared-manifest planning run before starting the storage commit:
an encoding/size error discards the working snapshot without poisoning the pager or coordinator.
Only after preparation succeeds does the writer arm the commit guard and stabilize any adopted
generation, before its first allocation or write. A failed stabilization or any later failure
keeps the guard armed until reopen. Ordinary SQL transaction rollback rules still apply.

The lock-bundle protocol does not change: pages 0 and 1 remain the discovery location,
one global writer remains, and co-resident commits never reuse body pages. A transaction
begin takes `commit SH`, reads both meta pages directly, and validates a newly observed
generation before adopting its root. A writer takes `commit EX` for meta publication and
the final durability barrier. The no-peer fast path retains zero foreground coordination
syscalls and zero per-transaction meta reads.

## 4. Integration and verification

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
commit. The checked-in CSV retains every sample. macOS measurement remains pending.

Open-time work is proportional to the selected commit's dirty set, so reopening
after a large transaction reads more pages than the previous spine-only loader.
A retained shared pager caches its last validated metadata identity and dependency
set; an unchanged metadata page does not trigger repeated dirty-page reads.
