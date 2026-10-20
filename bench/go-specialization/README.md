# Go execution-specialization experiment

This is an opt-in G1/G2/G3 implementation of
[`go-execution-specialization.md`](../../spec/design/go-execution-specialization.md).
Production builds keep the ordinary evaluator. No dependency, public API, SQL
setting, planner policy, cost schedule, or file format changes.

## Implemented boundary

Only the existing direct, nonblocking, single-base-table pull cursor can use
kernels. The plan must also pass the existing cacheability gate. No DISTINCT,
aggregate, join, window, DML, correlated/host/volatile expressions, columnar path,
or encoded-span path is replaced. Full scans need PK-satisfied ORDER BY or an
existing LIMIT to enter that cursor; a plain unordered scan remains a control.

Each complete root can be:

- An integer arithmetic or comparison operator, whose operands are primitive
  leaves or one integer arithmetic operation over primitive leaves.
- IS [NOT] NULL over an integer/boolean/NULL leaf, or NOT over a boolean/NULL leaf.

Leaves are columns, finalized typed parameters, integer/boolean constants, or
NULL. Arithmetic uses the existing checked `+ - * / %` kernel and resolved
result width. Every operand has its own type proof; an integer result such as
`date - date` does not qualify. Comparisons accept only integer/NULL operands.
G2 composes these terminals and primitive leaves with value/filter AND, OR,
NOT, NULL tests, single-WHEN searched CASE and two-argument COALESCE. Filter AND
propagates filter context only through AND children; other forms evaluate their
children as values. This preserves the distinct NULL short-circuit rule.

Casts, larger arithmetic patterns, multiple WHEN arms, COALESCE with more than
two arguments and other types fall back as complete roots. G3 additionally
requires every projection and the optional filter to be admitted, and no host
read-ahead. It selects a precompiled row-at-a-time cursor loop at setup. The loop
uses the original source, meter and output ownership, with no survivor buffer.

## Build modes and checks

All entry points run through mise. The executable task is
[`mise-tasks/experiment/go-specialization`](../../mise-tasks/experiment/go-specialization).

| Build tags | Execution |
|---|---|
| none | Ordinary production path; no descriptor field/storage or runtime mode check. |
| `jed_specialization` | Selective: admit the narrow roots on unbounded pull scans. |
| `jed_specialization,jed_specialization_forced` | Forced: also try supported roots on bounded pull scans, from first execution. |
| `jed_specialization,jed_specialization_ordinary` | Build selective descriptors but use the ordinary evaluator. |
| append `jed_specialization_closures` | G2 closure-per-source-node control, over the same admitted surface. |
| append `jed_specialization_scan` | G3: select the precompiled scan loop for fully admitted bundles. |
| append `jed_specialization_diagnostics` | Untimed atomic invocation counters, available to private tests only. |

```sh
mise run experiment:go-specialization test
mise run experiment:go-specialization diagnostics
mise run experiment:go-specialization verify
JED_SPECIALIZATION_TAGS=jed_specialization,jed_specialization_forced \
  mise run experiment:go-specialization race 'Specialization|PlanCache|CustomPlan|Cancel|QueryMemory|Depth|Streaming'
GOFLAGS=-tags=jed_specialization mise run experiment:go-specialization norec
mise run experiment:go-specialization escape
mise run corpus:check spec/conformance/suites/expr/execution_specialization.test
mise run verify
```

`verify` runs the complete Go corpus in memory and disk in ordinary, G2 forced/
selective, forced closures, and G3 forced/selective modes. `corpus` runs the mode selected by `JED_SPECIALIZATION_TAGS`;
normal `JED_CONFORMANCE_*` harness filters/resource controls still apply.
`norec` runs two seeds on Go by default; set `JED_SPECIALIZATION_CORES=go,ts,rust`
to include the other installed core runners. The new shared relation compares
eligible arithmetic/filter roots with equivalent multi-WHEN CASE fallback roots.

G3 uses the `jed_specialization_scan` tag independently of the closure control;
append both tags to exercise the combined implementation. G2 is the default
experimental tag, while G3 remains an explicit comparison.

The dedicated tests cover every statement/lifetime prefix of small admitted
expressions (including nonzero lifetime totals and ties), eager strict operands,
NULL, error equality, cache reuse/rejection/DDL, changed bindings, interleaved
cursors, concurrent sessions sharing one statement, finite query memory,
LIMIT/OFFSET, cancellation, and actual file-page fault/read-order injection.
Shared SQL fixtures remain PostgreSQL-checked. The existing deliberate
OFFSET/projection-skip behavior is covered by a private cursor comparison.

## Ownership and bounds

Each newly resolved plan receives its immutable artifact before the existing
atomic cache publication (or parameter-memo publication). Execution passes the
current row, bindings, environment and meter; none is captured. Descriptors
contain only scalar fields and no source-expression or host-object pointers.
Plans retain their existing source trees for ordinary fallbacks and EXPLAIN.
Cached negative decisions are nil slots/reasons or a nil plan artifact, not a
process cache. An invalidated plan can remain pinned by its existing cursor.

A bounded preflight stops at 192 source nodes or depth 32 before allocating a
representation. G1 terminals still have at most seven nodes/depth three; G2
composition is recursive but bounded. The plan admits at most seven projections
plus one filter. Rejected roots are not retried per row. Tests exercise the
largest accepted node count and depth and reject one beyond each, including
zero allocations for oversized rejection.

Static G2 retains at most one descriptor per source node. The closure control
expands G1 patterns into one closure per source node, discards the temporary
static graph, and retains one root wrapper plus those closures. Both stay below
the shared 1,536 lowered-unit ceiling. Constants and slots are inline; there is
no pool, register file or per-row scratch allocation. The existing cacheability
traversal precedes preparation; this does not bound the entire planner.

On linux/amd64 a static descriptor is 240 bytes and the plan artifact is 216
bytes, plus an 8-byte pointer in the experimental select plan. A conservative
static payload ceiling is 368,856 bytes across eight maximal roots, before
allocator rounding and the existing source trees/cache owners. Actual small
patterns retain far less. Closures have variable capture sizes and add
preparation allocations; whole-query allocation metrics and escape analysis
are part of the evidence. G3 adds a small cursor wrapper with references to the
original cursor and immutable kernels. There is no global cache or sync.Pool.
These bytes are outside logical query-memory accounting; this is not a
whole-engine heap cap.

## Bounded timing

```sh
JED_SPECIALIZATION_STAGE=g2 JED_SPECIALIZATION_RESULTS=bench/results/go-specialization-g2 \
  mise run experiment:go-specialization bench
JED_SPECIALIZATION_STAGE=g3 JED_SPECIALIZATION_RESULTS=bench/results/go-specialization-g3 \
  mise run experiment:go-specialization bench
JED_SPECIALIZATION_RESULTS=bench/results/go-specialization-g3 \
  mise run experiment:go-specialization report
```

The task builds all variants before timing: the pinned clean production revision
(only the identical benchmark source copied into an archived tree), disabled
build, experimental
ordinary and selective kernels. G2 also includes the pinned G1 commit and
closures; G3 includes a G2 static control. It then runs **five fresh process sets**, in
alternating order, sequentially, at **100 ms per workload**, not long soak runs.
Each process measures memory and reopened/warm file hosts; each complete query
is drained and checksummed, with exact cost checked on both parameter bindings.
The report rejects cross-variant checksum/cost disagreement. Setup is untimed;
prepared steady state warms both bindings, while separate ad-hoc and first-call
lanes include preparation. Tests/counters/escape analysis run outside timing.

Targets: `(a+b)*(a-b)` projection, `a+b > $1` filtering, composed boolean
filtering, CASE and nested CASE/COALESCE over 4,096 rows.
Controls include 16-row/LIMIT/OFFSET, finite-cost, hit/miss points, scalars,
unsupported functions, columnar aggregation, ad-hoc and first-call churn.
The declared gate is ≥10% lower median on two target shapes, each faster in at
least four of five process pairs; investigate reproducible >3% regressions in
cheap/unsupported/columnar controls. Shared-host noise is reported, not discarded.

Results, limitations and the adoption decision are in
the [G1 report](results/2026-10-19/report.md) and
[G2/G3 continuation report](results/2026-10-20/report.md).

When a matrix has suspicious controls, `mise run experiment:go-specialization
recheck` reuses the same binaries for **three 250 ms process sets** of only
columnar/scalar/first-call/point-miss/ad-hoc controls. It preserves the original
samples and writes separate `recheck/` evidence; it is not a longer soak or an
outlier replacement.

Set `JED_SPECIALIZATION_RECHECK` to a Go benchmark regex to restrict a recheck to
specific suspicious lanes; every selected variant still runs sequentially.
`JED_SPECIALIZATION_PROFILE_QUERY=boolean` selects the composed-filter lane for
the short diagnostic profile. Neither diagnostics nor profiles enter the timing
matrix.
