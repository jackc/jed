# Pure Ruby experiment: implementation in progress

This is an **incomplete experiment**, not a completed port or a fourth supported
core. Work started against `d4a01ebdbf3edff0c130b1fdb4c1115df6d1c4bd`.
The implementation is independent Ruby; its SQL execution does not call Rust,
Go, TypeScript, WASM, FFI, or another database engine. No gem dependency was added.

Normal tests, benchmarks, code generation, and CI do not run or maintain this
directory. The explicit `ruby-native:*` mise tasks are the entry points. Future
feature work must leave the persisted experiment unchanged unless requested.

## Validation status

The runner reads the existing shared SQL corpus. Unsupported harness directives
are reported separately and cause a failed audit; they are not passing tests.
Value, type, name, error-code, and cost mismatches also fail. Full audits remain
red. The latest comprehensive audit will be recorded after implementation stops
changing; intermediate runs are diagnostic, not a release gate.

Executed independent checks include:

- Native API: prepared statements, parameter inference and coercion, atomic
  scripts, rollback, and isolation of mutable host inputs and returned values.
- Pure Ruby regex VM: shared program and match/cost fixtures, bounded programs,
  and the SQL regex corpus.
- Storage: 106 shared file images (97 accepted, nine deliberately rejected),
  65 byte-exact encodings, recovery, exclusive lock protocol, error poisoning,
  serialization failures, and metadata reopening.
- A real Ruby → Go → Ruby → Go file handoff covering decimal typmods, an index,
  overflow text, a view, updates, and inserts.

None of these scoped results establishes whole-core conformance. In particular,
the SQL planner, deterministic costs, resource accounts, environment bundles,
page service, and the full host API still require implementation and verification.

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
protocol-v2 ownership with Ruby's built-in `flock`. It does not yet serve or join
the multi-process page service. Commits append a complete snapshot; page reuse,
incremental domain persistence, and bounded page residency are unfinished.
Large-file performance must not be inferred from the small in-memory comparison.

## Completion criteria

This report must be updated with a final full corpus audit, disk validation,
standard benchmark coverage, optimized interpreter/YJIT measurements, the
supported-core comparison, and an explicit remaining-gap list. The requested
complete-port task remains open until its completion criteria are met or a
concrete inability to implement a feature is reported.
