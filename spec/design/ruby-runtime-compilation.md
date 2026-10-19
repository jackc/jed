# Runtime compilation in the Ruby experiment

The maintainer explicitly requested investigating Ruby `eval` as a way to compile
prepared query plans and let Ruby's JIT optimize query-specific code. This is a
narrow exception to the prohibition on generated engine implementations: the
compiler and its semantics are handwritten Ruby, operating on already resolved
expressions. The Ruby core remains frozen, experimental, and explicitly selected
for every test, benchmark, and feature update.

## Execution boundary

The first implementation specializes expression kernels inside existing query
plans. The planner, access paths, row streams, sort and aggregate algorithms,
storage, and host API remain the existing engine. A resolved expression can emit a
Ruby method containing straight-line locals and branches instead of recursively
dispatching through expression objects. Ruby's JIT may compile this method after
its normal warmup; the library does not enable or configure a JIT.

Only fixed, compiler-owned templates, validated operators, and numeric local/pool
ordinals enter the source. SQL text, identifiers, literals, error messages, types,
and fallback expression objects stay in a separate data pool. No SQL string is
passed to `eval`, and no SQL value becomes a method name or interpolated Ruby
literal. Each kernel is an instance of a collectible anonymous class with one
generated `call(engine, row, params)` method. It captures no engine, session, row,
or parameter binding. `class_eval` defines the method once, outside the row loop.

Admission starts with plain integers and booleans: arithmetic and comparisons,
integer casts, unary operations, three-valued boolean control flow, searched CASE,
and selected integer functions. Nominal/open/collatable types, host calls,
subqueries, windows, and unsupported expression forms use the existing evaluator.
A fused projection may delegate an unsupported complete root to that evaluator,
in its original position. There is no fallback after execution has started: a
compiled error is returned immediately, never retried.

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

## Lifetime and bounds

Resolved plan caches retain at most 64 entries each and already validate table
publication, parameter types, type registry, environment, timezone, and collation.
Generated kernels belong to those plans and inherit their invalidation. The first
execution uses the evaluator; the second use may compile. Unsupported shapes
record a negative admission to avoid repeated emitter work. Internal diagnostic
switches can force first-use admission or disable compilation for differential
testing; neither changes public SQL or embedding APIs.

An expression/fused projection is limited to 192 emitted nodes, depth 32, 256 pool
entries, and 32 KiB source. A plan retains at most three kernels and 64 KiB source.
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
