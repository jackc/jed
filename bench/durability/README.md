# Durability experiments

The original research prototypes remain alongside the production validated-COW benchmark
below. See the [research record](../../spec/design/durable-commit-experiments.md) and
[production design](../../spec/design/validated-cow.md).

`experiment.rs` runs as an in-crate Rust test so the real SQL engine, pager, and
format remain private. It captures the production COW allocator's exact writes
for SQL UPDATE transactions, then times identical write sets through:

- The former body-sync / meta-sync COW (two-barrier control).
- `wal.rs`, external full-page redo journal.
- `wal.rs`, the same journal in a reserved in-file extent.
- `cow.rs`, final-address COW with two validating commit manifests.

No dependencies or runtime/public API changes. The default Rust test run includes
the deterministic recovery tests and ignores the disk benchmark.

```sh
mise run bench:durability
```

This runs the recovery tests, then records benchmarks in
`bench/results/durability-<timestamp>-<pid>/run.log`. Choose a directory on the
actual disk; `/tmp` is tmpfs on the research machine. Files are created exclusively
and removed after verification. Each sample preallocates real zero-filled space
and syncs allocation and its directory before timing. No PostgreSQL server is
needed: SQL semantics are unchanged, and this tests byte persistence/recovery.

Optional environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `JED_DURABILITY_DIR` | generated results directory | Real disk directory for images and log |
| `JED_DURABILITY_COMMITS` | 1024 | Transactions per sample |
| `JED_DURABILITY_REPEATS` | 3 | Repetitions, rotating protocol order |
| `JED_DURABILITY_CHECKPOINT` | 64 | WAL checkpoint interval in commits |
| `JED_DURABILITY_ROWS` | 512 | Seeded table row count |
| `JED_DURABILITY_STRIDE` | 1 | Multiplier: starting row is `txn × batch × stride mod rows` |
| `JED_DURABILITY_SYNC` | data | `all` selects `File::sync_all`; otherwise `sync_data` |
| `JED_DURABILITY_DELAY_US` | 0 | Additional sleep per sync; sensitivity testing only |

Both one-row and 64-row transactions run. Initialization, SQL execution, and
trace generation are outside the timer. Encoding, checksumming, writes, syncs,
coalescing, periodic checkpoints, and final checkpoint are inside. Per-commit
latency includes the checkpoint it triggers. File growth, WAL reader-overlay
lookups, and multi-process coordination are **not** measured. The trace and its
checkpoint coalescing are held in RAM by this bounded research harness; this is
not a proposal for a whole-engine memory strategy. COW uses FNV64 and WAL CRC64,
so checksum costs are those of each prototype, not a standardized common codec.

Every final image is reopened by jed and every ordered SQL row/payload is checked.
Separate tests recover real SQL pages before checkpoint, during an interrupted
checkpoint, and when the next transaction's body persists without its manifest.
The model tests enumerate reordered/torn persistence; clean reopen or killing a
process alone would not establish power-loss safety.

Summarize raw output (median across repetitions of each metric):

```sh
ruby bench/durability/summarize.rb bench/results/durability-*/run.log
```

Checked-in [Linux results](results/linux-ext4-2026-10-04.csv) derive from the
adjacent raw measurements. There is no synthetic-delay result in that dataset.

For macOS, the separate [pure-Go syscall probe](macos_sync_probe/README.md)
implements the no-format-change barrier/full-sync proposal and the fdatasync
side investigation. It cross-compiles on Linux; execute it on a Mac for actual
timings.

## Production validated COW benchmark

Run the actual Go SQL commit path with durable synchronization enabled:

```sh
mise run bench:durable_commit
```

The task uses a Go build overlay to include `production_go_bench_test.go` without
editing the core directory. It records six samples of 256 commits for each of
one-row and 64-row updates over a 512-row table. `JED_DURABILITY_COMMITS` and
`JED_DURABILITY_REPEATS` override those counts. `JED_DURABILITY_BENCH_DIR` selects
an existing persistent filesystem location; by default the task creates a
checkout-local `bench/results/production-durability-<timestamp>-<pid>` directory.
The images are removed after every sample; `run.log` is retained.

These timings include SQL parsing/execution, page allocation/reclamation,
checksumming, writes, real flushes, and file growth. Initialization and final
reopen verification are outside the timer. Each final image is reopened and every
row's accumulated update count is checked. Counters include the full flush and
zero allocation done by file growth. No artificial flush delay is used.

The [six-run Linux comparison](results/production-go-linux-v33.csv) used Go 1.27.1,
Linux/ext4 on `/dev/mapper/ubuntu--vg-ubuntu--lv`, Intel Core Ultra 9 285K, default
8192-byte pages, and the same persistent-filesystem directory for both revisions.
The baseline was master `4f1a3b27530b8af1d1ff64380ddda00bb34069d2`; copy this same
benchmark source to `impl/go/production_durability_bench_test.go` in an archive or
checkout of that commit, set `JED_DURABILITY_BENCH_DIR`, and run:

```sh
go test -run '^$' -bench BenchmarkProductionDurableCommit -benchtime=256x -count=6
```

The captured comparison alternated revision order between paired runs. It retains
all samples, including a contended first 64-row sample. Median time per commit:

| Rows updated | Master, two flushes | Validated COW | Speedup |
| --- | ---: | ---: | ---: |
| 1 | 3.677 ms | 2.292 ms | 1.60× |
| 64 | 3.880 ms | 2.349 ms | 1.65× |

Both revisions wrote identical application byte counts in these workloads:
41,248 and 42,368 bytes per commit respectively. Their manifests fit within the
meta page. Including one growth event per 256-commit sample, flushes fell from
2.00390625 to 1.00390625 per commit. Larger dirty sets spill manifest records to
additional pages, so this is not a universal zero-byte-overhead claim. Timings
are workstation observations, not a macOS measurement or a hardware-independent
performance guarantee. See the [production design](../../spec/design/validated-cow.md).
