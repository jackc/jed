# Plan-time cardinality and cost estimator — design

> The ratified Path-B contract for making cost a physical-plan input. The mechanical constants
> and total tie orders are canonical data in [../cost/estimator.toml](../cost/estimator.toml); the
> runtime unit weights remain canonical in [../cost/schedule.toml](../cost/schedule.toml). This
> document specifies the algorithms every core implements independently.
>
> **Status: implemented in all three native cores.** The landed contract includes exact row counts,
> relation-scoped prepared-plan validity, complete candidate inventory, base and whole-plan
> estimates, EXPLAIN columns, complete single-relation pipeline selection, hard-fenced N-way join
> search with Pareto-frontier DP through eight movable relations and deterministic cheapest-next
> construction above the cap, and the retained transactional `ANALYZE` facts specified in
> [statistics.md](statistics.md).

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
fractions, changed approved defaults, incomplete tie orders, and incoherent estimator vectors. The
generated constant tables and shared fixture matrix live at `spec/cost/estimator_vectors.toml`; the
verifier validates its input, per-unit-count, row, cost, and tie-key fields. Codegen may copy
mechanical facts into each core; it must never generate candidate enumeration, selectivity
traversal, cost propagation, or join search.

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

These are planner facts, not claims about real data. Collected statistics replace a default only
under [statistics.md](statistics.md)'s exact applicability rules; fallback values remain stable.

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
count walk. Since v29, explicit `ANALYZE` persists per-column facts; later DML retains them, marks
them stale, and rescales them deterministically against the current exact row count
([statistics.md](statistics.md) §2/§5).

Every relation also owns a transactional **estimator revision** used only for cache validity. A
successful statement that may change any admitted estimator input advances that relation's revision
once in the working snapshot; rollback restores it. It need not be persisted because a reopened
database has a new cache identity. Conservatively advancing after any successful row mutation is
valid; failing to advance after an input change is not.

Database identity and estimator revision are opaque, non-persisted equality tokens,
not counters or hashes. A snapshot clone shares the tokens; the first successful row mutation of a
relation in a statement replaces that relation's working token exactly once. Commit publishes the
replacement and rollback discards it. A fresh create/open/attachment receives a fresh database token,
so detach/reattach and reopen cannot alias an old cache entry. The tokens are cache metadata only:
they are never estimator arithmetic inputs, serialized bytes, EXPLAIN values, or runtime cost.

## 6. Parameters, literals, and prepared-plan caching

The pipeline remains `resolve → optimize → bind`. A literal is available to planning and may select
a structural class or a histogram bucket. `$N` is not bound at physical-selection time and always
receives a generic estimate: statistics-aware NDV average density for eligible equality, otherwise
the generic structural selectivity. It never receives an MCV/histogram bucket. Initial Path B has no
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

The signature is stored and compared field-for-field; it is not compressed into a hash. Catalog
generation is currently database-scoped, so unrelated DDL within the same database remains a safe,
conservative invalidation. Row/statistics invalidation is relation-scoped: an unrelated table's
revision change does not invalidate the entry. A cache entry may be filled only from committed state;
a pre-existing committed entry may be consulted inside a transaction, but working revisions can only
cause a miss and never replace that committed entry.

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

The two counts deliberately have different bases. **Output rows are estimated once from the complete
WHERE against the base relation's `N`, independently of the physical candidate.** Scan rows are
estimated from the candidate's access predicate. The executor still rechecks the complete WHERE for
every fetched row, so `operator_eval` uses scan rows even though logical output cardinality does not
apply the predicate a second time. In particular, a lossy/superset GIN or GiST bound does not square
the same selectivity merely because it has a residual recheck: current statistics do not collect
access-method false-positive statistics, and inventing a second reduction would make one logical
predicate's output cardinality depend on its physical path. This separation is deliberate.

The initial access-method classifications are canonical data in `estimator.toml`: scalar GiST `=`
uses `equality`; range GiST and every GIN strategy use `matching`. Ordered/PK bounds derive their
program from their equality prefix, optional trailing range, and interval disjunction. Unsupported
access predicates use `opaque`.

### 7.1 Boolean composition

Top-level AND/OR nodes flatten in source order. Before ordinary AND folding, the estimator performs
the same deterministic contradiction and paired-range inventory the access detector uses.

- **AND:** begin with `N`; apply each remaining conjunct's exact fraction to the current row count,
  left to right with `scale_ceil`.
- **OR:** estimate every disjunct against the original `N`, add the parts in source order with
  saturation, and cap at `N`. This disjoint-union upper estimate deliberately has no overlap model.
  Column statistics refine individual disjuncts but retain this upper fold.
  `N = 0` yields zero.
- **NOT:** `N - estimate(predicate, N)`.

The formulas approximate SQL 3VL when NULL-distribution facts are absent; they never change SQL
evaluation. An exact NULL fraction makes supported column complements subtract from non-NULL rows
under statistics.md §6. Syntactically equivalent but
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
- A recursive CTE's self-reference estimates zero rows/work at that recursive edge;
  the visible seed/nonrecursive body still estimates normally. Iteration cardinality needs a later
  recursive-growth model rather than an implementation-dependent planning loop.
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
  minimum of one per inspected key; estimated bucket verification uses estimated join candidates
  and includes each composite part's four-byte length frame;
- `aggregate_accumulate` is `input_rows × aggregate_count`;
- `cte_scan_row` is materialized rows per reference;
- `generated_row` is the SRF cardinality;
- `window_result` is output rows per window function. The current model has no
  partition-size/distribution statistic for `window_frame_step`, so that size-dependent extra is
  zero;
- `constraint_check` and `value_compress` are zero in current SELECT selection.

Join candidates count their different repetition shapes explicitly:

- the current materializing nested loop scans each base input once, evaluates its ON expression for
  every saturated Cartesian candidate pair, and emits the §7.2 join-row estimate; unmetered loop
  control and row concatenation add nothing;
- an index nested loop multiplies the inner access candidate's structural reads, access-method
  units, fetched rows, and residual/ON invocations by outer rows, in outer-row order; and
- a hash join scans each input once, charges `hash_build` for build rows/bytes and `hash_probe` for
  probe rows/bytes, then reapplies and charges the full ON expression for estimated
  bucket-verification candidates.

For an eligible ordered join top-N, let `T = OFFSET + LIMIT`, let `J` be the estimated post-ON,
post-WHERE rows before the window, and let `L` be the rows in the physical left subtree presented
to the final join step. When `T > 0` and `J > T`, the estimated number of left rows whose final join
runs are started is `min(L, ceil(T * L / J))`; `J = 0` conservatively starts all `L`, and `T = 0`
starts none. The multiply/divide uses quotient/remainder saturation, never float arithmetic. This
is the current row-count-only uniform-fanout model; column statistics leave it unchanged because
per-join fanout correlation is not a collected fact. It discounts only final-step work the executor
actually skips: nested-loop
candidate/ON visits, hash probes and bucket verification, or repeated INL inner scans. The selected
left subtree and ordinary base scans remain complete, as does a final hash build. In the
two-relation case `L` is the selected driver relation.

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

LIMIT/ordered-stream short-circuit is a physical-plan property: eligible single-relation scans and
backward-safe window top-N plans reduce their child scan to the rows/pages expected to be pulled,
not an eagerly estimated full input sliced only at the Limit node. An unbounded secondary-index
order prices the expected index prefix plus table point fetches. Bound index/GiST/GIN paths retain
their conservative structural descent and reduce admitted table fetches. A join-PK-ordered
stream applies §8.2's deterministic final-step prefix model while retaining its complete left
subtree, base materialization, and hash-build work. Consequently estimates are computed over a
complete candidate pipeline rather than by blindly adding immutable logical-node estimates.

The following attribution rules close the remaining current-plan shapes:

- a residual `Filter` applies the complete predicate once to the logical input population, caps the
  result by rows physically delivered by its child, and adds expression work for the child rows;
- a nested-loop join adds both input scans once and ON work over its saturated candidate pairs; an
  index-nested-loop repeats the selected inner access work by outer rows; a hash join adds its fixed
  build/probe work and ON recheck work over estimated bucket candidates;
- `Aggregate`, `Window`, `Distinct`, `Sort`, and `Limit` transform rows in executor pipeline order;
  unmetered bookkeeping adds no private unit. Projection work uses its real invocation population
  (`Distinct` projects before dedup; an ordinary projection is after the final window), while
  `row_produced` uses final emitted rows;
- a set-operation node owns the saturated sum of operand unit vectors. Its combine, trailing sort,
  and trailing limit are unmetered even when they change `est_rows`;
- a derived-table `Subquery` owns its recursively estimated body. Literal `Values` owns exact rows
  plus expression work; an SRF owns argument work plus `generated_row`; and a FROM-less `Result`
  starts from one virtual row and owns no scan work;
- a materialized CTE body contributes once and each reference adds `cte_scan_row`; an inlined body
  contributes at each reference; an unreferenced read-only body contributes zero. Definition
  subtrees remain intrinsically estimated for display, but a `WITH` root sums semantic execution
  contributions rather than blindly summing those metadata edges; and
- subqueries inside expressions contribute once when uncorrelated and once per estimated invoking
  row when correlated. Unknown value-size extras remain zero under §8.2's existing fallback.

### 8.4 DML estimates

The estimator covers every node in the DML shapes plain EXPLAIN currently renders, while the fixed
DML access-path policy remains authoritative:

- `INSERT ... VALUES` starts with the exact authored candidate count; `INSERT ... SELECT` owns its
  rendered source-query estimate;
- `UPDATE`/`DELETE` own the selected mutation scan plus residual-filter estimate, including a filter
  expression's scalar subplans; and
- a DML root's `est_rows` is affected rows. `ON CONFLICT DO NOTHING` / `DO UPDATE` keeps the source
  candidate count until conflict-frequency statistics exist.

Mutation-only work with no current rendered node — VALUES/default/assignment/check/`RETURNING`
expressions, uniqueness probes, compression, and phase-two writes — uses the initial zero fallback.
That is an explicit precision boundary, not an assertion that execution performs no such work; the
runtime meter remains authoritative. A later DML-estimator slice may add those units without changing
the legacy mutation access policy or root cardinality. These estimates never change mutation
execution or its actual cost.

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

### 9.1 Single-relation selector

The single-relation rule applies to a SELECT plan containing exactly one base relation and no join.
The rule applies recursively to a qualifying single-relation subquery, but not to SRF, CTE, or
derived-table relation nodes, which have no base-store candidate inventory. A multi-relation SELECT
feeds the same candidate inventories into §9.2/§10 so access paths are priced together with join
orientation and algorithm; independently replacing one join input from a base-only estimate would
not minimize the whole join plan.

The candidate set contains every legal access path: PK, every ordered B-tree bound, every GiST
bound, every GIN bound, PK interval set, every ordered-index interval set, and full scan.

The selector compares **complete single-relation physical pipelines**, not isolated base scans.
Each access candidate is composed with the ordering property it naturally provides:

- a table-storage-order path uses an eligible PK `ORDER BY` direction;
- a B-tree bound or ordered-index interval set uses an eligible exact same-index `ORDER BY`; and
- an incompatible order remains a blocking Sort whose bookkeeping contributes no private weight.

For `ORDER BY ... LIMIT`, every eligible secondary B-tree order that has no same-index access bound
adds one **order-only B-tree** candidate. It walks that index and point-fetches table rows while
retaining the complete WHERE residual. When the same index already supplies a legal bound, its one
candidate composes the bound and order rather than duplicating the identity. Multiple matching
ordering indexes are all inventoried and sorted by lowercased name; catalog order never chooses one.
The existing exact-order, forward `ASC NULLS LAST`, fixed-width-PK, non-partial-index gates remain
unchanged.

For every pipeline, the estimator applies the selected access work, residual, projection and other
nodes at their real stages, then applies any ordering/streaming `OFFSET + LIMIT` prefix. This makes a
plain `LIMIT` as well as an `ORDER BY ... LIMIT` capable of changing the winner when it changes
scheduled page, row, access-method, or expression work. A blocking sort comparison still adds zero:
the estimator does not price unmetered sorting. The winner is the lowest cumulative `est_cost`
after those effects, followed by §9's access-kind/name tie order. A natural-order form is canonical for its
access identity, so an otherwise-identical blocking-sort duplicate is not inventoried.

The resulting rule is deterministic:

1. build and estimate the complete access-path inventory;
2. compose each access path with its one canonical natural-order property;
3. add any missing eligible order-only B-tree identities;
4. estimate every complete pipeline through its final LIMIT/OFFSET; and
5. choose minimum cumulative cost, retaining the first candidate on an exact cost tie because the
   candidate list is already in §9 order.

UPDATE and DELETE continue to call the fixed mutation policy from §1/§8.4.

### 9.2 Two-relation selector

This rule applies when a SELECT has exactly two non-lateral base relations joined by one INNER or
CROSS edge. An SRF, CTE, derived table, outer join, or correlated/dependency-bearing input is a barrier and
keeps the complete fixed FROM-order policy. Attachments remain eligible base relations, but their
existing access-path inventory may contain only the full path.

The physical plan stores `relation_order = [outer_source_ordinal, inner_source_ordinal]`; resolved
column slots and every expression remain in logical source order. Execution places each local base
row into its original global slot interval before evaluating ON, WHERE, ORDER BY, or projection, so
reordering changes neither name resolution nor row shape.

For each of the two source-ordinal orientations, inventory in §9 order:

1. every legal ordinary access candidate for the outer relation;
2. every legal ordinary access candidate for the inner relation;
3. materializing nested loop for each outer/inner access pair;
4. deterministic hash join for the same pair when the existing safe ON-only bare-column equijoin
   gate accepts it; and
5. every legal INL inner access whose bare sibling source belongs to the chosen physical outer.

The selector does not derive a hash key from WHERE and therefore does not turn
`CROSS JOIN ... WHERE a=b` into a hash join. That is a separate logical-predicate rewrite. For hash,
the physical inner is the build input and the physical outer is the probe input; the selected key
records those roles explicitly. For INL, constants on the same key may tighten the bound, but a sibling column is legal
only when its owning relation is already on the physical left. Every full WHERE and ON expression
remains residual and authoritative.

Each candidate then recomputes join-order sort elision against its physical outer. The established
gate remains exact: two base relations, INNER/CROSS, forward outer-PK ORDER BY with no trailing key,
LIMIT, storage-order outer access, and a storage-key-ordered INL inner if present. An eligible
candidate applies §8.2's top-N prefix model; all others retain the blocking Sort, whose bookkeeping
has no private estimator weight.

Candidates are structurally sorted before estimation by physical relation sequence, outer access,
inner access, and join-algorithm rank. The minimum complete-pipeline cost wins; retaining the first
candidate on an exact cost tie therefore implements §9 without map iteration. The selected plan's
physical visit order also defines deterministic error visitation and cost-ceiling abort order.
The costed selector deliberately does not preserve FROM-order precedence between multiple possible
runtime errors; portable error corpus cases use a single offending evaluation unless a selected plan
is itself the behavior under test.

## 10. Join search and its deterministic bound

Only left-deep orders are in Path B. INNER/CROSS relations form reorderable islands; outer joins,
LATERAL, correlation, and other dependency-bearing nodes are barriers. The two-relation orientation
rule is the smallest instance of the N-way island search.

### 10.1 Islands and predicate ownership

An eligible input is a non-lateral base table. SRFs, CTE scans, derived tables, correlated inputs,
and every outer-join edge are hard fences. No relation or compound subtree moves across a fence.
In the resolver's authored left-deep chain, the all-base INNER/CROSS prefix is the first island. After
a fence, the fixed left prefix and the relation introduced by the fence remain in source order; a
following run of two or more base relations introduced by INNER/CROSS edges is a new reorderable
island appended to that fixed prefix. This is deliberately narrower than treating an outer-join
subtree as an atomic input that may move through a higher INNER join.

Each authored `ON` expression remains one intact eager expression tree. Its dependency set is the
union of:

- the original right-side/source-owner relation of that join; and
- every local base relation referenced by the expression.

Within an island, evaluate that complete tree at the earliest physical join step whose joined subset
contains the dependency set. If several trees become ready at one step, evaluate them by authored
join ordinal. Never split an `AND` tree merely to schedule or estimate it earlier: splitting would
change eager evaluation, errors, and `operator_eval` cost. A barrier step evaluates its own `ON` in
the authored position. The selected physical order defines deterministic error visitation.

An INL bound for the newly appended relation may use a sibling column from any relation already in
the physical left prefix. A hash alternative may use equality keys from any newly-ready `ON` tree
between that prefix and the appended relation. Hash remains legal only when the *complete* newly-ready
set passes the existing non-trapping safe-conjunct gate; every newly-ready tree remains an
authoritative residual recheck in authored order.

### 10.2 Partial state and exhaustive DP

For an island of at most `join_dp_limit = 8` movable base relations, enumerate left-deep candidates
by dynamic programming over source-ordinal subsets. A partial state contains:

```
JoinState {
    subset                 # source-ordinal bit set within the island
    relation_sequence      # physical prefix, source ordinals
    per_relation_access    # ordinary bound or the appended relation's INL bound
    per_step_algorithm     # INL, hash, or nested loop plus newly-ready ON ordinals
    estimate               # cumulative units, physical rows, logical rows
    satisfies_query_order  # requested forward driver-PK order, or false
}
```

The order property is query-specific rather than a catalog of every possible interesting order.
It is true only when the physical driver uses the storage-key form accepted by the existing
join-PK-order gate and the complete left-deep prefix preserves that order. Nested loop, INL, and the
deterministic probe-outer hash operator preserve their left input's order.

Access paths can produce different physical row estimates, so retaining only the cheapest state per
subset is not exhaustive under jed's estimator: a more expensive but smaller prefix can make every
later repeated seek or probe cheaper. For each `(subset, satisfies_query_order)` bucket, retain the
deterministic Pareto frontier over:

```
(cumulative est_cost, physical rows, logical rows)
```

State A dominates B only when all three values are `<=` and at least one is `<`. For identical
physical/logical rows, retain only the lower-cost state; for an exact triple tie retain §9's
canonical structural winner. Future left-deep extensions are monotone in all three dimensions, so a
dominated state cannot become a winner. Saturation does not weaken that property.

Enumeration is independent of maps: process subset cardinality ascending, subset masks numerically
ascending, retained states in §9 structural order, not-yet-joined source ordinals ascending, access
identities in canonical access order, and algorithms in canonical algorithm order. Implementations
may use a map for lookup only; they sort before every decision. A completed state's final comparison
uses the complete SELECT pipeline, including the final ordering/LIMIT rule, then §9.

### 10.3 Larger islands

For a larger island:

1. choose the cheapest one-relation driver candidate;
2. repeatedly append the not-yet-chosen relation plus legal access/join algorithm producing the
   cheapest new cumulative plan; and
3. break every tie by §9.

This deterministic cheapest-next fallback is polynomial and uses no random/GEQO path. Eight follows
PostgreSQL's default join-collapse boundary; the overriding reason for not adopting GEQO beyond it
is cross-core identity and reproducibility.

For a post-fence island, the already-fixed prefix is the driver state and step 1 is omitted. Greedy
selection retains one state, not a frontier. Every round inventories remaining source ordinals,
access identities, and algorithms in the same order as §10.2 and retains the first minimum under
§9. Planning exposes no counter or timing-dependent early exit.

### 10.4 Final ordering and LIMIT

After an island winner is installed, recompute ORDER BY satisfaction against the complete physical
tree. The initial N-way ordered-LIMIT executor fully materializes the selected left subtree and
streams only the final join step. Therefore the estimator retains every earlier scan/join unit and
applies §8.2's uniform prefix formula only to the final step's outer-prefix visits, hash probes and
bucket verification, or repeated INL inner work. Ordinary base scans and a final hash build remain
complete. This is the direct N-way extension of the two-relation selector and deliberately does not
claim a fully streaming recursive join tree; such an executor would require a broader per-level
discount model.

Join-PK order is ineligible across a semantic fence. Without an eligible ordered LIMIT, the complete
tree retains the blocking Sort; its unmetered bookkeeping still adds no private planner weight.

## 11. Column-statistics refinement

[statistics.md](statistics.md) owns collection, persistence, staleness, type eligibility, and the
complete formulas. The estimator-facing summary is:

- planning reads only resident snapshot facts; ANALYZE is the only leaf-scanning collection path;
- current NULL/MCV/histogram populations rescale from `analyzed_rows` to exact current table rows;
- low analyzed NDV (at most 10% of analyzed non-NULL rows) stays fixed, while higher NDV scales
  proportionally and clamps to current non-NULL rows;
- a known literal may match MCV or the step histogram; a generic parameter may use average NDV
  density but never a value bucket;
- eligible equality joins use both inputs' non-NULL rows and maximum NDV; simple-column GROUP BY /
  DISTINCT uses the product of per-column NDVs plus a NULL group; and
- average canonical key width replaces the variable-width hash fallback where present.

Facts never change §3 arithmetic or §9 ties. A stale/sample statistic is heuristic and cannot turn
an unobserved literal into a structural zero. No-statistics and ineligible shapes retain §§4/7's
stable defaults.

## 12. EXPLAIN contract

EXPLAIN appends two non-NULL `i64` columns to its existing result:

| column | meaning |
|---|---|
| `est_rows` | rows emitted by this node to its rendered parent (§8.3) |
| `est_cost` | saturated cumulative scheduled cost through this node (§8.3) |

Fallback rules make both values available for every node, so NULL is unnecessary. Existing
`depth`/`node`/`detail` spelling and pre-order row order stay unchanged. EXPLAIN ANALYZE's
`cost=<actual> rows=<actual>` root remains actual runtime data; estimated columns remain estimates.
The EXPLAIN statement itself still charges one actual `row_produced` per rendered row — adding two
cells changes no render cost.

For a DML root, `est_rows` means affected rows (§8.4). An `Analyze` wrapper repeats its planned
child's two estimate values; the actual `cost=<actual> rows=<actual>` figures remain confined to the
detail cell. `WITH` uses the semantic CTE attribution in §8.3 and explain.md rather than treating
definition-display edges as repeated execution.

Plain EXPLAIN is jed-owned and not PostgreSQL-oracle imported. Its estimate rows are asserted
`nosort` in the shared corpus, making arithmetic, candidate choice, and tie breaks one cross-core
contract.

## 13. Conformance coverage

- `rake verify` checks shared estimator facts, exact arithmetic vectors, and complete tie orders.
- Byte goldens, transactional row-count/statistics tests, reopen tests, and cache fresh-vs-hit parity
  cover persisted and snapshot inputs.
- Shared access-candidate and estimator vectors pin every base access method.
- EXPLAIN rows and `# cost:` assertions pin the selected physical tree, estimates, and actual
  runtime consequence.
- Each enabled physical choice carries a NoREC relation and affected benchmark lane.
- ANALYZE coverage includes SQL/cost corpus cases, v29 cross-core/Ruby goldens, collection
  arithmetic, retained-stale/cache/rollback behavior, plan flips, NoREC, and skew/uniform lanes.

PostgreSQL remains the result oracle, not the plan/estimate oracle. The borrowed default
selectivities are recorded data, not a promise to reproduce PostgreSQL plans.

## 14. Deliberate boundaries and deferred work

- No parameter-sensitive/custom plans in the current planner.
- No cost-based DML access policy until a mutation-specific slice.
- No extended/multi-column statistics, automatic analyze, configurable targets, MCV-aware join
  skew, or distribution facts for composite/array/json/jsonb.
- No planner-only wall-clock cost model.
- No bushy join trees, GEQO/random search, parallel-plan search, or adaptive runtime re-planning.
- No planning-time leaf reads or statistics sampling.
- No claim that `est_cost == actual cost`; equality is expected only for fully known simple shapes.

These are contained doors, not accidental omissions. Each can land only by extending the shared
data/algorithm contract and its cross-core fixtures first.
