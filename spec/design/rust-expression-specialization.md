# Rust: cached typed expression programs — proposed experiment

> **Proposal, not implemented or ratified for production.** This investigation
> can run independently of Go and TS. It inherits the
> [experiment contract](execution-specialization-experiments.md), including
> semantic invariants, structural caps, coverage and performance gates.
> No dependency, unsafe code, JIT backend or public API change is proposed.

## 1. Question and hypothesis

Can Rust retain the scan improvement of the expression-VM experiment while
removing its repeated lowering and cheap-query overhead?

The [prior report](https://github.com/jackc/jed/blob/a85f2127cc64b8440cf86c3604aaac0ec10a0676/spec/design/expression-vm-experiment.md)
records approximately 17% less time for arithmetic scans and 30% for boolean
filters, but 18% more for prepared point queries. It used compact typed registers
and lowered once per cursor, including prepared execution. That motivates
plan-owned reuse and selective entry, not a general statement VM.

The first measured candidate is a flat, immutable typed scalar program with
per-execution registers. A small static-kernel control may establish whether
flat dispatch is necessary for common shapes. Native machine-code generation,
Cranelift/LLVM, WASM runtimes and external `rustc` invocation are out of scope.

## 2. Integration and ownership

Inspect these existing seams at the investigation's pinned base:

- [`RExpr::eval` / `eval_filter`](../../impl/rust/src/executor/eval.rs).
- [`PreparedStatement`](../../impl/rust/src/api.rs) and its validated `CachedPlan`.
- The scan/cursor implementation under [`executor/`](../../impl/rust/src/executor/).

Proposed private components are an admission/lowering module, immutable
`ExprProgram`, and execution-owned `ExprScratch`. Names are illustrative. Keep
the lowering algorithm handwritten Rust, with a small explicit instruction enum
and existing checked value kernels. No generated implementation or deep generic
framework is needed.

A valid cached scan plan owns a bounded bundle of predicate/projection artifacts.
At execution setup, select the evaluator once for each complete root and bind
the immutable program to the cursor. The row loop receives that choice; it must
not hash an expression pointer or search a process cache per evaluation.
For preparation reuse, lower at most once per valid plan/root/context, including
negative admission. Plans that are not cacheable retain their current validity
rules; the first selective version may leave them on ordinary evaluation.

Preserve the existing `Rc`/thread-affine prepared-plan contract. Do not require
`Arc`, `Send` or a global lock merely to share programs. If lazily installing a
program into an existing plan/cache entry requires interior mutability, confine
the borrow to setup and release it before execution. Two live cursors can share
an immutable program but never mutable registers or a meter. An old cursor keeps
its pinned program when the statement is rebound/replanned elsewhere.

Constants and slot/type descriptors are owned by the program or borrowed through
its owning immutable plan with explicit lifetimes. They must not borrow a current
row or parameter vector. General `Value` conversion belongs at input/output
boundaries; loaded deferred values retain the original resolution-on-touch seam.

## 3. Program semantics

Each slot has an admitted static type and NULL state. An integer slot can carry
an `i64` payload with its resolved width in program metadata; booleans use their
own representation. The first implementation may use one simple slot enum
rather than multiple arrays: measure before complicating the layout.

The program must express, without moving observable work:

- Original entry guards, including leaves, and original node charges before
  children. Use the shared operator cost data, never private opcode weights.
- Column/parameter/constant loads in the original order.
- Checked arithmetic at the resolved width, including the specified division,
  remainder and cast cases. Native integer operators alone are insufficient.
- Explicit conditional jumps and merges for lazy CASE/COALESCE and three-valued
  boolean evaluation. Value AND and filter AND are distinct contexts.
- Original post-operand guards and conversion/result construction boundaries.

The compiler verifies every jump target and slot use before publishing a
program. Normal safe bounds checking remains enabled; no unchecked indexing,
raw code pointers or hand-written machine code. Use bounded traversal so a
lowering pass cannot undermine the existing 1 MiB optimized-build stack promise.
Keep the source expression available through the ordinary plan for fallback and
diagnostics; report that the program adds a representation instead of replacing
the tree.

## 4. Admission and stages

**R1 — integrated expression semantics.** Start with columns/parameters/constants,
integer arithmetic/comparisons and NULL tests in single-table row scans. Add
boolean branches, CASE/COALESCE and casts in small fixture-driven slices. Forced
mode admits eligible roots immediately for corpus coverage. All excluded
operators continue through ordinary execution.

**R2 — cached selective execution.** Retain programs with validated prepared plans,
including the existing exact-binding custom-plan behavior. Selective mode bypasses
point/scalar queries, bare-column projections and existing encoded/columnar paths
before lowering. Start with reused cacheable scans containing nontrivial admitted
roots. Record reuse policy and any structural/estimated-row threshold before
measurement; do not use wall-clock timing as an engine input. Measure first-use
compilation separately from warm reuse. A reuse counter must not retain session
state or alter the physical plan.

**R3 — static-kernel comparison, conditional on R2.** For the measured dominant
small shapes, compare a handwritten typed function selected at setup against the
program interpreter. This is a control for the cost of instruction dispatch.
Keep storage iteration and materialization unchanged. Pipeline fusion or new
aggregate/sort/join integration requires a subsequent scoped experiment.

Each stage has a named coverage inventory and reports admitted root invocations.
Finite cost/lifetime limits are supported on admitted expression paths; if a
stage cannot preserve a resource or profiling path, explicitly exclude that
path and retain it as a baseline control rather than silently altering semantics.

## 5. Rust-specific checks

In addition to the common suite:

- Count lowering across repeated prepared executions; a valid warm plan must not
  recompile per cursor. Verify relevant invalidation and negative-cache reuse.
- Hold two cursors from the same prepared statement with different bindings,
  interleave pulls and close one early. Assert separate scratch and correct
  destruction after plan replacement, errors and abandonment.
- Run enabled optimized-build depth tests on the prescribed 1 MiB stack, including
  compiler-cap rejection and the ordinary fallback at deeper accepted SQL depths.
- Inject cancellation at prescribed guards; compare the ordinary path's error,
  accrued cost and cleanup. No batch or instruction loop may skip checkpoints.
- Inspect program capacity, slot capacity, constants and retained source ownership
  separately. Bounded program length does not prove bounded allocator overhead.

Shared SQL fixtures should cover both filter and projection contexts. Per-core
tests assert admission, cache ownership and internal validation. Run normal and
forced-memory/disk corpus lanes plus relevant memory/spill/host tests; a corpus
pass with unsupported forms falling back does not establish those forms are
compiled.

## 6. Measurement and decision

Use at least three variants: clean production tree, experimental ordinary tree,
and cached selective program. Add forced mode for diagnostics and the small
static-kernel variant only when R3 is reached. Use the same release profile and
checks; separate allocator/profiling instrumentation from timing.

Repeat the earlier arithmetic, boolean, CASE and finite-cost scan families, then
prepared point hit/miss, ad-hoc point/scalar, unsupported function and columnar
controls. Measure low/high row counts, first use, repeated execution, changing
parameters, invalidation and churn on memory and warm files. Record compile
counts, complete-query time, exact results/cost, program/scratch retention and
allocation counts where available. Apply the shared decision protocol.

Success is a narrow reusable scan optimization with demonstrated control-path
behavior, not merely a faster evaluator microbenchmark. If caching removes setup
regressions but scan gains disappear on current production, retain that negative
result. If static kernels win the relevant shapes, choose the smaller mechanism.
Native JIT is a possible later proposal only if residual dispatch is measured to
dominate and its dependency/safety/packaging costs can be justified separately.

Deliver a Rust-only research branch, shared regression fixtures, an explicit
admission/coverage table, reproducible results and an adoption-or-rejection report.
The investigation neither waits for nor requires the Go/TS experiments.
