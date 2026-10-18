# Cancellation investigation — Go and Rust

Measured 2026-10-18 on `codex/cursor-cancellation`, based on
`c105eed383cae6ef98cfc8872b71aa3657f1c999`. No new dependencies. The TypeScript migration
and its worktree are untouched.

## Bug and implementation

Go used a per-call watcher goroutine to copy context cancellation into an atomic. `ergoQuery`
stopped it as soon as `Query` returned, although the returned streaming/blocking/deferred cursor
could execute later in `Next`. Its copied poll then read an atomic that nobody would update.

`armCancel` now captures the context's `Done` channel in an operation-owned poll. Every meter and
frozen engine retains that poll; restoring the live session cannot disarm a cursor or contaminate
another operation. There are no watcher goroutines to leak on query errors or early close.
`Next` closes resources/pins on exhaustion and terminal errors, after delivering the transaction
failure hook. Explicit `Close` remains necessary for early exits and is terminal/idempotent.
Cancellation after exhaustion or close cannot create a new error.

Both cores now distinguish **cost ceilings** from **cancellation** when admitting aggregate and
columnar projection paths. Those paths check cancellation at entry and between blocks of at most
256 scan/fold records. Scalar execution retains its existing guards. Finite statement/lifetime
cost ceilings retain the scalar path and its exact failure points. Successful cost totals are
unchanged. A canceled columnar walk reports its visited-page/record and fold work into the lifetime
account instead of losing it before the normal bulk accrual. The commit cancellation boundary is
unchanged; no storage format changes.

## Reproduce

```sh
# Run the normal checkout setup if its development environment is not initialized.
# These particular measurements use an in-memory database and do not query PostgreSQL.
GOMAXPROCS=1 taskset -c 0 mise run bench:cancellation:go
taskset -c 0 mise run bench:cancellation:rust
mise run bench:cancellation:latency

mise run test:cancellation:race
mise run unit:go
mise run unit:rust
mise run conformance:go
mise run conformance:rust
```

`taskset` is the Linux affinity wrapper, not a dependency of the benchmarks; omit it on other
platforms. Run measurements sequentially, after compilation/tests finish. For the historical
implementation, create a separate worktree at the base above and apply only the benchmark fixtures
and task registrations from this branch:

```sh
git diff c105eed383cae6ef98cfc8872b71aa3657f1c999 codex/cursor-cancellation -- \
  impl/go/cancellation_bench_test.go impl/rust/src/cancellation_bench.rs \
  impl/rust/src/shared.rs mise.toml > /tmp/jed-cancellation-bench.patch
git worktree add --detach /tmp/jed-cancellation-before c105eed383cae6ef98cfc8872b71aa3657f1c999
git -C /tmp/jed-cancellation-before apply /tmp/jed-cancellation-bench.patch
# Run the overhead tasks from that worktree. The old Go code fails cursor-lifetime cancellation;
# its never-triggered-token measurements do not establish correctness.
```

Environment: Intel Core Ultra 9 285K, 24 logical CPUs, approximately 60 GiB RAM; Linux x86-64
`7.0.0-38-generic`; Go `go1.27.1 linux/amd64`; Rust `1.99.0 (b940084d7 2026-09-28)`, Cargo release
optimization. Final throughput runs use CPU 0, with Go `GOMAXPROCS=1`. This is a shared workstation
with dynamic frequency and other work, not an exclusive benchmark machine. Raw repeated samples
are retained; sub-percent differences are noise, not evidence of a speedup.

Each workload uses 10,000 committed rows in a memory database (the ordinary packed-page/pager
path). Setup, preparation, context/token creation, and deadline installation are outside engine
timing. Each execution fully drains the cursor and checks its terminal error. Go reports allocation
counts; Rust uses no allocation profiler. Rust's prepared measurement invokes the existing private
`Session::armed` around `query_prepared` from a test module, because there is no public prepared
cancelable method. It adds no production host API.

The SQL shapes are a prepared primary-key lookup, an ordered streaming scan with LIMIT 10000,
`sum(v), count(*)`, and a bare-column projection. Each result below is the median of three runs of
at least 500 ms. `scalar-*` sets `max_cost` to the largest signed integer to force the old scalar
execution path without ever tripping the limit. This includes the small cost of ceiling comparisons;
it is a path-penalty control, not a pure polling microbenchmark.

## Final engine overhead

Cancellation is enabled but never triggered. Latency is per fully consumed query, not per row.

| Core / workload | Disabled µs | Enabled µs | Enabled overhead | Disabled → enabled queries/s |
|---|---:|---:|---:|---:|
| Go cheap prepared | 1.051 | 1.136 | 8.1% | 951,475 → 880,282 |
| Go streaming | 622.258 | 691.028 | 11.1% | 1,607 → 1,447 |
| Go aggregate | 133.751 | 133.242 | within noise | 7,477 → 7,505 |
| Go projection | 530.374 | 585.038 | 10.3% | 1,885 → 1,709 |
| Rust cheap prepared | 1.574 | 1.605 | 2.0% | 635,485 → 623,092 |
| Rust streaming | 712.574 | 709.482 | within noise | 1,403 → 1,409 |
| Rust aggregate | 208.869 | 208.998 | within noise | 4,788 → 4,785 |
| Rust projection | 319.639 | 316.282 | within noise | 3,129 → 3,162 |

Go enabling cancellation adds 40 bytes and two allocations per query in these measurements:

| Workload | Disabled B/op / allocations | Enabled B/op / allocations |
|---|---:|---:|
| Cheap | 1,944 / 28 | 1,984 / 30 |
| Streaming | 961,712 / 20,018 | 961,752 / 20,020 |
| Aggregate | 2,832 / 19 | 2,872 / 21 |
| Projection | 1,792,544 / 10,035 | 1,792,584 / 10,037 |

## Execution-path penalty

| Core / workload, cancellation enabled | Forced scalar µs | Optimized µs | Speedup |
|---|---:|---:|---:|
| Go aggregate | 1,644.129 | 133.242 | 12.3× |
| Go projection | 1,986.858 | 585.038 | 3.4× |
| Rust aggregate | 1,461.331 | 208.998 | 7.0× |
| Rust projection | 1,388.206 | 316.282 | 4.4× |

Go's enabled aggregate falls from approximately 5.28 MB / 10,094 allocations to 2,872 bytes /
21 allocations; projection falls from 5.60 MB / 20,087 allocations to 1.79 MB / 10,037 allocations.
The forced-scalar disabled/enabled pairs in the raw results isolate the smaller polling component.

Exploratory measurements on the original gate independently showed the same issue: Rust aggregate
210.6 → 1,498.9 µs and projection 323.4 → 1,437.1 µs when arming cancellation; Go aggregate
228.2 → 1,871.0 µs and projection 671.2 → 1,915.2 µs. These earlier runs were **unpinned**, Go used
24 runtime processors, and compilation/other work could overlap; do not compare their absolute
latencies with the final pinned results. `go-channel-scalar.txt` records the intermediate experiment:
removing the watcher alone did not recover the aggregate/projection performance. It also predates
the no-context restore-closure allocation fix.

## Watcher tradeoff and caller costs

The `watcher` engine variant models the old atomic/goroutine mechanism **with corrected lifetime**:
it restores the live session after opening the cursor and stops the watcher after draining. It uses
the same optimized eligibility as the final channel implementation.

| Go workload | Channel µs | Full-lifetime watcher µs |
|---|---:|---:|
| Cheap prepared | 1.136 | 1.757 |
| Streaming | 691.028 | 653.225 |
| Aggregate | 133.242 | 133.930 |
| Projection | 585.038 | 631.667 |

The watcher adds five allocations per engine operation versus the channel's two. It wins this
streaming case by about 5.5%, but costs about 55% more on the cheap lookup. The direct-channel
implementation was selected for cheap-call performance, fewer allocations, immediate observation
without a scheduled intermediary, and simpler lifetime/isolation behavior. Scalar polling is kept
at the existing checkpoints rather than adding another mutable sampling counter.

The isolated lifecycle/poll benchmark shows why this is a tradeoff: one poll plus watcher
setup/teardown costs 626 ns and roughly 164 bytes / three allocations, versus 4.28 ns and no
allocations for a cached channel check. At 1,000 polls, the watcher/atomic takes 1.094 µs versus
2.770 µs for direct channel polling. These exclude the engine and context creation.

Separately measured caller setup: Go `WithCancel` + materializing `Done` + cancel costs 147 ns,
208 bytes / three allocations; `WithTimeout` + `Done` + cancel costs 383 ns, 384 bytes / five
allocations. Rust token creation/drop costs 13.6 ns; its API has no built-in deadline scheduler.
These setup costs must be added by callers that allocate a fresh context/token for every query.

## Response latency

The latency task uses 1,000,000 rows. After opening a cursor, another goroutine/thread sleeps 1 ms
and records the instant immediately before requesting cancellation. The execution side drains until
`57014` and records its return time. Twenty samples include signal delivery, scheduling, and engine
cleanup, but exclude context/token creation and timer/thread setup. This is a timed performance
probe; deterministic correctness uses executor barriers instead.

| Workload | Median µs | p95 µs | Maximum µs |
|---|---:|---:|---:|
| Go Aggregate | 2.959 | 5.392 | 10.612 |
| Go Projection | 22.751 | 429.694 | 533.729 |
| Go Streaming | 1.294 | 5.116 | 1844.776 |
| Rust Aggregate | 2.974 | 9.703 | 10.239 |
| Rust Projection | 158.322 | 192.588 | 232.379 |
| Rust Streaming | 0.362 | 0.480 | 0.507 |

The Go streaming run includes a 1.845 ms scheduler/cleanup outlier despite its 1.294 µs median.
These are observations, not guarantees. Rust's projection must drop the gathered values before
returning, which contributes to response time. Parsing, sorting, individual scalar kernels and
host callbacks are not made resumable/preemptible, and commit work remains outside new checks.
Cancellation timing and the partial cost at that timing are not cross-core deterministic; completed
cost totals and deterministic cost-limit behavior remain the contract.

## Verification

- Go synchronized in-flight cancellation tests cover all database/session/transaction handles,
  prepared/unprepared calls, streaming/blocking/deferred cursors, Exec drains, writes and rollback.
  They assert `57014`, failed-transaction `25P02`, cleanup, isolation, exhaustion and early close.
- Go's race task passes, including related streaming/drain/host-function API tests. Rust has a
  synchronized in-Next test and batch-entry/never-triggered cost/limit regressions.
- Both full native host/unit suites pass (Rust: 1,027 library tests, plus three conformance-harness
  tests; opt-in measurements remain ignored).
- Each core's shared conformance run: memory **677 passed**, 2 skipped; file **596 passed**,
  83 skipped; zero failures. Skips are the existing capability/storage-mode exclusions.
- No SQL semantics or corpus answers changed, so no PostgreSQL oracle regeneration was needed.
  TypeScript and full cross-process CI were not rerun for this Go/Rust cancellation-only change.
