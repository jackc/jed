# Runtime compilation in the Ruby experiment

The maintainer explicitly requested investigating Ruby `eval` as a way to compile
prepared query plans and let Ruby's JIT optimize query-specific code. This is a
narrow exception to the prohibition on generated engine implementations: the
compiler and its semantics are handwritten Ruby, operating on already resolved
expressions, eligible single-table scan/filter/projection plans, typed ORDER BY
comparators, primitive aggregate input/reduction loops, and materialized WHERE
and grouping-key loops. The Ruby core
remains frozen, experimental, and explicitly selected for every test, benchmark,
and feature update.

## Execution boundary

The expression tier specializes kernels inside existing query plans. A second
tier fuses eligible single-table scan/filter/projection loops, including streaming
OFFSET/LIMIT. A third tier specializes comparisons of precomputed ORDER BY keys.
A fourth tier specializes primitive aggregate input collection and reduction.
A fifth tier specializes materialized relation filtering and grouping-key evaluation.
The planner, access paths, row sources, grouping and spill algorithms,
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
engine, input, parameters, output sink, OFFSET and LIMIT. An ordering kernel receives
two sort entries containing precomputed keys and their input ordinals. A relation
kernel receives the materialized row array and, when needed, the current memory
owner, columns, and mask. None captures an engine,
session, row source, output sink, or parameter binding. `class_eval` defines the
method once, outside the row loop.

Admission starts with plain integers and booleans: arithmetic and comparisons,
integer casts, unary operations, three-valued boolean control flow, searched CASE,
and selected integer functions. The explicitly requested text extension admits
plain text leaves, identity text casts, and ordinary comparisons under the fixed
`C` collation. A subsequent extension admits simple literal LIKE/NOT LIKE
operators under statically known C labels. It preserves text collation labels,
eager operand evaluation, NULL propagation, and variable-width comparison charges. Linguistic collations,
nominal/open types, host calls,
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

Ordering admission requires a complete key vector of plain integers, booleans,
or proven NULL literals. Validated direction and NULL placement select fixed
branches at compilation. Non-NULL integers compare directly; booleans retain
FALSE-before-TRUE ordering. Complete ties compare the original input ordinals.
Unsupported, nominal, and collatable keys retain the original comparator. The
kernel's cached method Proc retains only the kernel itself. Blocking single-table
sorting uses it with the existing bounded heap, full sort, or external sorter;
other operators keep their existing comparisons.

Aggregate admission specializes the memory host's general executor in its existing
two phases: first evaluate FILTER and arguments for every input, then reduce the
collected non-NULL scalars. It supports count, i16/i32 sum widened to i64,
integer/boolean min/max, boolean folds, and integer bit folds. Arguments and FILTER
must be complete supported expression trees. DISTINCT, aggregate ordering,
ordered sets, windows, host calls, named/collatable types, decimal, and float reductions
retain the ordinary executor. File hosts retain their existing aggregation.

Materialized-relation admission specializes the memory host's final residual
WHERE filter and the complete grouping-key matrix. It requires fully supported
expression trees, including the admitted C-text surface. Filtering returns the original accepted row
objects in input order. Key evaluation remains row-major and left-to-right
within each key vector; grouping-set construction and hash grouping stay in the
ordinary executor. File hosts, unsupported predicates, and unsupported complete
key vectors retain their existing loops. Non-array row sequences also retain
their original iterators. Neither path captures a resolved
expression or silently delegates an unsupported subtree.

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

Text comparison operands remain eager, including the right operand when the
left is NULL. Comparison work is charged after both reads and before comparison,
using the ordinary Unicode-character-length rule. Text results preserve their
exact transient collation labels through fixed, frozen compiler-owned C-label
constants; no resolved label or loaded table enters the pool. A direct C-string
comparison requires static C provenance and guards on each runtime value's exact
class. Unproven collation provenance or unexpected classes use the fallback
helper's original dynamic-label and comparison semantics. This guard is not
permission to compile a statically linguistic, conflicting, indeterminate, or
nominal expression. Explicit COLLATE
nodes, text functions/concatenation, text-returning CASE, text aggregate reductions,
and text sort comparators remain unsupported by this extension.

When a comparison and both immediate text leaves have admitted static C labels,
the leaves may use private plain strings inside that comparison. A canonical
frozen literal already owned by the pool needs no temporary wrapper. An exact
String or CollatedText column or parameter is copied with `String.new` at the
original wrapping point, before reading the next operand. This preserves the byte snapshot and
removes singleton methods just as the transient C wrapper did. An existing
CollatedText label would have been replaced by the child's static C label too.
Other runtime classes retain ordinary wrapping. If an unexpected value needs the comparison
helper, each private plain string is relabeled with its own child's original
C-label constants first, without rereading either operand. NULL decisions and
charges retain their original order. Mixed/unlabeled operands, cast boundaries,
and all escaping text values keep the previous wrapping path; in particular an
unlabeled host String's custom comparison cannot observe a changed operand class.
This substitutes cheaper snapshots for column wrappers; it does not eliminate
all string allocation or alter the retained pool/cache limits.

Literal LIKE specialization admits an exact literal, a literal prefix followed
by `%`, a literal suffix preceded by `%`, or a literal contained between `%`
wildcards. Repeated boundary `%` characters are equivalent. The default
backslash escape is decoded during bounded admission; escaped wildcards remain
literal characters. A further extension admits fixed-position `_` characters,
optionally followed by a final `%`, and ordered literal segments separated by
`%` when there is no unescaped `_`. Mixed patterns requiring wildcard retry,
a trailing escape, ILIKE, nonliteral patterns and explicit ESCAPE expressions
retain the evaluator.
Only plain text with static C labels is admitted, including a valid UTF-8
String literal pattern. Unlabeled parameters and linguistic or nominal types
retain the existing path.

Generated LIKE code charges the operator before evaluating its operands, takes
the subject snapshot at the original wrapping point, and preserves NULL
propagation. LIKE has no comparison-length surcharge. Exact normalized runtime
strings with valid UTF-8 use fixed equality/prefix/suffix/substring operations,
a forward fixed-position matcher, or ordered literal searches;
unexpected runtime values restore the subject's label and call the unchanged
matcher without rereading an operand. No SQL pattern enters generated source or
a Ruby regexp. The original pattern and decoded data both count against the
existing string-pool limit, checked before admission allocates their copies.
Trailing-escape errors keep their original data-dependent evaluation in the
matcher; admission never raises them early.

The fixed-position matcher stores an immutable binary program of literal-byte
runs and one-code-point runs, with an optional final-any marker. Its generated
loop compares literal bytes and advances each `_` by one validated UTF-8 code
point, without token arrays, character arrays or substring allocation per row.
Interior `%` with no `_` uses monotonic byte-index searches and explicit start/end
anchors; consumed segments cannot overlap. The earliest match leaves the most
room for subsequent segments. Arbitrary pattern data never enters emitted code.

Admission limits decoded patterns to 64 runs (coalescing adjacent `%` and `_`
runs) and preserves the existing source, node and pool limits. The original
pattern plus packed program or decoded literal bytes count toward the cumulative
32 KiB string pool budget; packed-program overhead is checked before allocating
the program. SQL values, errors, costs, collation labels and runtime fallback
remain those of the unchanged handwritten matcher.

Array pipelines preserve the existing bulk scan charge before the loop and bulk
output charge after successful completion. Streaming pipelines preserve source
pull, scan-row charge, predicate, accepted-row OFFSET, projection, output charge,
sink append/suspension, then LIMIT termination in that order. OFFSET rows do not
evaluate projections. LIMIT 0 does not enter the source iterator, while scan
preparation remains at its original site. Closing a suspended cursor unwinds the
same scanner and engine cleanup scopes. Charges are never summed or hoisted across
these boundaries, including when statement/lifetime budgets are enabled.

Plain integer/boolean ordering comparisons accrue no cost and invoke no host
callbacks. Specializing them does not move key evaluation, projection, errors,
memory reservations, or spill decisions. The sort algorithm and tie-break stay
unchanged, including the external sorter's run/merge ordering. Finite cost,
query-memory budgets, and profiling therefore do not require disabling this
comparator tier. Admission never interprets SQL text as a comparator expression.

Aggregate input visits retain FILTER-before-charge-before-argument ordering.
Reduction starts only after every input has been evaluated: an earlier partial
sum overflow cannot replace a later argument error. NULL omission and removing
one-element argument wrappers do not alter the fixed aggregates' logical memory
charges. Integer sum checks each addition at i64 width with the existing error.
Boolean reductions still evaluate every input even after their result is known.
Grouping substitutions, result caching, projection, and output charges stay in
the ordinary engine. Finite statement/lifetime limits retain the same prefixes.

Materialized filtering starts only after relation materialization, access-path
selection, and scan charging. With query-memory accounting, a rejected row's
existing logical bytes are released immediately after its predicate finishes;
ownership transfer remains unchanged. Every WHERE evaluation finishes before
any grouping key is evaluated, so a later predicate error still precedes an
earlier survivor's invalid key expression. The complete key matrix finishes
before grouping, aggregate reduction, or HAVING. These loops do not move
profiling checkpoints or compile grouped projections/HAVING, where grouping-set
substitutions can replace a whole expression with NULL.

## Lifetime and bounds

Resolved plan caches retain at most 64 entries each and already validate table
publication, parameter types, type registry, environment, timezone, and collation.
Generated kernels belong to those plans and inherit their invalidation. The first
execution uses the evaluator; the second use may compile. Unsupported shapes
record a negative admission to avoid repeated emitter work. Internal diagnostic
switches can force first-use admission or disable compilation for differential
testing; neither changes public SQL or embedding APIs.

Materializing any of the five kernel families uses a process-wide 64-entry LRU
of generated classes, keyed by concrete kernel superclass and exact source.
Each materialization still constructs a fresh frozen kernel referencing its
Program's immutable pool and node metadata. The cache retains only immutable
source and a frozen generated class: no kernel, pool, AST, engine, row, parameter
binding, or execution owner.
This allows different literal pools to reuse the same Ruby method and JIT work
without changing plan admission or invalidation. Ordering Procs remain bound to
their individual kernel instances.

A mutex serializes cache lookup, compilation and publication, never execution.
The process ID is checked before acquiring the mutex; a forked child replaces
the inherited cache and lock before new materialization. Already-live inherited
kernels remain valid. Concurrent first admissions in a child may duplicate an
initial compilation before converging on the replacement cache; this cannot mix
pools or execution state. Compiler source bounds apply to cache admission; oversized
direct internal Program construction bypasses sharing. At most 64 × 32 KiB of
source is retained by this cache, in addition to existing per-plan limits. Eviction
does not promise reclamation of VM/JIT code, and live kernels can retain evicted
classes. Code churn must therefore be measured separately.

An expression, fused projection, or pipeline is limited to 192 emitted nodes,
depth 32, 256 pool entries, 32 KiB of copied string pool data, and 32 KiB source.
The string budget is checked before duplication and restored when partial root
emission is rejected. Oversized text literals retain ordinary evaluation.
An ordering kernel permits at most
192 keys and 32 KiB source, and has no value pool. A plan retains at most five kernels
(predicate, projection, sort keys, pipeline, and ordering) and 64 KiB total source. A pipeline
is attempted before separate expression kernels on eligible execution; fallback
operators may later request those kernels under the same total cap. Rejection is
cached. The internal pipeline-disable diagnostic keeps the expression tier active
for differential comparisons; the ordering-disable diagnostic likewise leaves
the expression and pipeline tiers active. Neither is a public API.
Aggregate arguments in the general and spill executors use a separate 64-entry
LRU cache. Those expressions are resolved afresh, so the exact emitted source and
immutable scalar pool form the cache key. Emission happens before lookup;
`class_eval` happens only after reuse. Unsupported argument vectors have no
compiled program, and an admitted kernel is reused within all groups of the same
resolved expression. This avoids retaining stale folded subquery values or using
an AST object ID as a persistent execution key. Primitive aggregate kernels use
their own 64-entry source-and-pool cache with the same reuse policy and per-kernel
source/node/depth/pool limits. Lazy cursors share this bounded cache with the
session; execution context is passed on each call. The internal aggregate-disable
control leaves all earlier compiler tiers enabled. Unsupported aggregates retain
their existing argument compilation and folding. Grouping substitutions remain
unchanged.
Materialized-relation methods use another 64-entry source-and-immutable-pool
cache, shared by lazy cursors with their session. The same second-use policy and
per-method limits apply. Filter memory-accounting mode is encoded in fixed
source, while the current owner, columns, mask, rows, and parameters are passed
at invocation. The internal relation-disable switch leaves earlier tiers active.
The internal text-disable switch rejects text admission across emitting tiers
while retaining integer/boolean compilation. Its separate plan/expression memo
slots prevent a warmed text method from bypassing the switch. Shared source-and-pool
LRUs keep their existing limits in both modes. Ordinary operation has one 64 KiB
source state per resolved plan; toggling this internal diagnostic may retain two
independent states, up to 128 KiB per plan.
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
