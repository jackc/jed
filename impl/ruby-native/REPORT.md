# Ruby experiment: validation and optimization in progress

This is an **experimental core**, not a fourth supported core. The requested
implementation and performance work remains in progress. Work started against
`d4a01ebdbf3edff0c130b1fdb4c1115df6d1c4bd`.
The implementation is independent Ruby; its SQL execution does not call Rust,
Go, TypeScript, WASM, FFI, or another database engine. SQL digest functions use
the explicitly approved Ruby OpenSSL dependency; database algorithms remain Ruby.

Normal tests, benchmarks, code generation, and CI do not run or maintain this
directory. The explicit `ruby-native:*` mise tasks are the entry points. Future
feature work must leave the persisted experiment unchanged unless requested.

## Validation status

The runner reads the existing shared SQL corpus. Unsupported harness directives
are reported separately and cause a failed audit; they are not passing tests.
Value, type, name, error-code, and cost mismatches also fail. The first complete
SQL audits on October 18 passed against immutable source snapshots:

| Mode | Files considered | Checks passed | Declared file skips | Peak worker RSS |
|---|---:|---:|---:|---:|
| Memory | 679 | 20,267 / 20,267 | 2 | 145.4 MiB |
| Disk | 679 | 18,250 / 18,250 | 77 | 152.1 MiB |

Both had zero unsupported harness files, timeouts, or memory-limit failures.
The [memory report](results/working-memory-conformance.json) and
[disk report](results/working-disk-conformance.json) retain source hashes,
timestamps, limits, and exact counts. The memory snapshot predates the subsequent
bytea escape allocation reduction; its separate rendering suite passed 16/16.
Final integration and optimized-source audits remain pending.

An earlier unbounded run was stopped by the maintainer after reaching 49 GB.
An orphaned benchmark worker was also found and terminated. The exact allocation
path responsible for that peak was not established. Validation now isolates each
file/workload in a sequential child with a 512 MiB sampled process-group RSS
threshold, a hard 1 GiB address-space limit for Ruby, and an external deadline.
The supervisor kills descendants on timeout/interruption; a separate watchdog
also kills them if the supervisor dies. Reports and diagnostic payloads are
bounded. These are harness limits, separate from SQL's logical resource budgets.

Core fixes remove row-token arrays from file scans, share decoded-page cache
budgets across generations, use compact UTF-8 regex views, and avoid retaining
all global replacement captures. The benchmark adapter also removes closed
handles from its cleanup registry immediately: retaining every closed handle
kept every page cache alive throughout a 200-iteration cold-open workload. The
repeated cold-open regression now completes at 154.0 MiB peak RSS; its
[diagnostic report](results/working-cold-open-memory.json) records the workload.
Closed query cursors likewise release their engine and snapshot while retaining
their collected rows, metadata, and final cost.
Those fixes and harness containment do not
turn logical SQL budgets into a whole-engine heap guarantee.

Executed independent checks include:

- Native API: prepared statements, parameter inference and coercion, atomic
  scripts, rollback, and isolation of mutable host inputs and returned values.
- Independent sessions, writer blocking, pinned readers, and lazy cursors:
  early close, frozen snapshots, and matching full-drain values, types and costs.
  All 180 shared concurrency assertions pass in memory.
- Pure Ruby regex VM: shared program and match/cost fixtures, bounded programs,
  and the SQL regex corpus.
- Storage: 106 shared file images (97 accepted, nine deliberately rejected),
  65 byte-exact encodings, recovery, exclusive lock protocol, error poisoning,
  serialization failures, and metadata reopening.
- A real Ruby → Go → Ruby → Go file handoff covering decimal typmods, an index,
  overflow text, a view, updates, and inserts.
- Immutable timezone and Unicode bundle loading: 16 shared timezone vectors,
  46 shared collation sort-key vectors, casing, and byte round-trips.
- Logical query-memory scheduling, ownership and pending writes: 242/242 memory
  records and 64/64 disk records, including the 13 spill memory-limit records.
- Real bounded disk spools, stable external sorting and partitioned aggregate
  state: 865/865 aggregate records under forced spill with 96-byte `work_mem`.
- Actual bounded scans and physical join selection: 30/30 bounded LIMIT,
  32/32 two-input P7 and 94/94 multi-input P8 join-planning records.
- WHERE contradiction proofs: 58/58 SELECT/UPDATE/DELETE records; ON CONFLICT:
  349/349 records; the complete SQL audits above include broader plan coverage.

SQL corpus results do not alone establish host/file interoperability. Sparse
pending writes, native page-service owners and clients, host function registration,
and physical backup/compaction also have explicit host tests. All 108 Ruby/Go
process pairings pass, including byte-identical backups, owner handoff, crash
recovery, attachments, and serialization/storage-limit failures. The latest host
suite also passes unchanged-chain corruption and compression-cost checks, at
109 MiB peak RSS. Final performance measurements remain pending.

## Recorded prototype baseline

The initial measurements are in
[`baseline.json`](../../bench/ruby-native/results/baseline.json). The exact
measured source is preserved in
[`baseline-source.tar.gz`](../../bench/ruby-native/results/baseline-source.tar.gz) and
checked against its SHA-256 before reuse. This baseline was recorded while the
port was incomplete. Subsequent work includes correctness and coverage changes
as well as optimizations; a before/after ratio is therefore not an isolated
measurement of optimization alone.

Host: Intel Core Ultra 9 285K, x86-64 Linux, Ruby 4.0.7, Go 1.27.1,
Rust 1.99.0, Node 26.10.0. Each lane uses the same 1,000-row in-memory data,
prepared queries, varying parameter schedule, fixed warmup, five timing batches,
and 32 separate checksum validation cases. Raw samples, commands, runtime status,
and the plan hash are retained in the JSON. These small workloads do not replace
the standard benchmark corpus or represent large-file performance.

Baseline median microseconds per operation (lower is better):

| Workload | Ruby | Ruby YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|
| Scalar | 36.04 | 19.50 | 0.86 | 1.39 | 2.25 |
| Primary key hit | 3,111.71 | 2,214.92 | 1.35 | 2.25 | 4.66 |
| Primary key miss | 3,103.05 | 2,209.13 | 1.02 | 1.81 | 2.92 |
| Filtered scan | 3,145.30 | 2,270.27 | 161.60 | 132.73 | 409.12 |
| Aggregate | 3,556.34 | 2,549.48 | 65.00 | 53.36 | 249.35 |
| Group | 4,852.18 | 3,937.09 | 271.56 | 177.86 | 543.38 |
| Sort and limit | 3,400.39 | 2,778.16 | 211.73 | 134.50 | 433.69 |

YJIT is measured as its own lane. **ZJIT is unavailable in the installed Ruby
build**; the launcher verifies actual activation and refuses to label an
interpreter fallback as a JIT measurement.

The standard datasets were separately regenerated with the existing Go generator
and current file format. The opt-in adapter attempts every standard workload,
records failures as failures, and compares raw rows/checksums with Go. Final
coverage results and optimized timings are still pending.

## Storage limitations

The engine reads and writes the current v49 shared file format, including
validated COW content manifests. The current pure Ruby file host takes exclusive
protocol-v2 ownership with Ruby's built-in `flock`. Default `locking: :auto`
serves or joins the multi-process page service; `locking: :exclusive` requires
sole file access. SQL commits persist changed B-tree paths, retain
clean page identities, protect current manifest dependencies during free-page
reuse, and reclaim trailing free pages. Memory and file storage limits use the
shared page accounting; 48 memory, 35 file and 10 temporary-limit records pass.
File reads use lazy page-backed rows and a bounded cache. Ordinary mutations
retain their changed rows over immutable tree pages; constraint checks use key
probes or bounded spools. Changed
attachment domains persist independently and temporary-only work leaves main
unchanged.
Unchanged large-value spans and overflow chains survive leaf rewrites, including
unassigned values in UPDATE and primary-key changes. Their live-page dependencies
remain protected without rewriting them into the current content manifest.
Large-file performance must not be inferred from the small in-memory comparison.

## Completion criteria

This report must be updated with a final full corpus audit, disk validation,
standard benchmark coverage, optimized interpreter/YJIT measurements, the
supported-core comparison, and an explicit remaining-gap list. The requested
complete-port task remains open until its completion criteria are met or a
concrete inability to implement a feature is reported.
