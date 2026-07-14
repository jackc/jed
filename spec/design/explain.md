# EXPLAIN — design

> `EXPLAIN [ANALYZE] <statement>` renders the planner's chosen plan as a deterministic result
> set instead of running the statement. This is the observability surface for the cost-based
> planner: it makes the planner's access-path / join / sort-elision decisions
> inspectable and, crucially, **assertable in the shared conformance corpus** — one entry
> verifies all three cores render a byte-identical plan. This doc is the contract all three
> cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf), the capabilities in
> [../conformance/manifest.toml](../conformance/manifest.toml), and the cost contract EXPLAIN
> ANALYZE reports in [cost.md](cost.md). EXPLAIN is **jed-owned surface**: PostgreSQL's plan
> text is not something jed reproduces byte-for-byte (CLAUDE.md §1), so EXPLAIN entries are
> **not** oracle-imported.

## 1. Surface

```sql
EXPLAIN [ANALYZE] <statement>
```

`EXPLAIN` and the optional `ANALYZE` modifier are recognized **positionally** and stay
**non-reserved** (grammar.md §3): no statement begins with a bare identifier, so the leading
word disambiguates without lookahead, and `explain` / `analyze` remain legal identifiers
everywhere else. The inner statement is restricted to a **query** (`SELECT`, a set operation,
a read-only `WITH`) or a **DML** statement (`INSERT` / `UPDATE` / `DELETE`); DDL, transaction
control, and a nested `EXPLAIN` have no query plan to render and are rejected **42601**.

- **Plain `EXPLAIN`** plans the inner statement and renders the plan **without executing it** —
  an `EXPLAIN DELETE` deletes nothing, an `EXPLAIN INSERT` inserts nothing. It is always a
  **read**, even of a DML inner. Bind parameters are not accepted (a `$N` renders symbolically).
- **`EXPLAIN ANALYZE`** additionally **runs** the inner statement and reports its actual
  accrued cost + row count (§3). Of a DML statement it is a **write** — the mutation runs and
  commits (`stmtIsWrite` classifies `analyze && inner-is-write` as a write).

**Privileges** are those of the inner statement (an `EXPLAIN INSERT` requires `INSERT`), matching
PostgreSQL — even though plain `EXPLAIN` never executes.

## 2. Output shape (the two harness constraints that fix it)

EXPLAIN produces an ordinary query result set (a SELECT-shaped `Outcome`) with five columns:

| column | type | meaning |
|---|---|---|
| `depth`  | `i32`  | the plan node's nesting level (0-based), from a **pre-order DFS** of the plan tree |
| `node`   | `text` | the operator label — a **fixed cross-core vocabulary** (§4), the §8 spelling contract |
| `detail` | `text` | the node's attributes (access path, keys, counts); the sentinel `-` when it has none |
| `est_rows` | `i64` | rows this node is estimated to deliver to its rendered parent; a DML root uses estimated affected rows |
| `est_cost` | `i64` | saturated cumulative scheduled cost attributable through this node |

The estimate columns are non-NULL shortest-decimal integers in `0..i64::MAX`. They are planner
estimates, not safety limits and not a promise to equal execution. Their exact arithmetic and
per-node attribution are specified in [estimator.md §8.3/§11](estimator.md). There is no unavailable
sentinel: deterministic fallback rules produce an estimate for every currently renderable node.
For a one-base-relation SELECT, complete-pipeline `est_cost` chooses among every access
method and eligible order-only B-tree walk; `query/cost_plan_access*.test` asserts row-count and
selectivity flips, cross-method/name ties, scan-order composition, actual cost, and the staged
mutation boundary.

Two properties of the conformance harness ([../conformance/README.md](../conformance/README.md);
`impl/*/…/conformance` render the actual cell **raw** while the expected line is `TrimSpace`d, and a
blank cell terminates a record) fix the format:

1. **No cell carries leading or trailing whitespace.** Indentation is the `depth` integer, never
   spaces — this is why the output is columnar rather than an indented text tree.
2. **No cell is ever the empty string.** A node with no attributes renders `detail = "-"`.

Rows are emitted in **pre-order**, so the tree reads top-down as the executor's pipeline reads
bottom-up, and the order is **deterministic by construction**. EXPLAIN is therefore asserted with
**`nosort`** — a sanctioned use of `nosort` on an ORDER-BY-less result, exactly like composite
`record_out`'s fixed field order (conformance.md), because the row order is a property of the
rendering, not of a scan or hash.

## 3. `EXPLAIN ANALYZE` — deterministic actual cost

`EXPLAIN ANALYZE` runs the inner statement and prepends an **`Analyze`** root node whose detail is
`cost=<C> rows=<R>`, with the plan tree shifted one level deeper. `<C>` is the inner statement's
**actual accrued cost** and `<R>` is its row count (the rows returned, or the affected-row count for
a DML statement without `RETURNING`). Because jed's cost meter is a **deterministic, byte-identical
cross-core contract** (cost.md §1), `<C>` is reproducible across cores and asserted as an ordinary
cell — no `# cost:` directive is needed for it. This is what makes ANALYZE well-defined here where a
wall-clock ANALYZE would not be.

**Two independent cost figures.** The inner statement's real cost lives only in the `Analyze` root.
The `EXPLAIN` statement's **own** cost (its `Outcome.Cost`) is **one `row_produced` per emitted plan
row** — a small, deterministic function of the plan-row count, independent of the inner cost. So a
`# cost:` directive on an `EXPLAIN ANALYZE` pins the render cost, while the (larger) inner cost
appears only inside the root. Plain EXPLAIN runs no inner meter; it charges only its render cost.

The `Analyze` wrapper repeats the planned child's `est_rows` and `est_cost`; only its `detail`
contains actual figures. This keeps the estimated and actual surfaces separate without inventing a
second estimate for a renderer-only wrapper.

Per-node cost attribution is **out** for now (jed's meter is a single global counter); ANALYZE
reports the whole-statement figure. Per-node metering is a possible follow-on.

## 4. Node + detail vocabulary (the §8 cross-core spelling contract)

Every token below is fixed and must be spelled identically in every core — this is what makes the
corpus assertion cross-core-meaningful. The renderer is **hand-written per core** (like the rest of
the executor, §5 forbids codegenning it); the corpus + this table are the contract.

### Nodes

| source | node label |
|---|---|
| base-table scan | `Scan <table>` |
| set-returning function in FROM | `SRF <name>` |
| CTE reference | `CTE Scan <name>` |
| derived table / subquery in FROM | `Subquery <alias>` |
| nested-loop join | `Nested Loop` |
| hash join | `Hash Join` |
| residual WHERE | `Filter` |
| aggregation / GROUP BY | `Aggregate` |
| window stage | `Window` |
| DISTINCT | `Distinct` |
| ORDER BY (not elided) | `Sort` |
| LIMIT / OFFSET | `Limit` |
| FROM-less query | `Result` |
| VALUES relation | `Values` |
| set operations | `Union` / `Intersect` / `Except` |
| WITH wrapper / a CTE binding | `WITH` / `CTE <name>` |
| DML roots | `Insert <table>` / `Update <table>` / `Delete <table>` |
| EXPLAIN ANALYZE root | `Analyze` |

jed has one aggregation strategy (`Aggregate`) and two join operators (`Nested Loop`, `Hash Join`).
There is still no `HashAggregate` / `GroupAggregate` split.

### Operator order for a SELECT

Emitted outermost-first, each the pre-order parent of the next, so the tree reads top-down as the
pipeline reads bottom-up: **Limit → Sort → Distinct → Window → Aggregate → Filter → FROM tree**. A
node is emitted only when present. The FROM tree is a left-deep chain of join nodes over the plan's
physical relation order (the outermost node is the last join; its right child is the physical inner
relation), bottoming out at relation leaves. EXPLAIN may therefore render any relation from a
searched INNER/CROSS island as the driver or a later inner leaf. Hard-fenced outer/dependency
relations stay
in their authored position, and independently searched islands may follow them. Resolved logical
slots remain in source order; EXPLAIN shows execution order.

### Detail grammar

Attributes are `; `-separated; a node with none renders `-`.

- **Scan access path** (from the relation's chosen bound): `Full scan` · `PK bound: <col> <op> <src>`
  (conjuncts joined by ` and `; composite-PK members render in key order) ·
  `Index bound: using <index>` · `GIN bound: using <index>` ·
  `GiST bound: using <index>` · `PK interval set: <col>; intervals=N` ·
  `Index interval set: using <index>; intervals=N`. `N` is the structural OR-leaf count before
  runtime parameter encoding/canonicalization. The index name is the stored **lowercased** name. `<op>` is one of
  `= <> < > <= >=`. `<src>` is `$N` (a bind parameter, 1-based), `outer` (a correlated outer column),
  or a literal (integer / boolean / decimal / quoted text rendered via the value's canonical form; a
  **float** renders as the fixed token `<float>` — see §5).
- **Touched columns** (SELECT scans only): `touched=K`, the count of columns the query statically
  references (cost.md "touched set"); omitted when zero (e.g. `count(*)`). UPDATE/DELETE scans omit
  it (a DML touched set includes assignment sources; left to a follow-on).
- **Sort elision** (on the FROM top node when an ORDER BY is served by scan order rather than a
  `Sort`): `ordered: pk ordered` (`(reverse)` for a DESC scan) · `ordered: index order: <index>` ·
  `ordered: join pk ordered`.
- **Sort**: `keys=N`; when the blocking ORDER BY + LIMIT rule applies, `keys=N, top-k=K`, where
  K is the checked `OFFSET + LIMIT` retained-row bound (`LIMIT 0` renders `top-k=0`).
- **Filter / join ON**: `conjuncts=N` (top-level AND count). A full expression printer is a
  follow-on (§5); v1 renders a count, not the predicate text — except the compact bound predicate
  above.
- **Aggregate**: `groups=G aggs=A` (+ `sets=S` when more than one grouping set; + `having:conjuncts=K`).
- **Window**: `funcs=N`. **Nested Loop**: `<kind>` (`inner`/`cross`/`left`/`right`/`full`) +
  `on:conjuncts=N`; when several authored ON trees first become ready at the same physical step, the
  suffix is `on:predicates=P,conjuncts=N`. **Hash Join** inserts `keys=K` before the same ON suffix
  (`kind` is `inner`, or `left` on the established two-input path).
- **Limit**: `limit=N` / `offset=M` (an absent side omitted). **Values**: `rows=N`.
- **Set op**: `all` / `distinct`. **CTE**: `inlined` / `materialized` (the planner's choice) + `recursive`.
- **Insert**: `-` or `on conflict do nothing` / `on conflict do update`. **Update**: `sets=N`. **Delete**: `-`.
- **Analyze**: `cost=<C> rows=<R>`.

## 5. Determinism (why no ledger entry is needed)

The plan structs are already cross-core identical (they drive the `# cost:` contract), so the
rendering is deterministic **by construction provided every emitted token is deterministic**. The
surfaces and how each is pinned:

- **Index names** — always the stored lowercased name; estimated cost chooses across the complete
  single-relation set and deterministic lowest-name order breaks an exact same-kind cost tie
  (indexes.md §5).
- **Iteration order** — relation leaves iterate the selected physical-order slice inside each
  island and authored order at every hard fence; retained DP states, joins, aggregates, and CTE
  bindings iterate their specified structural order, never a map.
- **Literal rendering** — integer / boolean / decimal / text / date / timestamp / uuid render
  deterministically. **`float` is the one hazard** (its layout is a ratified determinism-ledger
  exception, and floats are keyable), so a float bound literal renders as the fixed token `<float>`,
  keeping the plan text off the ledger entirely.
- **Residual predicates** — a conjunct **count** (a deterministic integer), not expression text.

So **v1 needs no `determinism_exceptions.toml` entry.** Two follow-ons *would*: exact float-literal
bound rendering, and a full expression printer.

### Estimate attribution for non-tree execution shapes

Most rendered parent/child edges are execution-subtree edges and cumulative cost composes normally.
`WITH` is the deliberate exception because its display contains both CTE **definitions** and the
main body, while execution may inline a definition at a reference instead of executing it at its
displayed definition site:

- a materialized referenced `CTE <name>` owns its body once; a materialized `CTE Scan <name>` owns
  only `cte_scan_row` buffer reads;
- an inlined `CTE <name>` definition contributes zero at the definition site, while its `CTE Scan`
  owns the referenced body's intrinsic estimate;
- an unreferenced read-only CTE contributes zero even though its child plan remains displayed and
  carries its intrinsic informational estimate; and
- the `WITH` root includes only contributions that execution actually performs, plus its main body.

This semantic attribution is intentionally not a blind sum over every displayed metadata edge.
Derived-table `Subquery` edges are ordinary execution edges and do compose normally.

For DML, the root's `est_rows` is estimated affected rows, matching the row-count concept reported
by `EXPLAIN ANALYZE` for a mutation without `RETURNING`. `INSERT` uses estimated source candidates;
`UPDATE`/`DELETE` use the selected target scan plus the residual predicate. `ON CONFLICT` keeps the
candidate count until distribution statistics can predict conflicts without planning-time leaf I/O.

## 6. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **Format is jed's own**, not PG's indented `QUERY PLAN` text: structured
  `depth`/`node`/`detail`/`est_rows`/`est_cost` columns, chosen for corpus-assertability under the
  whitespace/empty-cell constraints of §2. Not oracle-imported.
- **No parenthesized option list** (`EXPLAIN (FORMAT …, VERBOSE, …)`); the surface is bare
  `EXPLAIN [ANALYZE] <stmt>`. A `(…)` option list is a possible follow-on.
- **Node vocabulary reflects jed's executor** (one `Aggregate`, one `Nested Loop`), not PG's richer
  set of physical operators — jed owns its surface.
- **ANALYZE reports deterministic accrued cost and actual root rows, not wall-clock time or
  per-node actuals** — the estimate columns remain planner values, which keeps both surfaces
  corpus-assertable.

## 7. Deferred follow-ons (none foreclosed)

- Per-node actual cost attribution under ANALYZE (needs a per-operator sub-meter).
- A full expression printer for the residual filter / projections (a ledgered spelling contract).
- Exact float-literal bound rendering (needs a ledger entry).
- A `(…)` option list; a streaming/buffered/deferred lane tag; EXPLAIN of a data-modifying `WITH`.
- The DML touched-set count (UPDATE/DELETE), and collation-name rendering in keys.
