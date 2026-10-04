# Durability experiments

Research prototypes, not a shipped storage mode. See the
[design and results](../../spec/design/durable-commit-experiments.md).

`experiment.rs` runs as an in-crate Rust test so the real SQL engine, pager, and
format remain private. It captures the production COW allocator's exact writes
for SQL UPDATE transactions, then times identical write sets through:

- Current body-sync / meta-sync COW.
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
