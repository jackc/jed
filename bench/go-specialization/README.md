# Go execution-specialization experiment

This is an opt-in G1 implementation of
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
Casts, value/filter AND/OR, CASE/COALESCE, larger trees and other types fall back
as complete roots. Those are explicit negative controls, not partially lowered
expressions. G2 composition and G3 loop replacement have not been implemented.

## Build modes and checks

All entry points run through mise. The executable task is
[`mise-tasks/experiment/go-specialization`](../../mise-tasks/experiment/go-specialization).

| Build tags | Execution |
|---|---|
| none | Ordinary production path; no descriptor field/storage or runtime mode check. |
| `jed_specialization` | Selective: admit the narrow roots on unbounded pull scans. |
| `jed_specialization,jed_specialization_forced` | Forced: also try supported roots on bounded pull scans, from first execution. |
| `jed_specialization,jed_specialization_ordinary` | Build selective descriptors but use the ordinary evaluator. |
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

`verify` runs the complete Go corpus in memory and disk in ordinary, forced and
selective modes. `corpus` runs the mode selected by `JED_SPECIALIZATION_TAGS`;
normal `JED_CONFORMANCE_*` harness filters/resource controls still apply.
`norec` runs two seeds on Go by default; set `JED_SPECIALIZATION_CORES=go,ts,rust`
to include the other installed core runners. The new shared relation compares
eligible arithmetic/filter roots with equivalent CASE-wrapped fallback roots.

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

The fixed grammar needs at most seven source nodes, depth three, seven inline
descriptors, four inline primitive constants and zero pool/register slots per
root. Preparation rejects a larger pattern without recursively walking it. The
plan admits at most seven projections plus one filter; a wider plan falls back
before allocating descriptors. A test checks these tighter bounds against the
shared experiment limits. The existing cacheability traversal still precedes
pattern preparation; this is not a new bound on the engine's entire planner.

On measured linux/amd64, one root descriptor is 200 bytes and the plan artifact
is 216 bytes, plus an 8-byte pointer in the experimental select plan. The worst
payload is 1,816 bytes per eligible plan, plus its pointer, allocator rounding
and the already-existing source tree/cache owners. There is no per-row kernel
allocation or extra row buffer, no global cache, and no sync.Pool. A warmed
prepared query retains its descriptors; ad-hoc/first-call preparation pays their
allocations. These bytes are outside the logical query-memory account, so this
experiment does not establish a whole-engine heap cap.

## Bounded timing

```sh
mise run experiment:go-specialization bench
mise run experiment:go-specialization report
```

The task builds all variants before timing: the pinned clean production revision
(only the identical benchmark source copied into an archived tree), disabled
build, experimental
ordinary and selective kernels. It then runs **five fresh process sets**, in
alternating order, sequentially, at **100 ms per workload**, not long soak runs.
Each process measures memory and reopened/warm file hosts; each complete query
is drained and checksummed, with exact cost checked on both parameter bindings.
The report rejects cross-variant checksum/cost disagreement. Setup is untimed;
prepared steady state warms both bindings, while separate ad-hoc and first-call
lanes include preparation. Tests/counters/escape analysis run outside timing.

Targets: `(a+b)*(a-b)` projection and `a+b > $1` filtering over 4,096 rows.
Controls include 16-row/LIMIT/OFFSET, finite-cost, hit/miss points, scalars,
unsupported functions, CASE, columnar aggregation, ad-hoc and first-call churn.
The declared gate is ≥10% lower median on two target shapes, each faster in at
least four of five process pairs; investigate reproducible >3% regressions in
cheap/unsupported/columnar controls. Shared-host noise is reported, not discarded.

Results, limitations and the adoption decision are in
[`results/2026-10-19/report.md`](results/2026-10-19/report.md).

When a matrix has suspicious controls, `mise run experiment:go-specialization
recheck` reuses the same binaries for **three 250 ms process sets** of only
columnar/scalar/first-call/point-miss/ad-hoc controls. It preserves the original
samples and writes separate `recheck/` evidence; it is not a longer soak or an
outlier replacement.
