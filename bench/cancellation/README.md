# Cancellation investigation — Go and Rust

Measured 2026-10-18 on `codex/cursor-cancellation`, based on
`c105eed383cae6ef98cfc8872b71aa3657f1c999`. No new dependencies. The TypeScript migration
and its worktree are untouched.

Before integration, the three cancellation commits were rebased onto master `d4a01ebd`, excluding
inherited encryption and TS design commits. The Go/Rust implementation trees are byte-identical
across that rebase. Revision references below use the rebased commits: raw logs' `46317446` maps
to `666575e8`, and `e3bb2db8` maps to `d4c8ef54`. The comparison tasks use those reachable revisions.

## Bug and implementation

Go used a per-call watcher goroutine to copy context cancellation into an atomic. `ergoQuery`
stopped it as soon as `Query` returned, although the returned streaming/blocking/deferred cursor
could execute later in `Next`. Its copied poll then read an atomic that nobody would update.

`ergoQuery` now captures the context's `Done` channel once. `Rows`, every meter, and each
frozen engine retain that channel; restoring the live session cannot disarm a cursor or contaminate
another operation. There are no watcher goroutines to leak on query errors or early close.
`Next` closes resources/pins on exhaustion and terminal errors, after delivering the transaction
failure hook. Explicit `Close` remains necessary for early exits and is terminal/idempotent.
Cancellation after exhaustion or close cannot create a new error.

The first fix (`666575e8`) distinguished **cost ceilings** from **cancellation** when admitting aggregate and
columnar projection paths. Those paths check cancellation at entry and between blocks of at most
256 scan/fold records. That version still polled at scalar cost guards; the follow-up below separates them. Finite statement/lifetime
cost ceilings retain the scalar path and its exact failure points. Successful cost totals are
unchanged. A canceled columnar walk reports its visited-page/record and fold work into the lifetime
account instead of losing it before the normal bulk accrual. The commit cancellation boundary is
unchanged; no storage format changes.

## Follow-up: store Go channels directly

The second follow-up compares two changes independently and together, against `d4c8ef54`
(the sampled-checkpoint implementation described below). **Keep both**:

- **Rows only:** cache `Done` in `Rows`, so each `Next` checks the channel directly instead of
  calling `Context.Done()` through an interface. Engine polling/restoration closures remain.
- **Engine only:** store the channel directly in the session and meters. A deferred method restores
  the previous channel without allocating a closure. `Rows` still retains the context and calls
  `Done()` on every `Next`.
- **Combined:** capture `Done()` once at admission, share that channel with `Rows` and snapshot
  engines/meters, and restore session state without closures. No context interface is retained.

`mise run bench:cancellation:channels-go` builds all four binaries before timing. It archives the
baseline, makes the two isolated variants in temporary copies, and uses the same benchmark fixture
for all four. It then varies process order across four rounds, with the same CPU 0,
`GOMAXPROCS=1`, 500 ms/sample, fully consumed prepared workloads, and reused contexts as before.
Caller context creation, deadline setup, and statement preparation are outside timing. No active
worktree switching or dependencies. Raw samples: `go-channels-{baseline,rows,engine,combined}.txt`.
Environment is unchanged from the environment section below. Values are medians of four samples.

| Cancellation enabled | Baseline µs | Rows only µs | Engine only µs | Combined µs |
|---|---:|---:|---:|---:|
| Cheap prepared | 1.104 | 1.108 | 1.083 | 1.071 |
| Streaming, 10k rows | 594.948 | 596.431 | 594.673 | 587.610 |
| Aggregate, 10k rows | 133.905 | 134.030 | 134.585 | 134.136 |
| Projection, 10k rows | 505.230 | 499.223 | 496.876 | 478.484 |

The combined enabled medians improve by **2.9% cheap, 1.2% streaming, and 5.3% projection**;
aggregate is 0.2% slower, within noise. The engine change alone removes **40 B and two allocations
per cancelable query** in every workload; Rows alone does not change allocation counts. Their
combination retains that allocation reduction and has the lowest enabled median in the cheap,
streaming, and projection workloads. The larger projection gain is consistent with avoiding
per-row interface calls/channel lookup and reducing allocation/GC work. These experiments do not
attribute every nanosecond to those components: compiler layout and GC also change between binaries.

| Combined workload | Disabled µs | Enabled µs | Enabled overhead | Enabled queries/s | B/op, both modes | Allocs/op, both modes |
|---|---:|---:|---:|---:|---:|---:|
| Cheap prepared | 1.051 | 1.071 | 1.9% | 933,707 | 1,944 | 28 |
| Streaming, 10k rows | 570.196 | 587.610 | 3.1% | 1,702 | 961,713 | 20,018 |
| Aggregate, 10k rows | 134.300 | 134.136 | within noise | 7,455 | 2,832 | 19 |
| Projection, 10k rows | 473.870 | 478.484 | 1.0% | 2,090 | 1,792,547 | 10,035 |

Timing noise is material on this shared workstation: Rows-only streaming has a 627.107 µs sample;
combined projection spans 473.288–509.730 µs versus baseline 488.505–516.332 µs. An initial run
suggested larger gains, but its engine-only control also reused the new entry-check helper; the
recorded rerun preserves the old Rows entry check byte-for-byte and uses its more conservative
results throughout. Small scan/aggregate differences are not a statistical guarantee. In particular,
the absolute streaming median improves slightly while its measured enabled/disabled overhead does
not (baseline 2.7%, combined 3.1%). The two isolated projection changes each help by only 1–2%;
the combination helps more, but those gains should not be assumed additive across machines.
The allocation reduction is exact; no new goroutines, counters, or polling intervals were added.

Final response latency was remeasured separately, unpinned, with the existing million-row,
20-sample probe, including scheduling and cleanup (`latency-channels-go.txt`):

| Go workload | Median µs | p95 µs | Maximum µs |
|---|---:|---:|---:|
| Aggregate | 2.915 | 18.330 | 125.156 |
| Projection | 15.589 | 188.338 | 206.143 |
| Streaming | 0.586 | 1.279 | 3.039 |

Reproduce that probe with
`mise exec -- env JED_CANCEL_LATENCY=1 go test -C impl/go -run '^TestCancellationLatencyMeasurement$' -v`.
These scheduler-sensitive samples are not evidence of a changed cancellation bound. Checkpoint
positions, immediate `Next` checks, deterministic cost guards, and the commit boundary are unchanged.
Cancellation is still best effort inside parsing, sorting, and individual scalar/host kernels.

The regression suite now closes real channels instead of injecting polling callbacks. A storage
read wrapper cancels synchronously inside an optimized scan and verifies partial lifetime cost;
no-output join tests still require the rejected-candidate loop's sampled check. New tests cover
one `Done()` capture per operation and restoration of prior session cancellation on success and
query errors, including nil/background contexts. Existing synchronized in-flight tests, transaction
rollback/failure, cursor isolation, early close, exhaustion, and drain-error coverage remain.

The historical SQL `watcher` benchmark fixture is available at `d4c8ef54`; its function-valued
atomic poll cannot be attached to the new channel-valued meter. Its recorded comparisons remain
below. The independent `BenchmarkCancellationMechanism` watcher/channel microbenchmark remains
runnable in this checkout. Rust is unchanged in this follow-up.

Verification after this change: `mise run unit:go`, `mise run test:cancellation:race`, and
`mise run conformance:go` passed. Conformance: memory **677 passed / 2 skipped**; disk
**596 passed / 83 skipped**, zero failures. Go formatting and `git diff --check` passed.

## Earlier follow-up: separate cancellation from cost enforcement

The first fix still polled cancellation at every scalar cost guard. The follow-up keeps those
cost guards at exactly the same points, but ordinary expression evaluation no longer polls.
Go polls on the first and every 256th execution step: input rows (including rejected rows),
join candidates, recursive iterations, and operator loops. Its existing `Next` entry check stays
immediate. Vectorized scans/folds check each block of up to 256 records. Host-call boundaries and
selected expensive built-ins retain checks. A cancellation-only poll never adds a cost guard.

Rust likewise separates expression cost guards from cancellation, but retains an atomic poll at
each row/operator step. Sampling Rust at 256 steps did not improve throughput and raised streaming
cancellation response from sub-microsecond to a 9.176 µs median (15.958 µs p95). It was therefore
rejected for Rust. Vectorized batches retain their existing 256-record checks in both cores.
`rust-coarse-sampled.txt` and `latency-coarse-sampled.txt` retain that experiment.

The initial Go sampling wrapper added an extra function call on the unlimited-cost path. The final
version keeps that path inline and leaves finite-limit comparisons in `guardLimits`; every cost
limit still aborts at the same guard. `go-coarse-uninlined.txt` retains the intermediate experiment.
At `d4c8ef54`, no allocations were added by sampling: enabling Go cancellation added 40 bytes / two
allocations per query, and caller context/deadline creation remains outside engine timings.

### Controlled Go comparison

`mise run bench:cancellation:compare-go` archives the prior commit (`666575e8`) into a temporary
directory, builds both test binaries before timing, and alternates identical workload selections
in fresh processes (control/current, current/control, control/current). All runs use CPU 0,
`GOMAXPROCS=1`, 500 ms per sample, and fully drained results. The task neither switches the active
worktree nor installs dependencies. Raw samples: `go-control-paired.txt`, `go-coarse-paired.txt`.
Medians of three runs:

| Go workload | Prior disabled / enabled µs | Current disabled / enabled µs | Prior overhead | Current overhead |
|---|---:|---:|---:|---:|
| Cheap prepared | 1.057 / 1.128 | 1.045 / 1.105 | 6.7% | 5.7% |
| Streaming, 10k rows | 582.243 / 654.682 | 577.179 / 594.559 | 12.4% | 3.0% |
| Aggregate, 10k rows | 133.493 / 133.539 | 133.767 / 134.240 | within noise | within noise |
| Projection, 10k rows | 465.181 / 529.323 | 475.166 / 501.664 | 13.8% | 5.6% |

With cancellation enabled, streaming is **9.2% faster** and projection **5.2% faster** than the
prior commit in this comparison. Disabled streaming is 0.9% faster; disabled projection is 2.1%
slower, so this is not a uniform speedup in every mode. Cheap-query setup still dominates its
remaining overhead. Current enabled throughput is approximately 905k cheap queries/s, 1,682
streaming queries/s, 7,450 aggregates/s, and 1,993 projections/s.

The full benchmark selection at `d4c8ef54` also included watcher and forced-scalar variants (`go-coarse.txt`).
It shows smaller enabled overhead (1.0% streaming, 2.2% projection), but different absolute times;
benchmark selection and preceding allocation/GC activity matter. Use the identical-selection,
alternating comparison above for before/after claims, rather than comparing unrelated runs.
The full run still favors channel polling over watcher setup for cheap queries (1.110 vs 1.821 µs).
The aggregate/projection fast paths remain available: enabled forced-scalar aggregate takes
1,713.702 µs versus 135.355 µs optimized (12.7×); projection takes 1,691.765 versus 566.161 µs (3.0×).

### Final Rust overhead and response latency

The final Rust choice (per-step atomic checks, no expression-node polls) measures:

| Rust workload | Disabled µs | Enabled µs | Enabled queries/s |
|---|---:|---:|---:|
| Cheap prepared | 1.524 | 1.537 | 650,787 |
| Streaming, 10k rows | 714.878 | 717.627 | 1,393 |
| Aggregate, 10k rows | 207.076 | 207.010 | 4,831 |
| Projection, 10k rows | 319.936 | 320.602 | 3,119 |

Every measured disabled/enabled difference is below 1%, too small to claim a throughput change on
this workstation. The final response measurement uses the same million-row / twenty-sample probe
as the first fix, unpinned, including scheduling and cleanup:

| Core / workload | Median µs | p95 µs | Maximum µs |
|---|---:|---:|---:|
| Go aggregate | 3.737 | 6.489 | 6.720 |
| Go projection | 5.681 | 187.404 | 197.568 |
| Go streaming | 0.783 | 1.808 | 2.101 |
| Rust aggregate | 3.231 | 5.717 | 7.246 |
| Rust projection | 126.173 | 185.348 | 229.296 |
| Rust streaming | 0.254 | 0.306 | 0.399 |

Raw results: `rust-coarse.txt`, `latency-coarse.txt`. This preserves fast cancellation without
paying for it at every expression node. Best effort still applies: individual scalar kernels,
parsing, sorting, host callbacks, and commit work can delay observation. No new public knob or
dependency was introduced, and the commit boundary is unchanged.

## Reproduce

```sh
# Run the normal checkout setup if its development environment is not initialized.
# These particular measurements use an in-memory database and do not query PostgreSQL.
GOMAXPROCS=1 taskset -c 0 mise run bench:cancellation:go
taskset -c 0 mise run bench:cancellation:rust
mise run bench:cancellation:latency
mise run bench:cancellation:compare-go # Linux; alternating control/current comparison
mise run bench:cancellation:channels-go # Linux; four-way Go channel comparison

mise run test:cancellation:race
mise run unit:go
mise run unit:rust
mise run conformance:go
mise run conformance:rust
```

`taskset` is the Linux affinity wrapper, not a dependency of the benchmarks; omit it on other
platforms. Run measurements sequentially, after compilation/tests finish. For the historical
implementation, create a separate worktree at the pre-fix master base and apply only the benchmark fixtures
and task registrations from this branch:

```sh
git diff d4a01ebdbf3edff0c130b1fdb4c1115df6d1c4bd codex/cursor-cancellation -- \
  impl/go/cancellation_bench_test.go impl/rust/src/cancellation_bench.rs \
  impl/rust/src/shared.rs mise.toml > /tmp/jed-cancellation-bench.patch
git worktree add --detach /tmp/jed-cancellation-before d4a01ebdbf3edff0c130b1fdb4c1115df6d1c4bd
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

## First-fix engine overhead (`666575e8`)

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

In the first-fix measurements, enabling Go cancellation added 40 bytes and two allocations per query:

| Workload | Disabled B/op / allocations | Enabled B/op / allocations |
|---|---:|---:|
| Cheap | 1,944 / 28 | 1,984 / 30 |
| Streaming | 961,712 / 20,018 | 961,752 / 20,020 |
| Aggregate | 2,832 / 19 | 2,872 / 21 |
| Projection | 1,792,544 / 10,035 | 1,792,584 / 10,037 |

## First-fix execution-path penalty

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

## First-fix watcher tradeoff and caller costs

The historical `watcher` engine variant (`666575e8`) models the old atomic/goroutine mechanism **with corrected lifetime**:
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
without a scheduled intermediary, and simpler lifetime/isolation behavior. That initial version still polled at scalar cost checkpoints. The follow-ups above replace this
with sampled execution steps.

The isolated lifecycle/poll benchmark shows why this is a tradeoff: one poll plus watcher
setup/teardown costs 626 ns and roughly 164 bytes / three allocations, versus 4.28 ns and no
allocations for a cached channel check. At 1,000 polls, the watcher/atomic takes 1.094 µs versus
2.770 µs for direct channel polling. These exclude the engine and context creation.

Separately measured caller setup: Go `WithCancel` + materializing `Done` + cancel costs 147 ns,
208 bytes / three allocations; `WithTimeout` + `Done` + cancel costs 383 ns, 384 bytes / five
allocations. Rust token creation/drop costs 13.6 ns; its API has no built-in deadline scheduler.
These setup costs must be added by callers that allocate a fresh context/token for every query.

## First-fix response latency

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
  synchronized in-Next test and batch-entry/never-triggered cost/limit regressions. New tests\n  separate cancellation checkpoints from deterministic cost enforcement; Go also proves cancellation in scans
  and nested-loop joins that reject every row, including a join with fewer than 256 input rows.
- Both full native host/unit suites pass (Rust: 1,028 library tests, plus three conformance-harness
  tests; opt-in measurements remain ignored).
- Each core's shared conformance run: memory **677 passed**, 2 skipped; file **596 passed**,
  83 skipped; zero failures. Skips are the existing capability/storage-mode exclusions.
- No SQL semantics or corpus answers changed, so no PostgreSQL oracle regeneration was needed.
  TypeScript and full cross-process CI were not rerun for this Go/Rust cancellation-only change.
