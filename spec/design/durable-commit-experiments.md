# Durable commit experiments

Status: historical research record, followed by the production v33 implementation in
[validated-cow.md](validated-cow.md). These initial experiments were scoped to one core;
the Rust prototypes live in `bench/durability` and use dirty-page writes captured from
the real SQL engine. No new dependency is required. The measurements below describe
the pre-v33 implementation; the production comparison is linked from the new design.

The pre-v33 recipe was body writes, durable barrier, alternate meta write, durable
barrier. A checksum on the meta alone cannot justify removing the first barrier:
the meta may reach storage before the body. The experiments compare:

1. The previous two-barrier recipe, with preallocated files.
2. A checksummed full-page redo WAL in a separate file, one barrier per commit,
   including the cost of periodic checkpoints and safe journal reuse.
3. The same redo protocol in a reserved region of the database file.
4. COW writes to their final locations plus a bounded commit manifest which binds
   the root to the exact newly written body pages. Recovery validates both the
   descriptor and its body dependencies before accepting the root. Reuse must
   preserve the preceding root and the dependencies needed to validate it.
5. On macOS, an ordering barrier for the body followed by a durable barrier for
   the meta. Linux cannot establish its performance or hardware guarantees.

Fault tests must cover reordered persistence and partial writes, not merely
process termination (which leaves the kernel page cache alive). Benchmarks must
report checkpoint-inclusive time, bytes written, barriers, recovery/reopen
correctness, and filesystem context. Artificial flush delays, if used, are a
sensitivity experiment and must never be presented as macOS measurements.

Promotion would require a shared byte specification and fixtures, all three
native cores, reader/reclamation and multi-process coordination, bounded memory,
file growth and checkpoint recovery, encryption/replication review, and macOS
hardware validation. The existing production format and locking protocol remain
authoritative throughout this experiment.

## Recommendation

First measure **body `F_BARRIERFSYNC` → meta `F_FULLFSYNC` on a Mac**. It is the
smallest plausible fix: one full drive-cache drain, no redo copies, no format
change. Apple's ordering contract supports its safety argument, but the Linux
work here cannot establish its macOS performance or hardware behavior.

For a portable one-sync design, **validated COW merits a production design
slice before adopting a WAL**. It was competitive with these WAL prototypes
while writing almost the same number of bytes as existing COW. Its
cost is new recovery metadata and stricter dependency/reuse bookkeeping, not a
second copy of every body page. This is a promising prototype, not a release-ready
storage mode.

Both WAL layouts work in the experiment. They move complexity into read overlays,
checkpointing, and reclamation. Their small timing differences change direction
across workloads; these measurements do not establish a reliable advantage for
either layout. Prefer one-file deployment if choosing the embedded version, not
an unsupported claim that sharing the file is faster.

## What the current implementation actually does

`impl/rust/src/file.rs::persist` writes dirty tree/catalog/free-list pages, calls
`Pager::sync`, writes an alternating meta page, and calls `sync` again.
`FileBlockStore` uses `File::sync_data`. Growth separately allocates real zeros
and calls `sync_all`. The experiment excludes that amortized growth cost by
preallocating the complete bounded workload, identically across protocols.

Linux `fdatasync` flushes data and metadata necessary to retrieve it; it can omit
unnecessary timestamp metadata. It is not a promise that no metadata ever needs
flushing. On Apple targets, Rust's `sync_data` and `sync_all` both request
`F_FULLFSYNC`. Go's file `Sync` does likewise, with a fallback to ordinary fsync
when unsupported. Consequently the macOS cost is more than a difference in inode
timestamp handling: the full device-cache flush is relevant.
([Rust implementation](https://github.com/rust-lang/rust/blob/master/library/std/src/sys/fs/unix.rs),
[Go implementation](https://go.dev/src/internal/poll/fd_fsync_darwin.go),
[Linux fsync contract](https://man7.org/linux/man-pages/man2/fsync.2.html))

The negative-control test removes the first barrier's effect: it persists a valid
new meta while leaving its fresh body absent. The native SQL reader fails. A
successful final sync would make that particular commit durable, but cannot repair
the unsafe crash window *before* the sync returns.

## Implemented protocols

### External and embedded redo WAL

Each commit encodes the real dirty pages and final meta in one CRC64-protected
frame, appends it, and syncs once. Frames are 4096-byte aligned so appending the
next frame cannot tear a sector holding an acknowledged preceding frame. Lengths,
record counts, offsets, sequence, padding, and checksum are validated. Recovery
accepts only a contiguous valid transaction prefix.

Every 64 commits (configurable), the prototype coalesces the last write to each
body offset, writes those pages, syncs the database, publishes the checkpoint meta,
and syncs again. It retains the entire journal until that finishes. The checkpoint
txid excludes stale old frames when the bounded journal starts again at offset
zero, avoiding a third reset barrier. The external version uses another file;
the embedded version uses an otherwise excluded extent on the same file.

**A tested checkpoint trap:** selecting the checkpoint meta slot by the final
transaction's parity is unsafe when each batch has even length. Every checkpoint
would overwrite the same slot. A torn later checkpoint can then destroy the txid
that identifies the retained WAL's beginning. These prototypes instead alternate
slots by **checkpoint**, independent of SQL transaction parity. Existing jed's
meta decoder permits this, but it would be an explicit new byte-format rule.

Checkpoint can overwrite pages reachable from the old checkpoint root. Recovery
must inspect only checksum-valid meta headers, replay the retained WAL, and
**then** open the native SQL image. Trying to load the old root first can encounter
incomplete checkpoint pages. Interrupted-checkpoint tests exercise this order.

Full-page logging need not double application write bytes: repeated writes can
collapse at checkpoint. It still approaches twice the bytes when coalescing is
ineffective. SQLite's documented WAL/checkpoint model provides the relevant prior
art; its SQL behavior is not used as an oracle here.
([SQLite WAL](https://www.sqlite.org/wal.html))

Production gaps: read-through page-to-log mapping; shared-process freshness and
reader pinning; recovery before opening pages; journal capacity/backpressure;
checkpoint errors/restarts; external-file identity, directory durability, and
backup handling; or embedded-region allocation/growth. A fixed reserved tail
whose location is known in advance is sufficient for this experiment, not a
general growing-database layout. In-file WAL also duplicates ciphertext/page
bytes at the storage seam; it is not automatically simpler than COW merely
because it has one pathname.

### Validated COW, without body redo

Body pages go to their final ordinary COW addresses. Instead of immediately
publishing ordinary meta, commit writes an alternate manifest containing the new
meta and `(offset, length, checksum)` for each body write, then syncs once.
Recovery validates the manifest and all listed body dependencies before accepting
its root. Inherited pages were durable before the preceding acknowledged commit;
recovery reads the latest dirty set, not the whole database.

If new writes would overlap **any** dependency of the previous manifest, the
writer first checkpoints the previous already-durable meta and syncs it. This
protects fallback even for dirty pages that were orphaned within their transaction
and are no longer reachable from its root. Merely relying on the existing
allocator's live-root protection is insufficient for those extra dependencies.
The latest descriptor remains intact across a torn checkpoint. The tested SQL
workloads needed only the final checkpoint; a synthetic orphan-reuse test proves
the extra barrier is taken when needed.

After a process-only crash, readable pages may still exist only in the kernel
cache. Resuming writes performs a sync before permitting another round of reuse;
the second crash must not destroy a fallback whose durability was only inferred
from readable cache contents. Failed writes/syncs poison the writer until recovery.

The descriptors have bounded, preallocated slots; a transaction too large for a
slot fails before I/O. This prototype uses FNV64 for accidental tear detection;
the WAL uses CRC64. Neither is authentication, and a production spec must select
shared checksums and scalable descriptor storage. Each prototype's checksum CPU
cost is included in its measurements. Region checks reject descriptor references
outside the bounded data region and references overlapping the meta.

Production gaps: byte-exact manifests and fixtures, large-transaction descriptor
allocation, all-core recovery, page-reuse accounting, atomic rebuild/compaction,
and locking/replication/encryption integration. The existing protocol's meta-only
freshness read must change: other processes cannot see a new manifest through the
current ordinary meta pages. Old binaries must reject the new format.

## Linux measurements, 2026-10-04

Machine: Linux 7.0.0-34-generic, x86_64 Intel Core Ultra 9 285K; writable ext4 on
`/dev/mapper/ubuntu--vg-ubuntu--lv`. `/tmp` is tmpfs and was deliberately not used.
Rust 1.99.0 release build. Measurements use actual synchronization calls, **no
artificial delay**, with four repetitions and rotating protocol order. This is
a shared workstation, not an isolated storage lab; small differences between
candidates should not be ranked as statistically established wins.

These are **SQL-derived persistence timings**, not end-to-end SQL timings. Trace
generation executes actual jed transactions and captures their production page
allocator/reuse behavior. Timed replay includes frame generation, checksumming,
write/sync calls, coalescing, and all periodic/final checkpoints. SQL parsing,
planning, read-overlay lookup, file growth, initial allocation, and multiprocess
locking are excluded. Every completed image is read by native jed and checked
against every expected ordered row and text payload.

Each table row below is the median of the four per-run means, including
checkpoints. Default 8 KiB pages; a 512-row table; 512 commits; WAL checkpoint
every 64 commits. Byte ratios count application writes, not SSD NAND amplification.

| Protocol | One-row commit | 64-row commit | Syncs/commit | Bytes vs current COW, one-row |
| --- | ---: | ---: | ---: | ---: |
| Current two-sync COW | 3.249 ms | 2.944 ms | 2.000 | 1.000× |
| External WAL | 2.210 ms | 1.868 ms | 1.031 | 1.133× |
| Embedded WAL | 2.088 ms | 2.050 ms | 1.031 | 1.133× |
| Validated COW | 2.081 ms | 1.847 ms | 1.002 | 1.004× |

Validated COW improved this persistence path by **1.56× / 1.59×** while retaining
single body writes. The regular transaction-batching option also matters: 64
updates in one current COW transaction amortize its persistence to about **46 μs
per updated row**, versus 3.249 ms for a separate one-row transaction. This does
not reduce the latency of an individual transaction or measure executor cost.

With **8192 rows and stride multiplier 137** (transaction starts at
`txn × batch × stride mod rows`; 256 commits, four repetitions), one-row means
were **3.155 / 2.165 / 2.015 / 2.015 ms** in the
same protocol order; 64-row means were **3.178 / 1.964 / 1.943 / 1.740 ms**.
WAL byte ratios grew to **1.374× / 1.404×**; validated COW stayed around **1.005×**.

Checkpoint cadence is load-bearing: at **one checkpoint per commit**, the WALs
needed **3 syncs/commit**, approximately **2.10× application write bytes**, and
took **5.838 / 5.939 ms**, versus current COW's **3.868 ms** for one-row commits
in that run. A WAL is not an unconditional win; its checkpoints must amortize.

Raw samples and p50/p99 summaries are checked in under
[`bench/durability/results`](../../bench/durability/results). The full-sync Linux
control used 256 commits, four repetitions, and checkpoint interval 64. Its
one-row means were **9.293 / 5.263 / 4.784 / 4.879 ms** in the same protocol
order (validated COW **1.90×** faster). Linux `fsync` results must not be called
macOS `F_FULLFSYNC` measurements.

## macOS side investigation

Darwin **does implement `fdatasync`**: XNU dispatches it to `fsync_common` with
`MNT_DWAIT`. Using it is not blocked by a missing kernel operation. However,
ordinary fsync/fdatasync do not provide the same explicit drive-cache drain as
`F_FULLFSYNC`, so replacing jed's barriers with them is not an equivalent
durability optimization.
([XNU VFS source](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c),
[Apple storage presentation](https://developer.apple.com/videos/play/wwdc2019/419/))

Apple documents `F_BARRIERFSYNC` as flushing the file and ordering preceding
flushed I/O before following I/O, without promising persistence when it returns.
It documents `F_FULLFSYNC` as draining the device cache. Therefore the supported
inference is **body → barrier → meta → full flush**, keeping durable completion
at acknowledgment. Unsupported barriers fall back to full flush; arbitrary I/O
errors abort. This still has two calls, but only one compulsory full cache drain.
([Apple fcntl contract](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/fcntl.2))

Implemented a standalone pure-Go probe comparing the two full flushes, the
barrier/full-flush proposal, and explicit unsafe fsync/fdatasync timing controls.
It preallocates real bytes, checks syscall errors, reports fallback counts and
latency distributions, and checks final content. Five protocol tests and vet pass
on Linux; `CGO_ENABLED=0` builds for Darwin arm64 and amd64. **It has not run on
macOS**, and content readback is not a power-cut test. Instructions and primary
source details are in the [probe README](../../bench/durability/macos_sync_probe/README.md).

Production would need a distinct ordering operation in the block-host seam:
silently weakening the existing `sync()` violates its durability contract. The
Go probe needs no dependency, cgo, or unsafe code. Rust std has no separate barrier
operation; a production Rust host adapter must resolve that API boundary under
the repository's dependency/memory-safety policy.

## Other options and verification boundaries

Explicit transaction batching is available now and was exercised by the 64-row
lane. Group commit could amortize syncs across independently acknowledged
transactions, but needs a durable-ack queue and cannot simply expose unflushed
roots to the current reuse path; it was not implemented here.

Linux `sync_file_range` is not an equivalent barrier: it does not flush all
required metadata or volatile drive caches. `O_DIRECT` alone is not durability.
Combining a meta write with `O_DSYNC`/`RWF_DSYNC` may save a syscall, and supported
direct-I/O paths can use FUA, but these still require ordered body/root phases;
they were researched, not benchmarked or adopted.
([sync_file_range contract](https://man7.org/linux/man-pages/man2/sync_file_range.2.html),
[kernel direct-I/O design](https://docs.kernel.org/filesystems/iomap/operations.html),
[writeback cache/FUA contract](https://docs.kernel.org/block/writeback_cache_control.html))

The Rust research suite has 26 passing tests plus the opt-in benchmark. It
includes 131,072 WAL sector-subset states across append/reuse; 25,936 COW
sector-subset/torn-byte cases; partial checkpoint and stale-tail cases; repeated
reuse; poisoned I/O; and real SQL recovery across 64 interrupted COW commits.
Tests check old-or-new valid recovery as appropriate, not arbitrary hybrid state.
These finite deterministic models are evidence for the implemented protocols,
not a proof against all devices, checksum collisions, or actual power failure.
The complete Rust library suite also passes: **853 tests**, with only the disk
benchmark ignored by default. No production SQL or storage behavior changed,
so the PostgreSQL oracle and other cores were outside this research validation.
An independent `strace` smoke run confirmed **90 `fdatasync` calls** for the
eight 8-commit samples, exactly matching the summed protocol counters, plus
**20 initialization/directory `fsync` calls** outside their timers. Traced timings
are excluded from the performance dataset.
