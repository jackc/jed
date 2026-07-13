# EXPLAIN — design

> `EXPLAIN [ANALYZE] <statement>` renders the planner's chosen plan as a deterministic result
> set instead of running the statement. This is the observability substrate the cost-based
> planner work builds on: it makes the planner's access-path / join / sort-elision decisions
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

EXPLAIN produces an ordinary query result set (a SELECT-shaped `Outcome`) with three columns:

| column | type | meaning |
|---|---|---|
| `depth`  | `i32`  | the plan node's nesting level (0-based), from a **pre-order DFS** of the plan tree |
| `node`   | `text` | the operator label — a **fixed cross-core vocabulary** (§4), the §8 spelling contract |
| `detail` | `text` | the node's attributes (access path, keys, counts); the sentinel `-` when it has none |

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
| join | `Nested Loop` |
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

jed has **one** aggregation strategy and **one** join executor, so `Aggregate` and `Nested Loop`
are single spellings (no `HashAggregate` / `GroupAggregate` split — that would be a contract jed
cannot yet honor deterministically).

### Operator order for a SELECT

Emitted outermost-first, each the pre-order parent of the next, so the tree reads top-down as the
pipeline reads bottom-up: **Limit → Sort → Distinct → Window → Aggregate → Filter → FROM tree**. A
node is emitted only when present. The FROM tree is a left-deep chain of `Nested Loop` nodes over
the plan's relations (the outermost node is the last join; its right child is the last relation),
bottoming out at relation leaves.

### Detail grammar

Attributes are `; `-separated; a node with none renders `-`.

- **Scan access path** (from the relation's chosen bound): `Full scan` · `PK bound: <col> <op> <src>`
  (conjuncts joined by ` and `; composite-PK members render in key order) ·
  `Index bound: using <index>` · `GIN bound: using <index>` ·
  `GiST bound: using <index>`. The index name is the stored **lowercased** name. `<op>` is one of
  `= <> < > <= >=`. `<src>` is `$N` (a bind parameter, 1-based), `outer` (a correlated outer column),
  or a literal (integer / boolean / decimal / quoted text rendered via the value's canonical form; a
  **float** renders as the fixed token `<float>` — see §5).
- **Touched columns** (SELECT scans only): `touched=K`, the count of columns the query statically
  references (cost.md "touched set"); omitted when zero (e.g. `count(*)`). UPDATE/DELETE scans omit
  it (a DML touched set includes assignment sources; left to a follow-on).
- **Sort elision** (on the FROM top node when an ORDER BY is served by scan order rather than a
  `Sort`): `ordered: pk ordered` (`(reverse)` for a DESC scan) · `ordered: index order: <index>` ·
  `ordered: join pk ordered`.
- **Filter / join ON**: `conjuncts=N` (top-level AND count). A full expression printer is a
  follow-on (§5); v1 renders a count, not the predicate text — except the compact bound predicate
  above.
- **Aggregate**: `groups=G aggs=A` (+ `sets=S` when more than one grouping set; + `having:conjuncts=K`).
- **Window**: `funcs=N`. **Nested Loop**: `<kind>` (`inner`/`cross`/`left`/`right`/`full`) + `on:conjuncts=N`.
- **Limit**: `limit=N` / `offset=M` (an absent side omitted). **Values**: `rows=N`.
- **Set op**: `all` / `distinct`. **CTE**: `inlined` / `materialized` (the planner's choice) + `recursive`.
- **Insert**: `-` or `on conflict do nothing` / `on conflict do update`. **Update**: `sets=N`. **Delete**: `-`.
- **Analyze**: `cost=<C> rows=<R>`.

## 5. Determinism (why no ledger entry is needed)

The plan structs are already cross-core identical (they drive the `# cost:` contract), so the
rendering is deterministic **by construction provided every emitted token is deterministic**. The
surfaces and how each is pinned:

- **Index names** — always the stored lowercased name; the planner's index choice is already a
  deterministic lowest-name tie-break (indexes.md §5).
- **Iteration order** — relations, joins, aggregates, CTE bindings iterate in slice order, never a
  map.
- **Literal rendering** — integer / boolean / decimal / text / date / timestamp / uuid render
  deterministically. **`float` is the one hazard** (its layout is a ratified determinism-ledger
  exception, and floats are keyable), so a float bound literal renders as the fixed token `<float>`,
  keeping the plan text off the ledger entirely.
- **Residual predicates** — a conjunct **count** (a deterministic integer), not expression text.

So **v1 needs no `determinism_exceptions.toml` entry.** Two follow-ons *would*: exact float-literal
bound rendering, and a full expression printer.

## 6. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **Format is jed's own**, not PG's indented `QUERY PLAN` text: structured `depth`/`node`/`detail`
  columns, chosen for corpus-assertability (the whitespace/empty-cell constraints of §2) and for
  clean forward extension (future `est_rows`/`est_cost` columns for the cost-based planner slot in as
  new columns). Not oracle-imported.
- **No parenthesized option list** (`EXPLAIN (FORMAT …, VERBOSE, …)`); the surface is bare
  `EXPLAIN [ANALYZE] <stmt>`. A `(…)` option list is a possible follow-on.
- **Node vocabulary reflects jed's executor** (one `Aggregate`, one `Nested Loop`), not PG's richer
  set of physical operators — jed owns its surface.
- **ANALYZE reports deterministic accrued cost, not wall-clock time / actual-vs-estimated rows** —
  the property that makes it corpus-assertable.

## 7. Deferred follow-ons (none foreclosed)

- Per-node cost attribution under ANALYZE (needs a per-operator sub-meter).
- A full expression printer for the residual filter / projections (a ledgered spelling contract).
- Exact float-literal bound rendering (needs a ledger entry).
- Estimated-cost columns (`est_rows`/`est_cost`) once a plan-time cost estimator lands — the reason
  the structured-column shape was chosen.
- A `(…)` option list; a streaming/buffered/deferred lane tag; EXPLAIN of a data-modifying `WITH`.
- The DML touched-set count (UPDATE/DELETE), and collation-name rendering in keys.
