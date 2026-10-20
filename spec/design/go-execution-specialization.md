# Go: static execution kernels — proposed experiment

> **Proposal, not implemented or ratified for production.** This investigation
> can run independently of Rust and TS. It inherits the
> [experiment contract](execution-specialization-experiments.md), including
> semantic invariants, structural caps, coverage and performance gates.
> Keep the engine memory-safe and pure Go: no new dependency, cgo, FFI, unsafe
> trampoline, runtime compiler or public API change.

## 1. Question and hypothesis

Can preparing a small set of ordinary typed Go kernels remove enough dispatch,
call and value-copy overhead to improve complete queries?

The [earlier expression VM](https://github.com/jackc/jed/blob/a85f2127cc64b8440cf86c3604aaac0ec10a0676/spec/design/expression-vm-experiment.md)
made file arithmetic/boolean scans roughly 16–20% slower and barely changed
their allocation counts. Its process cache also retained too much state. This
proposal therefore starts with common multi-step operations, not another general
instruction interpreter. A closure-per-node implementation is a useful control,
not the assumed production architecture.

Go's ahead-of-time compiler compiles the kernel bodies. Preparation selects and
binds those bodies to a query; it does not create a new native function for every
SQL expression. Any indirect calls and lost inlining must be measured explicitly.

## 2. Integration and representation

Inspect [`(*rExpr).eval` / `evalFilter`](../../impl/go/eval.go), the resolved
types in [`rexpr.go`](../../impl/go/rexpr.go),
[`PreparedStatement`](../../impl/go/api.go), and the plan cache/scan paths in
[`exec_scan.go`](../../impl/go/exec_scan.go).

Proposed private components are a bounded admission/binding module and a small
set of static scalar/filter/projection kernels. Favor a concrete descriptor
holding a kernel kind, validated column/parameter slots, resolved widths and
immutable constants. Select the function or loop once during execution setup;
avoid introducing interface dispatch or a global map lookup per node/row.
Compare direct switch-at-setup selection against a function value if needed.

Initially recognize a small explicit family:

| Shape | Work selected once |
|---|---|
| Integer column compared with parameter/literal | Operand slots, comparison, integer proof and cost descriptors. |
| Checked arithmetic feeding comparison/projection | Supported arithmetic shape, widths, loads and original checkpoint sequence. |
| NULL tests and short boolean combinations | Nullable slots and filter/value branch behavior. |
| Supported filter plus projection | Per-root kernels and existing row/output boundaries. |

These are handwritten patterns over resolved nodes. Match complete typed roots;
never match raw SQL text or trust only a result type. Do not build a combinatorial
catalog of every expression shape. Add a pattern only with a workload motivating
it and differential coverage for its checkpoint sequence. Shared operator/type
facts continue to come from existing spec data.

## 3. Ownership and sharing

Install immutable bound kernels with the existing validated cache entry. Go
prepared statements can be shared across goroutines even though an executing
handle remains single-goroutine. Publish only a complete immutable artifact;
never mutate captured slots/constants or an initialized plan in place while
another session can use it. Use the established atomic cache publication pattern.
Bounded duplicate preparation under a race is acceptable to investigate, but
must be measured and must not grow a permanent duplicate cache.

Current bindings, input row, meter and scratch are call arguments or execution
state. A closure must not capture an engine/session/current parameter slice.
Scratch belongs to each cursor/invocation. Avoid `sync.Pool` in the first slice:
it obscures retention and does not establish a deterministic memory bound.
No process-global expression-address cache is allowed.

Record compiler escape analysis and allocation profiles to determine whether
descriptors, closures, error returns or parameter wrappers unexpectedly escape.
Treat these reports as diagnostics; measure allocations in actual query runs.
Do not require a particular inlining decision as a correctness condition.

## 4. Stages and execution fences

**G1 — static scalar patterns.** Start with comparisons/NULL tests, then a small
checked-arithmetic family. Bind once per valid root/context. Forced mode admits
eligible corpus queries on first use. Selective mode keeps point/scalar and
existing columnar paths on their established implementations until evidence
justifies widening. Complete unsupported roots retain ordinary evaluation.

**G2 — bounded composition.** Add explicit three-valued boolean branches and
selected CASE/COALESCE patterns only where profiling/benchmarks motivate them.
Compare static composition to a closure-per-node control over the same admitted
surface. No version may speculate a branch, elide an original guard or replace
checked arithmetic with unchecked wraparound. Inputs that exceed the common
structural limits select ordinary evaluation.

**G3 — scan-loop specialization.** If calls or intermediate construction still
dominate, choose a precompiled filter/projection loop at the existing nonblocking
single-table cursor boundary. Start row-at-a-time with the original source and
sink; preserve pull, scan charge, predicate, accepted-row OFFSET, projection,
output charge/ownership, yield and LIMIT ordering. LIMIT 0 must not pull input.
Do not introduce a materialized survivor buffer to claim a fusion speedup.

Batching beyond an existing columnar boundary is deferred until a separate
proposal proves budget, deferred-error, cursor and logical-memory behavior.
Keep existing vectorized/encoded-span kernels as controls. Aggregates, sorting,
joins, DML, host calls and volatile expressions retain their current algorithms.

An artifact may remove physical wrapper allocations only when the logical
reserve/release schedule remains unchanged. A different aggregate reduction
order, scan visitation or spill threshold is not an implementation-private
optimization under this proposal.

## 5. Go-specific verification

Run the common forced/selective corpus and host checks, plus:

- `-race` tests with one prepared statement used through separate permitted
  sessions/handles, changing parameters and replacing cache entries. Never make
  a test misuse a single-goroutine handle to manufacture unsupported concurrency.
- Assert that every warmed valid plan reuses its descriptors, unsupported plans
  reuse bounded rejection state, and no descriptor captures the previous call's
  bindings. Interleaved live cursors have separate scratch.
- Check cancellation at every prescribed guard using deterministic injection,
  including fused-loop close/error cleanup and statement/lifetime error priority.
- Compare retained descriptor/constant/source ownership after invalidation and
  churn. Include cache races, repeated errors and abandoned cursors.
- Require no per-row closure/descriptor construction in admitted loops; report
  total query allocations and bytes as well as evaluator-only allocations.

SQL semantics stay in shared fixtures; race, cache and allocation checks are
legitimate per-core tests. Run all existing depth/resource tests with enabled
paths. Forced mode cannot count fallback-only fixtures as kernel coverage.

## 6. Measurement and decision

Measure clean production, experimental ordinary, static kernels, and (where
implemented) equivalent closure composition. Add G3 only after measuring G1/G2
so its incremental contribution remains visible. Compare builds with identical
compiler settings; an empty/disabled build-tag path is still checked against
clean production for layout/setup overhead.

Record whole-query latency, allocations/query, allocated bytes/query, warm
cache/compile counts and retained state. Use arithmetic/filter/CASE target
families appropriate to declared admission, plus point, ad-hoc, LIMIT/OFFSET,
finite-budget, unsupported-function and columnar controls on memory and files.
Disclose when a workload is unsupported rather than attributing its ordinary
execution to a kernel. Apply the shared decision protocol.

PGO is an optional separate measurement after the main comparison. Train on a
declared representative workload set, evaluate on held-out workloads, and build
both baseline and candidate with the same PGO policy. Report it separately so
PGO gains cannot be credited to specialization. It is a build experiment, not a
runtime SQL compiler or a required deployment setting.

Success means a small pattern set that improves complete queries without a
regression in cheap controls or unsustainable pattern growth. If dispatch was
not the bottleneck, prefer a measured value-layout/allocation improvement or keep
the existing evaluator. A native JIT or Go-emitted WASM executed by wazero remains
a separate dependency/architecture proposal, not an automatic next stage.

Deliver a Go-only research branch, shared fixtures, pattern/admission coverage,
allocation and race evidence, raw timings and a scoped adoption-or-rejection
report. No Rust/TS implementation is a prerequisite.
