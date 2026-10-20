# Go scan row allocation

Production baseline: `c105eed383cae6ef98cfc8872b71aa3657f1c999`, clean detached
checkout before creating `codex/go-row-materialization`. Reference investigation:
`571e3e9fd2e3de2d4eb3daa2b3dd1715781de7b1`,
`bench/go-specialization/results/2026-10-20/report.md` at that commit. No
specialization implementation or build tags are used here.

The adopted change reuses one input row per ordinary streaming range cursor.
It preserves the existing full-column reconstruction order, including lazy
wrappers for untouched variable-length columns and eager fixed-width decoding.
It does not replace that sequence with masked decoding: that could change error
visitation. The packed decoder fills a private slice; decoded leaves are copied
into it. Resolution may mutate this private slice, but never a tree-owned row.

`streamingCursor` owns the scratch, not a plan, prepared statement, tree, or pool.
It lasts through filter and projection evaluation, including synchronous nested
queries. Expressions copy Values out of the row; ROW/ARRAY constructors allocate
their own element slices. Overwriting input slots does not mutate their payloads.
Every projected output retains its own slice. `Rows.Row()` exposes that slice;
`Values()` creates a separate `[]any`; `Scan()` converts into caller destinations.
Advancing and closing must preserve retained outputs from all three surfaces.
Host-call read-ahead keeps independently owned input rows and is excluded, as are
point scans (which have only one input), columnar and blocking lanes.

No plan, cost charge/guard, decode/resolve boundary, or query-memory reservation
changes. The single transient input row is not an input-growing row buffer under
memory.md §5.4. LIMIT and OFFSET still run at their existing points. Reuse saves
the row backing allocation, not individual variable-length payload allocations.

The [adoption report](results/2026-10-19/report.md) and compact raw timing samples
are tracked. Generated summaries, profiles, inventories, test logs, and resource
traces belong in ignored `bench/results/go-row-materialization/2026-10-19/`, the
task's default output directory. The original complete evidence bundle remains
there locally; binary profiles and executables stay under `.dev/`.

Reproduce through mise:

```
mise run experiment:go-row-materialization build
mise run experiment:go-row-materialization profile
mise run experiment:go-row-materialization run
mise run experiment:go-row-materialization trace
mise run experiment:go-row-materialization checks
mise run verify
```

Build archives the pinned production Go sources and installs exactly the same
benchmark source in both builds. Profile sets `MemProfileRate=1` before setup,
warms both bindings, snapshots cumulative allocations, runs 100 complete queries,
then snapshots again; pprof subtracts setup from total. Profiling overhead is not
decision timing. Raw profiles include the first snapshot writer's allocations;
the additional `steady-query` reports filter `runtime/pprof` stacks and recompute
percentages. Binary profiles and executables stay under `.dev/`.

Timing uses five fresh sequential process pairs, alternating order, 150 ms per
workload. Setup, prepare and warmup are outside the timed region; cursor open,
drain, value checksum, cost check, and close are inside. File hosts reopen and
warm the database; SkipFsync applies only to fixture preparation. Ad-hoc and
first-call lanes include parse and plan work. Both bindings' checksums and costs
must agree across builds. Each query runs over 4,096 deterministic rows.

`JED_ROW_BASELINE` and `JED_ROW_WORK` select a different comparison revision and
isolated build directory for integration checks; defaults retain the measured
production baseline. `JED_ROW_RESULTS`, `JED_ROW_BENCH`, `JED_ROW_PAIRS`, and
`JED_ROW_TIME` select a separate result directory and affected lanes for one short
targeted recheck.
The recorded control recheck used:

```
JED_ROW_RESULTS=bench/results/go-row-materialization/2026-10-19/recheck \
JED_ROW_BENCH='BenchmarkRowMaterialization/(memory|file)/(point_hit|point_miss|columnar)$' \
JED_ROW_PAIRS=3 JED_ROW_TIME=250ms mise run experiment:go-row-materialization run
```

Adoption requires meaningful complete-query gains and no reproducible control
regression; do not extend measurements repeatedly to chase significance.

`report` rebuilds summaries from existing timing logs. An optional input directory
reads the committed samples while writing summaries to the ignored output directory:

```
mise run experiment:go-row-materialization report bench/go-row-materialization/results/2026-10-19
JED_ROW_RESULTS=bench/results/go-row-materialization/2026-10-19/recheck \
mise run experiment:go-row-materialization report bench/go-row-materialization/results/2026-10-19/recheck
```

`profile-report` rebuilds
the filtered reports from the local binary profiles. `checks-corpus` resumes
checks after unit/race tests without repeating them. Checks run serially: unit,
full race suite, PostgreSQL oracle, two Go NoREC seeds, complete Go memory/disk
corpus, then baseline/candidate resource suites with finite memory in memory,
disk, and forced-spill modes, comparing every record's peak and storage bytes.
`resources` runs only those resource comparisons. The finite accounting budget
is 1 TiB, allowing scalar-limit tests to reach their intended limit; explicit
per-record query-memory budgets still test admission failures. Start this
checkout's PostgreSQL stack with `mise run dev` before oracle/corpus checks.
`trace` compares both builds over all 0–96 statement/lifetime cost prefixes and
context cancellation before open or a cursor pull, at three memory budgets on
both hosts; it records plans, types, rows,
per-pull cost/account balances, memory peaks and terminal errors. It stores the
identical transcript once as gzip plus its SHA-256 digest.
