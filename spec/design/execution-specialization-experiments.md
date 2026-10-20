# Independent execution-specialization experiments — proposed contract

> **Proposed experiments, not a production rollout.** Written 2026-10-19. The
> maintainer requested separate investigations for Rust, Go and TypeScript.
> Each can run and reach a decision independently. No new dependency, SQL/API
> behavior, file format, planner policy or semantic cost schedule is selected.

## 1. Independent proposals

| Core | Proposal | First hypothesis |
|---|---|---|
| Rust | [Cached typed expression programs](rust-expression-specialization.md) | Retain the earlier VM's scan benefit while removing repeated lowering and point-query penalties. |
| Go | [Static execution kernels](go-execution-specialization.md) | Resolve common operation shapes once and reduce calls/value movement without another general dispatch loop. |
| TypeScript | [CSP-compatible expression specialization](ts-expression-specialization.md) | Closures and static kernels capture useful specialization without runtime source generation. |

The [investigation](query-compilation-investigation.md) records prior evidence,
including the separate Ruby and expression-VM branches. Ruby is background for
these proposals; its experimental compiler and possible merge are separate work.
The TS proposal does not depend on the proposed [async runtime](ts-runtime.md).

Each branch changes its own core and adds reusable language-neutral tests where
needed. Other cores remain controls and run the shared SQL corpus normally.
There is no requirement to port Rust's program representation to Go or TS, or to
wait for all three experiments to finish. Passing an experiment does not select
a production default. Later adoption is a separate reviewed change.

## 2. Behavioral boundary

The canonical contracts remain [determinism.md](determinism.md),
[estimator.md](estimator.md), [cost.md](cost.md), [memory.md](memory.md),
[streaming.md](streaming.md) and [api.md](api.md). Implementation diversity is
useful only within those contracts:

| Implementation choice | Required invariant |
|---|---|
| Representation, calls, registers, fusion | Same resolved meaning and chosen physical plan, including estimates and ties. |
| Unboxing, borrowing, fewer physical allocations | Same result values/types/metadata and logical memory reserve/release behavior. |
| Earlier selection of an operation | Same operand visitation, lazy branches, deferred reads, traps and error precedence. |
| Lowering and caching | Same binding, privileges, environment and schema validity on every execution. |
| Cost implementation | Same accrual and guard checkpoints, including zero-charge guards and statement/lifetime precedence. |
| Cursor integration | Same pulls, LIMIT/OFFSET behavior, output ownership, cleanup and existing cancellation checkpoints. |

Lower only after contextual typing and physical selection. No new constant
folding, common-subexpression elimination, predicate reordering or changed
aggregate fold order. Runtime specialization never influences planner weights.
Bulk charges are allowed only at boundaries already permitted by the contract.
Finite budgets must not silently acquire a new fast-path cost schedule.

Admission is side-effect-free: it reads no pages, invokes no host functions, and
does not evaluate an expression to discover its value. SQL errors stay at their
original execution point. Reject unsupported roots before evaluation; never
catch an optimized-path query error and retry through the ordinary evaluator.
An operator may use an optimized predicate and ordinary projection, but it must
choose each complete root before execution and preserve their ordering.

## 3. Common initial expression surface

Initial experiments target read-only, nonblocking single-base-table SELECT
scans, with the existing row source, residual filter and projection boundaries.
Plain signed integers and booleans, nullable or nonnullable, are the first types.
Admission must prove the type of every leaf and intermediate, not infer input
types from an integer-valued result or from the first observed row.

The coverage inventory includes column/parameter/scalar-constant reads, checked
integer arithmetic/comparisons, NULL tests, value AND/OR/NOT, filter AND, searched
CASE, COALESCE and integer casts. A core may begin with a smaller declared subset;
unimplemented forms are explicit negative controls. Division/remainder and
narrowing casts enter only with their overflow and error-order fixtures.

Exclude decimal, floats, text/collation, nominal/open types, host calls, volatile
operations, subqueries/correlation, grouped substitutions, windows and DML in the
first slice. Preserve existing encoded-span/columnar paths instead of routing
their work through a new evaluator. Unsupported descendants reject the complete
root. Exclusions are performance eligibility choices, never SQL errors.

## 4. Ownership and bounded preparation

Prepared artifacts belong to a valid resolved plan or its owning cache entry.
Reuse the full existing validity checks, including relation-scoped identities,
catalog/estimator revisions, parameter typing/custom-plan selection and relevant
environment/type context. Never introduce a cache keyed only by SQL text, object
address, AST identity, or a lossy hash. Cache entries retain no execution context,
transaction, current binding, row source, meter or result owner.

Immutable artifacts may be shared where the current API already permits it;
scratch belongs to each active execution/cursor. Reentrancy must receive separate
scratch. Do not change Rust thread affinity or Go's prepared-statement sharing
contract to simplify a prototype. An active cursor may retain its valid pinned
plan after the prepared slot is replaced. Test close/error/cancellation release.

The proposed structural caps are data in
[`limits.toml`](../experiments/execution-specialization/limits.toml). A limit
selects ordinary execution; it does not reject otherwise valid SQL. Check limits
incrementally before allocating the rejected representation. Count all retained
specialization variants together, not once per mode. Cache a bounded rejection
marker within the plan so unsupported roots are not reconsidered per row.

These caps bound added structure per plan/execution, not process heap. There is
no global artifact cache. Existing application-owned statements/cursors may be
numerous; report retained source trees, code/descriptors, scratch and allocator
overhead separately. Any additional buffer proportional to input rows is outside
the first slice. Compiler work is bounded preparation, outside the existing SQL
execution-cost counter. This proposal does not add private cost units or claim
that `max_query_memory_bytes` caps compiler/runtime memory. Production adoption
must explicitly account for new owners and any remaining coverage gap.

## 5. Verification without vacuous passes

Each experiment provides harness-only `ordinary`, `forced` and `selective` modes.
Names and CLI plumbing may differ. No mode is a SQL setting or public API, and
no evaluator reads process environment or wall-clock time. `forced` attempts
admission on the first execution of eligible corpus shapes; it never bypasses
type proofs, limits or semantic fences. `selective` exercises proposed shipping
admission/cache behavior. Record why each root was admitted, excluded or reused.

Add ordinary SQL behavior to the shared corpus, with PostgreSQL oracle checks
where comparable. Reuse existing cost/type/plan fixtures and add missing ones.
Keep admission counters, scratch ownership, cancellation injection, byte fixtures
and host/cache introspection in per-core tests when the corpus cannot express
them. Any optimization slice also adds an appropriate shared NoREC/metamorphic
relation. A local evaluator is a differential control, never a new reference
implementation; settle disagreements against spec, corpus and the PG oracle.

Required cases, on the admitted surface:

- NULL at each operand, eager right-hand errors despite a NULL left operand,
  filter AND versus value AND, and unselected CASE/COALESCE branches that trap.
- Integer width boundaries, negative division/remainder, minimum integer / -1,
  narrowing failures, and two competing errors whose visitation is specified.
- Every statement/lifetime budget prefix for small expressions, nonzero lifetime
  starting totals, tied limits, and finite query-memory cost/error precedence.
- Changed bindings/types, DDL and temp shadowing, relevant/unrelated statistics
  changes, another database/attachment/environment, and transactional replanning.
- LIMIT 0, OFFSET without projection, partial drain, early close, cursor reuse
  isolation, and prescribed cancellation/pull checkpoints.
- Deferred-read faults and row-access order where the chosen integration can
  encounter them; do not claim this coverage from in-memory integer arrays.
- Positive/negative admission, every structural cap, deepest accepted compiler
  inputs, and existing parser/native-stack limits with the experiment enabled.

Run the full shared corpus in memory and disk with forced and selective modes,
plus ordinary baseline controls, relevant query-memory/spill suites, and host
tests. Report optimized-root invocation counts for named target fixtures so a
successful fallback-only run cannot pass the coverage gate. Collect diagnostics
outside timing, or prove their equal overhead in the timed variants. Preserve
the declared cancellation differences between languages; injection tests should
compare checkpoint behavior, not promise identical wall-clock cancellation rows.

## 6. Performance protocol and decision

Use a clean production baseline and a same-experimental-build ordinary control.
The latter isolates execution choice but cannot reveal larger objects, disabled
hooks or setup overhead introduced by the experiment. Record source hashes,
compiler/runtime/build options, hardware, storage mode and exact commands.
Verify and fetch prior experiment commits before reuse; do not silently make a
moving sibling branch part of the baseline.

Each investigation measures complete prepared and ad-hoc queries, with result
drain/checksum and exact cost validation. Common workload families are arithmetic
projection, selective boolean filtering, CASE/COALESCE, finite-cost scans,
prepared hit/miss point lookups, scalar queries, unsupported expensive functions,
existing columnar aggregates, LIMIT/OFFSET, cache invalidation and statement churn.
Use memory and warm file hosts, small and substantial scans, and parameter
changes. This does not establish cold-I/O or write performance.

Separate preparation, first call, warmup and steady state. Record artifact/cache
hits, compile counts, allocations/retained memory where measurable, and churn
after cursors/statements are released. Microbenchmarks explain a mechanism; they
do not substitute for complete queries. Run timing workers sequentially, alternate
baseline/candidate order, retain all samples and lengthen tiny controls. Use at
least five fresh process pairs for a decision; CPU affinity is useful where
available but is not isolation from other workloads.

Before timing, name the target workloads and an acceptable regression budget.
Suggested initial decision thresholds: at least 10% lower complete-query median
time on two distinct target shapes, consistent direction in at least four of
five process pairs; investigate any reproducible >3% slowdown in point/ad-hoc or
unsupported/columnar controls. These are proposed research criteria, not SQL
contracts or statistical guarantees. Report ranges; a noisy or inconclusive
control does not count as evidence of no regression. Narrowing eligibility after
seeing a regression requires a fresh declared validation/measurement run.

Correctness and bounded ownership are unconditional gates. Acceptable outcomes
are proceed with a narrow adoption proposal, retain research with named missing
evidence, or reject the approach. A negative result in one language does not
block the others. Deliver the source, shared fixtures, commands, raw results,
coverage/retention evidence and a written decision. Full production adoption
requires green repository CI and any required documentation/API work; the
experiment itself changes no standing architectural brief.
