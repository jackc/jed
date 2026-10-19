# Runtime compilation in the Ruby experiment

The maintainer explicitly requested investigating Ruby `eval` as a way to compile
prepared query plans and let Ruby's JIT optimize query-specific code. This is a
narrow exception to the prohibition on generated engine implementations: the
compiler and its semantics are handwritten Ruby, operating on already resolved
expressions and eligible single-table scan/filter/projection plans. The Ruby core
remains frozen, experimental, and explicitly selected for every test, benchmark,
and feature update.

## Execution boundary

The expression tier specializes kernels inside existing query plans. A second
tier fuses eligible single-table scan/filter/projection loops, including streaming
OFFSET/LIMIT. The planner, access paths, row sources, sort and aggregate algorithms,
storage, and host API remain the existing engine. A resolved expression can emit a
Ruby method containing straight-line locals and branches instead of recursively
dispatching through expression objects. Ruby's JIT may compile this method after
its normal warmup; the library does not enable or configure a JIT.

Only fixed, compiler-owned templates, validated operators, and numeric local/pool
ordinals enter the source. SQL text, identifiers, literals, error messages, types,
and fallback expression objects stay in a separate data pool. No SQL string is
passed to `eval`, and no SQL value becomes a method name or interpolated Ruby
literal. Each kernel is an instance of a collectible anonymous class with one
generated `call(engine, row, params)` method. A pipeline instead receives the current
engine, input, parameters, output sink, OFFSET and LIMIT. Neither captures an engine,
session, row source, output sink, or parameter binding. `class_eval` defines the
method once, outside the row loop.

Admission starts with plain integers and booleans: arithmetic and comparisons,
integer casts, unary operations, three-valued boolean control flow, searched CASE,
and selected integer functions. Nominal/open/collatable types, host calls,
subqueries, windows, and unsupported expression forms use the existing evaluator.
A fused projection may delegate an unsupported complete root to that evaluator,
in its original position. There is no fallback after execution has started: a
compiled error is returned immediately, never retried.

Pipeline admission requires complete supported expression trees, including trivial
column/literal projections. It has no expression fallback inside its generated
loop. Specialized point and scalar paths retain their existing execution; DISTINCT,
blocking ordering, profiling, and finite query-memory accounting keep the existing
operators. A streaming plan may consume an index-ordered input without generating
a sort. Host callbacks and unsupported types select the existing operator and
expression tier. Array execution preserves its indexed `while`; streaming uses
the existing source's internal `each` so it introduces no Enumerator Fiber per row.

## Semantic obligations

Resolution and contextual typing finish before emission. Filter conjunction
marking is preserved: a filter AND stops on FALSE or NULL, while a value AND
stops only on FALSE. Ordinary binary arguments remain eager even when the first
is NULL; CASE and COALESCE preserve lazy branch access. Integer division truncates
toward zero, remainder follows the dividend, and overflow retains the original
SQLSTATE and message.

Every interior expression node charges in its original preorder. Compilation
must not combine charges, move a row access before its charge, remove a zero-work
checkpoint, or reorder a host callback. This preserves statement/lifetime budget
failures, lazy storage failures, cursor pulls, and host batch accounting. Existing
output, query-memory, spill, and storage charges remain at their original sites.
Grouping substitutions keep the original evaluator path.

Array pipelines preserve the existing bulk scan charge before the loop and bulk
output charge after successful completion. Streaming pipelines preserve source
pull, scan-row charge, predicate, accepted-row OFFSET, projection, output charge,
sink append/suspension, then LIMIT termination in that order. OFFSET rows do not
evaluate projections. LIMIT 0 does not enter the source iterator, while scan
preparation remains at its original site. Closing a suspended cursor unwinds the
same scanner and engine cleanup scopes. Charges are never summed or hoisted across
these boundaries, including when statement/lifetime budgets are enabled.

## Lifetime and bounds

Resolved plan caches retain at most 64 entries each and already validate table
publication, parameter types, type registry, environment, timezone, and collation.
Generated kernels belong to those plans and inherit their invalidation. The first
execution uses the evaluator; the second use may compile. Unsupported shapes
record a negative admission to avoid repeated emitter work. Internal diagnostic
switches can force first-use admission or disable compilation for differential
testing; neither changes public SQL or embedding APIs.

An expression, fused projection, or pipeline is limited to 192 emitted nodes,
depth 32, 256 pool entries, and 32 KiB source. A plan retains at most four kernels
(predicate, projection, sort keys, and pipeline) and 64 KiB total source. A pipeline
is attempted before separate expression kernels on eligible execution; fallback
operators may later request those kernels under the same total cap. Rejection is
cached. The internal pipeline-disable diagnostic keeps the expression tier active
for differential comparisons; it is not a public API.
Aggregate arguments in the general and spill executors use a separate 64-entry
LRU cache. Those expressions are resolved afresh, so the exact emitted source and
immutable scalar pool form the cache key. Emission happens before lookup;
`class_eval` happens only after reuse. Unsupported argument vectors have no
compiled program, and an admitted kernel is reused within all groups of the same
resolved expression. This avoids retaining stale folded subquery values or using
an AST object ID as a persistent execution key. Aggregate folding and grouping
substitutions remain unchanged.
Exceeding a limit selects the evaluator. These bound retained source and compiler
work, not total VM/JIT memory; active cursors can retain their own old plans and
Ruby controls machine-code reclamation. Validation and benchmarks therefore also
run with process memory/time limits and measure generated-code churn.

## Evidence required

Keep `Engine#evaluate` as the independent within-core control. Compare values,
types, errors/messages, exact costs, and failure prefixes, including lazy faults,
parameters, host boundaries, and invalidated plans. Run the shared corpus as well
as host/internal tests; force admission in a dedicated corpus lane because most
corpus statements execute only once. Record whole-query before/after timings for
interpreter, YJIT, and ZJIT, plus cross-core controls, compile/first-call latency,
warmup, allocation and code churn. Direct VM evidence must distinguish JITed
generated methods from a JITed surrounding engine. Expression-only speedups are
not evidence of equivalent whole-query speedups or universal Go/Rust parity.
