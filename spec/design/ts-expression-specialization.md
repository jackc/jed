# TypeScript: CSP-compatible expression specialization — proposed experiment

> **Proposal, not implemented or ratified for production.** This investigation
> can run independently of Rust and Go. It inherits the
> [experiment contract](execution-specialization-experiments.md), including
> semantic invariants, structural caps, coverage and performance gates.
> The proposed execution uses ordinary shipped JavaScript functions only. No
> runtime source generation, permissive CSP, special Node flags, new dependency
> or public API change is required.

## 1. Question and hypothesis

Can closures and a small set of static typed kernels produce useful complete-query
gains while working under strict CSP and code-generation-disabled Node?

The [local probes](../../bench/query-compilation/README.md) found roughly
1.15–1.28× throughput from closure specialization over the current tree evaluator
on three small integer expressions. Generated JavaScript added about another
8%. Those are kernel results, not integrated SQL or browser performance. This
experiment tests the more portable approach first and keeps the source-generating
probe as historical evidence, not a production import or required test backend.

Ordinary functions and closures can be optimized by the host's JIT without
string evaluation. Optimization/inlining is a host decision, never a correctness
assumption or a promise that V8, SpiderMonkey and JavaScriptCore behave alike.

## 2. Integration and type proof

Inspect [`evalExpr` / `evalFilter`](../../impl/ts/src/eval.ts), resolved types and
scan/cache code in [`executor.ts`](../../impl/ts/src/executor.ts), and
[`PreparedStatement`](../../impl/ts/src/api.ts). Keep one semantic TS core for
Node and browser. This work does not require the [async runtime](ts-runtime.md)
proposal, change worker placement, or add suspension inside scalar kernels.

Proposed private components are typed admission, closure/kernel binding, and a
bounded immutable artifact attached to the validated plan/cache entry. Build
one callable per complete admitted root at setup. The scan operator holds that
callable instead of checking a WeakMap or mode flag for every row/node.

Column/parameter `RExpr` nodes alone do not prove their value family. Admission
must obtain exact resolved types from the resolver/plan schema and binding-type
context, and retain width/NULL facts required by the selected operation. An
integer result does not prove integer operands: date subtraction is an explicit
negative case. Nominal types that happen to contain integers are not implicitly
admitted. Missing proof means ordinary evaluation, never inference from the first
row or a conversion through JS `Number`.

## 3. Executable representation

The first backend binds ordinary prewritten functions into closures. For example,
a comparison callable captures validated operand readers, the selected comparison
helper, and immutable cost/type descriptors. It receives the current row,
parameters/environment and meter on each invocation. It does not capture SQL
text, an engine, a session, a transaction or current bindings.

Begin with ordinary `Value` inputs/results and existing checked arithmetic
helpers. This isolates removal of node-kind/type-selection dispatch from value
representation changes. Where profiling warrants it, add a handwritten kernel
for a larger common shape to reduce child-closure calls. Compare each extension
against the simpler closure backend on the same admitted inputs.

A later within-experiment variant may carry `bigint`/boolean payloads and explicit
NULL state internally, boxing at escape points. Preserve width checks for every
intermediate. Do not use floating-point Number arithmetic for i64 or unchecked
BigInt truncation. A typed array's wraparound is not jed's overflow behavior.
Any helper or temporary that can expose a result must keep its original type and
metadata contract. Text/UTF-16 collation differences, floats, decimals and open
types remain outside the initial surface.

Callable argument shapes and descriptor layouts should be stable, with a modest
number of handwritten variants. Measure polymorphic-call/shape effects rather
than constructing an enormous specialized-function family. Preserve existing
variable-cost and zero-charge guard behavior even where a primitive comparison
could otherwise appear as one JS operator.

## 4. Lifetime and admission

Artifacts live with validated cached plans and share their invalidation, including
database/attachment identities, catalog/estimator revisions, exact custom-plan
bindings and relevant type/environment context. A global WeakMap alone is not a
validity or capacity policy. Cache negative admission within the bounded owning
plan. The number of closures/descriptors includes the full captured graph under
the common structural limits, not just one top-level function reference.

Two active cursors may reference the same immutable artifact while holding
different parameters, meters and scratch. Helpers must not stash scratch on the
callable itself. If reentrancy can occur through a retained ordinary boundary,
give it a separate invocation frame and release references on both success and
error. Unsupported host-calling roots stay wholly ordinary in the initial slice.

Only primitive immutable constants and validated slot/type descriptors may be
captured. Do not retain arbitrary fallback expression graphs through an otherwise
admitted kernel. Keep the ordinary resolved plan as its existing owner instead.
Measure closures, retained plan graphs and process memory separately; GC and JIT
metadata retention cannot be inferred from the number of live artifacts.

## 5. Stages

**T1 — typed closures.** Integrate integer/boolean leaf and operation binding at
the existing nonblocking single-table row-expression boundary. Add arithmetic,
comparisons/NULL tests, then lazy boolean/CASE/COALESCE/casts with shared fixtures.
Preserve eager operand evaluation even when NULL will determine the result.
Finite cost/lifetime limits follow the original guard sequence.

**T2 — selective caching.** Reuse artifacts with prepared plans and select the
root callable once per execution. Forced mode attempts eligible compilation on
the first corpus execution. Selective mode initially bypasses point/scalar,
bare-column and encoded/columnar paths, and targets repeated cacheable scans with
nontrivial supported roots. Declare any reuse/row-count threshold before timing;
do not use measured time or require JIT introspection in engine code.

**T3 — static patterns and representation controls.** If T1/T2 show residual
closure-call or boxing overhead, add a small static-kernel family and an optional
unboxed-internal variant, each separately measurable. Fusing filter/projection
into an existing loop is allowed only after its source pulls, guards, OFFSET,
projection, memory ownership, output and yield/LIMIT order are explicitly pinned.
Do not prefetch a batch or materialize survivors to manufacture a speedup.

Normal `evalExpr` remains complete. Fallback happens before an unsupported root
runs; no catch-and-retry on a query error. Aggregates, sort comparators, joins,
DML, host functions, asynchronous kernels, runtime WASM emission and generated JS
are subsequent proposals, not prerequisites or hidden additions to this one.

## 6. CSP and host acceptance gates

The production-reachable experiment graph must contain no `eval`, `Function`
constructor, `node:vm` compiler, dynamically generated module/script/blob, or WASM
compiler used to implement specialization. Never install a Trusted Types policy
or relax a document/worker response header. There is no capability probe: the
engine only composes shipped functions and data. A lexical scan is useful but
insufficient; inspect the actual browser bundle/import graph and run host tests.

Execute integrated SQL, not only `new Function` capability checks, under:

- Browser scripts served with restrictive `script-src 'self'`, using an external
  permitted worker script with its own equally restrictive CSP. No eval allowance.
- A Trusted-Types-enforced Chromium deployment, using host-approved static worker
  loading; no policy creation by the engine. The host's legitimate script-loading
  setup is separate from query specialization.
- Ordinary Node with no special flags, and Node with
  `--disallow-code-generation-from-strings` as an additional diagnostic gate.

Use the existing browser harness with Chromium, Firefox and WebKit where available.
Report missing runtimes and untested policies; do not download browsers or new
dependencies automatically. Include memory and browser OPFS behavior where the
existing harness supports it. Inspect results written by actual page/worker
scripts so DevTools evaluation cannot bypass the policy being tested.

`--jitless` is an optional diagnostic, never a deployment requirement. Use the
same prebuilt plain-JS artifact for both compared modes because native TS
stripping in the previously tested Node required WASM unavailable in that mode.
No V8-specific intrinsic, optimization flag or inspector connection may be needed
for correct execution. Extension policy compatibility is a design property to
verify in packaging; strict-CSP tests alone do not constitute extension approval.

## 7. Correctness, measurements and decision

Run the shared forced/selective memory/disk corpus, relevant memory/spill tests,
and host/cache tests. Include stack tests with specialization enabled at the
existing 512 KB V8 budget, including admitted depth and ordinary fallback past
the compiler cap. Assert that closures retain no old parameter values, that
replanning clears old eligibility, and that independently active cursors do not
share scratch. Preserve the actual TS cancellation boundary; do not assume a
synchronous kernel can observe a same-thread AbortSignal change mid-call.

Measure clean production, experimental ordinary, closures and static kernels as
separate variants. Compare unboxed internals only if implemented as a separately
identified stage. Use complete prepared/ad-hoc queries and the common controls;
include mixed admitted/unsupported workloads and changing parameter values to
expose polymorphism, cache churn and JIT warmup effects. Browser and Node timings
are separate results, not a single JS number. Keep worker transport/public API
costs unchanged and report both execution and end-to-end timing when measured.

Retain first-use preparation, first call, warmup trajectories, steady state and
post-release retention measurements. GC stays enabled. Runtime optimization
traces may explain results in a separate diagnostic run; they must name the
functions of interest and are never themselves proof of an overall speedup.
Heap measurements must disclose whether they include the rest of the engine,
runtime and tooling. Apply the common complete-query decision criteria.

Success is a useful CSP-compatible optimization under ordinary deployment,
with mandatory fallback coverage and bounded added structure. If static kernels
provide the benefit and generic closures do not, prefer the smaller pattern set.
If the only gains require generated source, report that outcome without relaxing
this proposal's host requirements. A later optional source-generating backend
would require its own proposal and would not replace the complete no-eval path.

Deliver a TS-only research branch, shared regression fixtures, browser/Node policy
results, admission/retention evidence, raw measurements and a scoped decision.
Neither Rust/Go changes nor the async-runtime migration are prerequisites.
