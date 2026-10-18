# Expression VM experiment

Status: opt-in experiment; the production evaluator remains the recursive tree walker.
No SQL behavior, public embedding API, storage format, or semantic cost schedule changes.

## Question and scope

Does flattening resolved scalar expressions improve end-to-end execution enough to justify
another lowering pass in the three independent cores? This tests the expression portion of
the VM idea left open by [streaming.md §3](streaming.md), not a statement VM or native-code JIT.
The existing relational operators, planner choices, encoded-span predicates, columnar paths,
and cursor/storage lifetimes remain in charge.

The initial admitted surface is integer and boolean expressions: columns, bound parameters,
constants, integer arithmetic and comparisons, NULL tests, boolean connectives, CASE and
COALESCE. Integer widths and overflow semantics are resolved before execution. Existing value
kernels remain authoritative for each core's operations. Unsupported expressions fall back
as a whole; an unsupported subtree must not be evaluated speculatively or partially lowered.
More complex values/functions are controls, not a claim of full evaluator coverage.

## Execution contract

Each core independently lowers into a flat instruction array and interprets it. Physical
layouts and dispatch strategies are implementation choices, not persisted bytes or a public
ABI. Programs are immutable; temporary registers belong to an execution. A cache may retain
only valid source expressions and must not confuse reused addresses or filter/value contexts.
Experimental process caches are reset explicitly and are not a production cache design.

The semantic operations are:

| Operation | Obligation |
|---|---|
| Enter | Perform the original node's guard, then its original semantic charge, before children. Leaf guards still occur. |
| Load | Read the same column/parameter/constant and preserve deferred-value handling. |
| Unary / binary | Apply the resolved kernel; preserve any post-operand guards and checked integer width. |
| Branch / merge | Follow original left-to-right evaluation, with SQL three-valued boolean logic. |
| Filter AND | Stop on FALSE **or NULL**, as the existing filter evaluator does. Value AND stops early only on FALSE. |
| CASE / COALESCE | Evaluate only the selected arm or the required prefix, respectively. |
| Return | Return the same typed value or filter decision; release transient scratch appropriately. |

Instruction dispatch is not charged. Cost means the same semantic work as the existing tree
walker, including its exact abort boundaries. Source error positions/details remain intact.
No operation moves volatile calls, clock/entropy access, or allocation checks across a branch.
The VM does not relax parsing/planning depth limits or introduce host access into built-ins.

## Shared experiment inputs

`spec/experiments/expression-vm/cases.tsv` defines direct expression comparisons using the
schema and rows in `rows.tsv`: `t(id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)`.
The expression parameters are always `[7::i32, 3::i32]`. Each case runs as both engines over
each row, including ceilings from zero through the successful cost plus two, comparing values,
errors and accrued costs. Filter cases compare the predicate decision, not its nullable value.
These are internal evaluator equivalence tests, not replacements for the SQL corpus or PG.

`workloads.tsv` contains end-to-end SQL controls and targets. Each driver creates `t` with
20,000 rows: id=1..20000, a=id%97, b=id%13+1, p=(id%2=0), q=(id%3=0). Every 17th a,
19th b, 23rd p, and 29th q is NULL. Setup and warmup are excluded from timing.
Columns are separated by literal tabs: name, execution (`prepared` or `adhoc`), iterations,
SQL. Prepared queries are prepared once; ad-hoc queries include parsing/planning on each call.
Every timed query is fully drained. Checksums consume every value outside or inside timing
consistently in both modes. Cost and checksum must match. A metered scan control installs a
nonbinding cost ceiling; no speculative batching may change deterministic abort behavior.

Run cores sequentially, with independent processes and alternating tree/VM order. Report
paired process medians, not the fastest sample. Report preparation/compilation overhead,
program/cache footprint and allocations where measurable, and state where instrumentation or
coverage differs across cores. Do not compare absolute times across languages as a VM effect.

## Decision gate

Correctness is mandatory: shared SQL corpus, internal differential checks including budget
boundaries, and existing resource/stack tests. Performance adoption requires material gains
on real expression-heavy queries without meaningful regressions in point lookup, ad-hoc,
columnar and expensive-function controls. Microbenchmarks alone do not justify adoption.
No new dependency is needed. Results and a decision will be recorded here after measurement.
