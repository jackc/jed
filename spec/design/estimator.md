# Plan-time cardinality and cost estimator — design

> The ratified Path-B contract for making cost a physical-plan input. The mechanical constants
> and total tie orders are canonical data in [../cost/estimator.toml](../cost/estimator.toml); the
> runtime unit weights remain canonical in [../cost/schedule.toml](../cost/schedule.toml). This
> document specifies the algorithms every core will implement independently.
>
> **Status: P0 contract ratified; implementation not yet landed.** The current planner still uses
> the fixed rules in [planner.md](planner.md). P1–P8 in
> [../../TODO-cost-plan-input.md](../../TODO-cost-plan-input.md) implement this contract as vertical
> slices. Until the selector slice lands, no plan, actual cost, or EXPLAIN row changes.

## 1. Decision and scope

Path B resolves [determinism.md](determinism.md) §8's plan-dependent-observable fork by
**specifying the plan**. For a fixed `(resolved query, visible database snapshot, estimator data)`,
Rust, Go, and TypeScript must:

1. inventory the same legal physical candidates;
2. compute the same integer cardinality and per-unit cost estimates;
3. compare candidates with the same total order; and
4. select the same physical plan.

Plan identity therefore remains inside G1/G2/G3 rather than becoming a class-P exception. Actual
cost stays byte-identical because the selected plan and the runtime meter are both shared contracts.
EXPLAIN makes the selected plan and estimates corpus-assertable.

Initial cost-based selection applies to **SELECT**. UPDATE and DELETE share the behavior-neutral
candidate inventory but retain their existing fixed access-path policies until a dedicated DML
slice decides mutation visitation and error-selection consequences. DDL is outside the selector.

The estimator is a planner heuristic, **not a safety gate**. An underestimate cannot weaken
`max_cost` or `lifetime_max_cost`: execution still accrues the actual units and aborts through the
runtime meter. Planning itself remains unmetered, as [cost.md](cost.md) §3 defines.

## 2. Objective: the runtime schedule, and nothing private

The selected candidate minimizes an estimate of the **same units the runtime meter charges**. Each
estimate contains:

```
Estimate {
    rows:  non-negative i64,
    units: one non-negative i64 count per schedule.toml unit, in schedule order,
    cost:  saturated Σ(units[i] × schedule[i].weight),
}
```

The unit vector is useful even though v1 EXPLAIN exposes only `rows` and the weighted `cost`: shared
fixtures can identify which event diverged rather than reporting one opaque total. The schedule's
authored order is canonical; no map iteration participates in the sum or rendering.

There are **no planner-only performance weights**. Work that the runtime schedule deliberately does
not meter — sort comparisons, DISTINCT/set membership, nested-loop control flow and row
concatenation, LIMIT slicing, buffer residency, physical I/O latency — contributes zero directly to
`est_cost`. It can still change estimated cost indirectly by changing how many scheduled events a
plan performs, for example an ordered LIMIT stopping a scan early. If an unmetered operation should
become a plan objective, add a runtime unit to `schedule.toml`, specify and re-pin its actual accrual
first, then let the estimator count it. The optimizer cannot grow a shadow wall-clock schedule.

## 3. Representation and exact arithmetic

All public estimate values lie in `0..MAX_ESTIMATE`, where
`MAX_ESTIMATE = 9_223_372_036_854_775_807` (`i64::MAX`). Rust and Go carry `i64`; TypeScript computes
with `bigint` and clamps to the same maximum. Floats are forbidden in estimator arithmetic.

The primitive operations are:

- `sat_add(a, b) = min(MAX_ESTIMATE, a + b)`;
- `sat_mul(a, b) = min(MAX_ESTIMATE, a × b)`;
- `scale_ceil(n, p/q)`, for reduced `0 ≤ p ≤ q`, equals `ceil(n × p / q)` with saturation.

`scale_ceil` is implemented by quotient/remainder so Rust/Go need no wider integer:

```
whole = sat_mul(n / q, p)
tail  = ceil((n % q) × p / q)   # both factors are < q; checked + saturated
result = sat_add(whole, tail)
```

`estimator.toml` caps authored selectivity denominators at `1_000_000`, so the tail product is
strictly below `10^12` and fits i64. An implementation may use a wider temporary where its language
supplies one, but the result and overflow boundary above are authoritative. A positive fraction over
a non-empty input estimates at least one row. Only a structural proof — an empty table, constant
FALSE/NULL, contradictory bound, NULL equality endpoint, or an impossible unique lookup — produces
zero directly.

Subtraction is clamped at zero. Counts render as shortest unsigned decimal even though their SQL
type is signed `i64`. Every fold has a specified order below; algebraic reassociation is forbidden
because intermediate ceiling and saturation are observable.

## 4. Canonical data

[../cost/estimator.toml](../cost/estimator.toml) schema version 1 owns:

- `MAX_ESTIMATE`, ceiling rounding, and the generic-before-bind parameter strategy;
- PostgreSQL-derived no-statistics selectivities expressed as exact rationals;
- deterministic fallbacks for boolean and opaque predicates;
- the default distinct count, SRF cardinality, and variable-width hash-key work;
- the eight-relation dynamic-programming limit and larger-island strategy; and
- access-path and join-algorithm tie orders.

`rake verify` runs `estimator_verify.rb` to reject missing/duplicate facts, unreduced or invalid
fractions, changed approved defaults, and incomplete tie orders. P4 adds generated constant tables
and the shared fixture matrix at `spec/cost/estimator_vectors.toml`; the verifier will validate its
input, per-unit-count, row, cost, and tie-key fields then. Codegen may copy mechanical facts into
each core; it must never generate candidate enumeration, selectivity traversal, cost propagation,
or join search.

The initial selectivities are:

| structural class | fraction | source/rationale |
|---|---:|---|
| equality | `1/200` | PostgreSQL `DEFAULT_EQ_SEL` |
| one-sided inequality | `1/3` | PostgreSQL `DEFAULT_INEQ_SEL` |
| paired lower+upper range | `1/200` | PostgreSQL `DEFAULT_RANGE_INEQ_SEL` |
| `IS NULL` | `1/200` | PostgreSQL `DEFAULT_UNK_SEL` |
| LIKE/regex-style match | `1/200` | PostgreSQL `DEFAULT_MATCH_SEL` |
| other matching operator | `1/100` | PostgreSQL `DEFAULT_MATCHING_SEL` |
| bare boolean expression | `1/2` | jed fallback |
| unsupported/opaque predicate | `1/3` | jed conservative fallback |

These are planner facts, not claims about real data. P9 replaces a default with statistics only
where that replacement is itself spec'd; fallback values remain stable.

## 5. Inputs and the no-planning-I/O rule

An estimate is a pure function of:

- the resolved logical plan and its source-order expression trees;
- catalog facts already resolved for planning: types, keys, indexes, collations, partial-index
  predicates, access-method metadata, and relation kind;
- transactional statistics visible in the query's snapshot, beginning with exact table row count;
- storage-structure facts available from the resident skeleton without faulting a leaf: node count,
  leaf count, height, index equivalents, and literal-bound overlap counts; and
- the shared estimator and runtime cost data.

It must not depend on buffer-cache residency, whether a page is physically fetched, allocation,
wall-clock time, host iteration order, map layout, a filesystem probe, or a planning-time scan of
table/index leaves. Planning may inspect already-resident interior separator bytes to compute a
literal bound's overlapping node count; it may not fault a leaf to count matching rows or large
values.

Statistics are snapshot state. Pending mutations change the writer's working statistics and
rollback discards them; readers keep the statistics pinned with their data snapshot. Since v28, the
exact row count is persisted in the table catalog, so reopen does not restore the former full-leaf
count walk. Future NDV/histogram collection follows the same transactional rule.

Every relation also owns a transactional **estimator revision** used only for cache validity. A
successful statement that may change any admitted estimator input advances that relation's revision
once in the working snapshot; rollback restores it. It need not be persisted because a reopened
database has a new cache identity. Conservatively advancing after any successful row mutation is
valid; failing to advance after an input change is not.

## 6. Parameters, literals, and prepared-plan caching

The pipeline remains `resolve → optimize → bind`. A literal is available to planning and may select
a structural class or, once P9 lands, a histogram bucket. `$N` is not bound at physical-selection
time and always receives the generic selectivity for its predicate class. Initial Path B has no
custom parameter-sensitive plans.

A prepared-plan cache entry is reusable only when an exact, collision-free, relation-scoped input
signature still matches. For every referenced persistent relation, in source ordinal order, the
signature contains:

```
(database identity, catalog generation, lowercased table name, estimator revision)
```

The database identity is an equality token, never a value read by the estimator or EXPLAIN. An
attachment contributes its own identity/generation/revision, so the main database's generation
cannot validate a plan against a changed attachment. Temp, SRF, CTE, and derived-relation plans keep
their existing uncacheable status until their cache identity is separately specified.

This is a relation-scoped invalidation contract: a write to an unrelated table does not evict a
prepared point lookup. A relevant revision change forces re-planning even if row count happens to
return to its prior value, because structure or future distribution statistics may differ. Cache
hits must be plan-, estimate-, result-, and actual-cost-identical to fresh planning over the same
snapshot.

## 7. Base cardinality

Let `N` be a base relation's exact row count.

- No predicate: `N`.
- Constant TRUE: `N`; constant FALSE or NULL: `0`.
- Full unique/primary-key equality: `min(N, 1)`; a literal NULL equality endpoint: `0`.
- Other equality: `scale_ceil(N, equality)`.
- `IN (v1, …, vk)`: apply §7.1's OR rule to equality entries in authored order. A literal NULL
  contributes zero true rows. Canonically equal resolved literals are de-duplicated with the first
  occurrence retained; parameters are never assumed equal to each other or a literal. `NOT IN`
  uses the NOT rule, except that a known literal NULL anywhere in the list proves zero true rows.
- `<>`: `N - equality_rows`.
- One-sided `<`, `<=`, `>`, or `>=`: `scale_ceil(N, inequality)`.
- A lower and upper comparison on the same resolved operand in one AND chain:
  `scale_ceil(N, paired_range)`; the pair is consumed once, not as two inequalities.
- Positive `BETWEEN` is that paired-range rule. `NOT BETWEEN` is the §7.1 OR of its two
  one-sided inequalities. A known literal NULL bound proves zero true rows for positive BETWEEN;
  for NOT BETWEEN the remaining non-NULL bound contributes its one outward inequality, or zero if
  both bounds are NULL.
- A structurally contradictory range or conflicting equality: `0`.
- `IS NULL`: `scale_ceil(N, null_test)`; `IS NOT NULL`: its complement.
- LIKE/ILIKE/regex/pattern class: the `match` or `matching` fact assigned by the operator catalog's
  future estimator classification; absent a classification, `opaque`.
- A bare boolean column/expression: `boolean`.
- An otherwise supported predicate with no specialized rule: `opaque`.

An access candidate has two row counts:

1. **scan rows** — rows admitted/fetched by its access predicate; and
2. **output rows** — scan rows after residual predicate selectivity.

Conjuncts consumed into an access bound are not applied a second time to output cardinality, but
their expression nodes are still counted for runtime `operator_eval` because execution always
rechecks the full WHERE. A known-superset access method uses its access-method selectivity for scan
rows and the complete unproved portion as residual.

### 7.1 Boolean composition

Top-level AND/OR nodes flatten in source order. Before ordinary AND folding, the estimator performs
the same deterministic contradiction and paired-range inventory the access detector uses.

- **AND:** begin with `N`; apply each remaining conjunct's exact fraction to the current row count,
  left to right with `scale_ceil`.
- **OR:** estimate every disjunct against the original `N`, add the parts in source order with
  saturation, and cap at `N`. This disjoint-union upper estimate is deliberately simple until P9
  supplies overlap/NDV facts. `N = 0` yields zero.
- **NOT:** `N - estimate(predicate, N)`.

The formulas approximate SQL 3VL when NULL-distribution facts are absent; they never change SQL
evaluation. P9 may replace them only with spec'd NULL/NDV statistics. Syntactically equivalent but
differently associated predicates may estimate differently because source-order ceiling is part of
the plan contract; the chosen plan still returns identical rows.

### 7.2 Other row-producing nodes

- A FROM-less `Result`: one virtual row before WHERE; the ordinary predicate may reduce it to zero.
- Literal `VALUES`: exact authored row count.
- `generate_series` with plan-time literal bounds/step: exact checked cardinality; other SRFs and
  non-literal bounds: `default_srf_rows`.
- Built-in catalog relations: the exact count of matching resident catalog entries in the visible
  database/attachment snapshot; they require no storage leaf reads.
- Derived table / scalar subquery / CTE body: its recursively estimated output.
- Materialized CTE reference: the body's rows; its scan work is counted separately (§8).
- CROSS JOIN: `sat_mul(left_rows, right_rows)`.
- INNER equality join without NDV facts: equality selectivity over the saturated product; multiple
  equality keys apply left to right. Other ON predicates use their structural fractions.
- LEFT/RIGHT outer join: at least the preserved side's rows. FULL JOIN: at least the larger input.
  Until cost-based outer reordering exists these are EXPLAIN estimates, never reorder permission.
- Aggregate without GROUP BY: exactly one row, including empty input.
- GROUP BY with `K` key expressions: `min(input_rows, sat_pow(default_distinct_values, K))`; an
  empty input yields zero groups.
- DISTINCT over `K` projected expressions: the same group-count rule, capped by input rows.
- Sort and Window preserve input row count.
- OFFSET `O`: `max(0, input_rows - O)`. LIMIT `L`: `min(input_rows, L)` after OFFSET. A non-literal
  LIMIT/OFFSET, if later admitted, uses the unchanged input estimate.
- UNION ALL: saturated sum. DISTINCT set operations: capped by the saturated sum and estimated with
  the DISTINCT rule over unified output columns. INTERSECT is at most the smaller side; EXCEPT at
  most the left side, with `opaque` selectivity until statistics give a stronger rule.

`sat_pow` multiplies left to right with `sat_mul`; zero keys are handled by each node's semantic rule,
not by the generic power.

## 8. Estimating runtime units

The estimator counts only events in `schedule.toml`. A unit not reachable in a SELECT candidate is
zero. A value-dependent extra with no admitted statistic is also zero rather than a made-up private
weight; the owning base unit still counts the operation. Runtime metering remains authoritative.

### 8.1 Scans and access paths

- Full table/index scan `page_read` is the exact resident-skeleton node count.
- A literal contiguous bound may use the exact skeleton overlap count without leaf I/O.
- A generic/parameter bound applies the access predicate's same ordered selectivity program to
  `node_count`, then clamps a non-empty result to `[height, node_count]`; an empty row estimate is
  zero. This avoids a second rounded `estimated_rows / table_rows` division.
- A table point fetch after a secondary-index hit estimates `table_height` pages per fetched row.
- `storage_row_read` equals the candidate rows fetched from table storage, before residual filter.
- Overflow-chain `page_read` and `value_decompress` are zero until persisted size statistics exist;
  planning never scans leaves to discover them.
- GIN `gin_entry` defaults to the estimated candidate/posting rows; GiST `gist_descent` uses the
  admitted resident structural descent count where available, otherwise the estimated non-leaf
  pages of its bound. Both retain their ordinary table-fetch and residual costs.
- Interval sets sum disjoint interval page estimates in canonical interval order with saturation.
  A LIMIT-aware candidate estimates only intervals it expects to start, in execution order.

### 8.2 Expressions and size-dependent work

`operator_eval` counts each interior resolved expression node for each estimated invocation,
following [cost.md](cost.md)'s pre-order, eager-binary, CASE, and COALESCE rules. An access predicate
still executes as residual and therefore retains its expression charges. Projection/order
expressions use the row count at their real pipeline stage; final projection is after LIMIT/OFFSET.

The following rules cover additional units:

- fixed structural calls such as `timezone` and `sequence_advance` count once per estimated
  invocation when the corresponding built-in runs;
- `decimal_work`, `collate`, `varlen_compare`, and regex size extras are zero without admitted
  value-size facts; constant operands may compute their exact structural work;
- constant regex compilation may count its exact compiled-program work; a parameter pattern uses
  no extra until statistics/model data define one;
- `hash_build`/`hash_probe` use exact encoded bytes for fixed-width keys and
  `default_variable_key_bytes` for a variable-width or unknown key, never less than the runtime
  minimum of one per inspected key; estimated bucket verification uses estimated join candidates;
- `aggregate_accumulate` is `input_rows × aggregate_count`;
- `cte_scan_row` is materialized rows per reference;
- `generated_row` is the SRF cardinality;
- `window_result` is output rows per window function; `window_frame_step` follows the spec'd frame
  algorithm over estimated partition rows where derivable, otherwise zero;
- `constraint_check` and `value_compress` are zero in initial SELECT selection.

Join candidates count their different repetition shapes explicitly:

- the current materializing nested loop scans each base input once, evaluates its ON expression for
  every saturated Cartesian candidate pair, and emits the §7.2 join-row estimate; unmetered loop
  control and row concatenation add nothing;
- an index nested loop multiplies the inner access candidate's structural reads, access-method
  units, fetched rows, and residual/ON invocations by outer rows, in outer-row order; and
- a hash join scans each input once, charges `hash_build` for build rows/bytes and `hash_probe` for
  probe rows/bytes, then reapplies and charges the full ON expression for estimated
  bucket-verification candidates.

An uncorrelated scalar, EXISTS, or IN subquery contributes its subplan once. A correlated subquery
contributes its subplan and owning predicate work once per estimated invoking outer row, using
saturated multiplication. EXISTS reduces its child pull to at most one row when the physical plan
can short-circuit; IN uses the materialized subquery rows and the current source-order membership
evaluation contract. CTE bodies/references follow the same once-versus-per-reference distinction.

### 8.3 Per-node and statement totals

For EXPLAIN, a node's `est_rows` is the rows it emits to its rendered parent. Its `est_cost` is the
saturated cumulative cost of the rendered subtree through that node. A SELECT's outermost rendered
node additionally owns the top-level projection and final `row_produced` units because the v1 plan
tree has no separate Project node. A lone Scan/Result is outermost when no wrapper exists.

LIMIT/ordered-stream short-circuit is a physical-plan property: its child scan estimate is reduced
to the rows/pages expected to be pulled, not estimated eagerly then sliced only at the Limit node.
Consequently estimates are computed over a complete candidate pipeline rather than by blindly
adding immutable logical-node estimates.

## 9. Candidate total order

Candidate comparison never stops at numeric cost. The total order is:

1. lower saturated `est_cost`;
2. lexicographically smaller physical relation sequence by original FROM ordinal;
3. per relation, the access-path rank from `estimator.toml`;
4. within an index-bearing path, lowercased index name by raw UTF-8 bytes;
5. per join step, the join-algorithm rank from `estimator.toml`; and
6. remaining physical booleans/ordinals in the fixed field order specified where that candidate is
   introduced.

`est_rows` is not a separate tie-break: scheduled downstream row work should already affect cost;
where the schedule deliberately ignores it, the structural order wins. The initial ranks preserve
today's choices on exact ties:

```
access: PK < B-tree < GiST < GIN < PK interval < index interval < full
join:   index nested loop < hash < nested loop
```

A future access method or join algorithm is ineligible for cost selection until this data and the
candidate's final field order are extended, verified, and implemented in all cores.

## 10. Join search and its deterministic bound

Only left-deep orders are in Path B. INNER/CROSS relations form reorderable islands; outer joins,
LATERAL, correlation, and other dependency-bearing nodes are barriers. P7 introduces two-relation
orientation; P8 generalizes the island search.

For an island of at most `join_dp_limit = 8` relations, enumerate left-deep candidates by exhaustive
dynamic programming over source-ordinal subsets. State keys, retained physical properties, and
iteration order are fixed by the P8 spec; unordered containers may store states but may never decide
enumeration or winners. Exact ties use §9.

For a larger island:

1. choose the cheapest one-relation driver candidate;
2. repeatedly append the not-yet-chosen relation plus legal access/join algorithm producing the
   cheapest new cumulative plan; and
3. break every tie by §9.

This deterministic cheapest-next fallback is polynomial and uses no random/GEQO path. Eight follows
PostgreSQL's default join-collapse boundary; the overriding reason for not adopting GEQO beyond it
is cross-core identity and reproducibility.

## 11. EXPLAIN contract

When P5 lands, EXPLAIN appends two non-NULL `i64` columns to its existing result:

| column | meaning |
|---|---|
| `est_rows` | rows emitted by this node to its rendered parent (§8.3) |
| `est_cost` | saturated cumulative scheduled cost through this node (§8.3) |

Fallback rules make both values available for every node, so NULL is unnecessary. Existing
`depth`/`node`/`detail` spelling and pre-order row order stay unchanged. EXPLAIN ANALYZE's
`cost=<actual> rows=<actual>` root remains actual runtime data; estimated columns remain estimates.
The EXPLAIN statement itself still charges one actual `row_produced` per rendered row — adding two
cells changes no render cost.

Plain EXPLAIN is jed-owned and not PostgreSQL-oracle imported. Its estimate rows are asserted
`nosort` in the shared corpus, making arithmetic, candidate choice, and tie breaks one cross-core
contract.

## 12. Conformance and slice gates

- **P0:** data coherence only (`rake verify`); no engine behavior changes.
- **P1/P2:** byte goldens, transactional statistics, reopen, and cache fresh-vs-hit parity.
- **P3/P4:** shared candidate/estimator vectors; legacy selector keeps all corpus outputs unchanged.
- **P5:** EXPLAIN estimate columns become the differential assertion surface.
- **P6–P8:** each enabled choice carries EXPLAIN cases, actual `# cost:` re-pins, a new NoREC
  relation, and affected benchmarks.

PostgreSQL remains the result oracle, not the plan/estimate oracle. The borrowed default
selectivities are recorded data, not a promise to reproduce PostgreSQL plans.

## 13. Deliberate boundaries and deferred work

- No parameter-sensitive/custom plans in initial Path B.
- No cost-based DML access policy until a mutation-specific slice.
- No NDV, MCV, histogram, or value-size statistics until P9.
- No planner-only wall-clock cost model.
- No bushy join trees, GEQO/random search, parallel-plan search, or adaptive runtime re-planning.
- No planning-time leaf reads or statistics sampling.
- No claim that `est_cost == actual cost`; equality is expected only for fully known simple shapes.

These are contained doors, not accidental omissions. Each can land only by extending the shared
data/algorithm contract and its cross-core fixtures first.
