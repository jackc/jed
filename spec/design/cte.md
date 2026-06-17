# Common table expressions (`WITH`) — design

> The reasoning behind non-recursive CTEs. The grammar is authoritative
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) `query_statement` / `with_clause` / `cte`);
> the cost contract lives in [cost.md](cost.md) §3; this doc is the *why*. When a decision here
> changes, change it in the grammar / cost doc / corpus in the same edit, and update
> [CLAUDE.md](../../CLAUDE.md) / [TODO.md](../../TODO.md) if it revises a load-bearing commitment.

A **common table expression** names a query and exposes it to the statement's FROM clause as a
relation: `WITH t AS (SELECT …) SELECT … FROM t`. A CTE is, structurally, a **named derived
table** — a relation that is not a catalog table — so it generalizes the seam the set-returning
function `generate_series` already opened ([functions.md](functions.md) §10): a `FROM` relation
need not be a stored table, it can be a **computed row source**. CTEs make that source a *planned
query*, named and visible by scope rather than produced by a builtin.

This slice is **non-recursive only**. `WITH RECURSIVE`, data-modifying CTEs
(`WITH x AS (INSERT … RETURNING …)`), and `WITH` attached to `UPDATE`/`DELETE` are deferred
follow-ons (§6).

## 1. Surface

```
WITH a AS (SELECT 1 AS n),
     b (x, y) AS (SELECT n, n * 2 FROM a),
     c AS MATERIALIZED (SELECT * FROM b)
SELECT x, y FROM c CROSS JOIN a
```

- **A list of one or more CTEs** prefixes a top-level query (the `query_statement` production).
  Each CTE is a `name`, an optional parenthesized **column-rename list**, the keyword `AS`, an
  optional `[NOT] MATERIALIZED` hint, and a parenthesized **body** `( query_expr )`. The body is
  any query expression — a single `SELECT`, a JOIN, an aggregate, or a set operation
  (`UNION`/`INTERSECT`/`EXCEPT`).
- **Column-rename list** (`b (x, y)`): renames the body's output columns left-to-right. **Fewer**
  aliases than the body has columns is a **partial rename** — the first columns take the aliases and
  the rest keep their body output names (PostgreSQL). **More** aliases than columns is `42P10`
  (`invalid_column_reference`). Without the list, the column names are the body's own output names.
- **`MATERIALIZED` / `NOT MATERIALIZED`**: the explicit override of the evaluation rule (§3).
- A reference to a CTE in `FROM` works exactly like a table reference: bare or aliased
  (`FROM c AS k`), `SELECT *` expands its columns, qualified `c.x` works via the label mechanism.

**Top-level only.** The `WITH` prefix attaches at the statement level. A `WITH` *inside* a
parenthesized subquery or inside another CTE body is a documented narrowing this slice — the body
production is the WITH-less `query_expr`, so a nested `WITH` surfaces as a leftover-token `42601`.
CTE *references* inside nested subqueries are fully supported (§2).

## 2. Scope machinery

A CTE is a **statement-local relation** resolved by name, ahead of the catalog:

- **Forward-only visibility.** A CTE is visible to **later** CTEs in the same list, to the main
  query, and to the main query's nested subqueries — **never to itself or to earlier CTEs**. A
  self- or forward-reference therefore resolves no relation and raises `42P01`
  (`undefined_table`), the same code a missing table takes. (This is also precisely why
  `WITH RECURSIVE` is a separate feature: recursion *requires* a CTE to see itself.)
- **Shadowing.** A CTE name shadows a catalog table of the same name **within the statement** —
  `WITH orders AS (…) SELECT * FROM orders` reads the CTE, not the table. The one exception is the
  CTE's **own body**: because the binding is not yet in scope for itself,
  `WITH orders AS (SELECT * FROM orders)` resolves the inner `orders` to the **base table** (PG
  semantics), and is `42P01` only if no base table exists. Lookup is case-insensitive, like table
  names.
- **Bodies are independent queries — not correlated.** A non-recursive CTE body is planned as a
  top-level query (`parent = None`): it sees only its own FROM, the catalog, and the **earlier CTE
  bindings**, never the scope at the reference site. An outer-query column referenced inside a CTE
  body is therefore unresolved (`42703`/`42P01`), *not* a correlated reference. This is the most
  important behavioral rule — a CTE is **not** a lateral/correlated subquery.
- **Duplicate CTE name** in one `WITH` list is `42712` (`duplicate_alias`), matching PostgreSQL.
- **Duplicate output columns.** A body that produces two columns of the same name
  (`WITH t AS (SELECT 1 AS a, 2 AS a)`) is allowed; `SELECT *` emits both, but a later **bare**
  reference to that name is `42702` (ambiguous), exactly as a self-join of two same-labelled
  relations resolves. (Same rule a future inline derived table will take.)

Resolution is threaded as an **inherited binding slice**, not by walking the correlated `parent`
chain: every freshly built scope (a subquery's, a join operand's) inherits its parent's CTE
bindings directly, so a CTE is visible across nesting depth without being confused with an
`Outer{level}` correlated reference.

## 3. Evaluation rule — PostgreSQL's hybrid (the cost contract)

jed needs **one defined rule** because cost is a cross-core conformance contract (CLAUDE.md §13);
it adopts PostgreSQL ≥12's behavior because that rule is observable in the **rows**, not only the
cost:

- A CTE referenced **exactly once** (and not `MATERIALIZED`) is **INLINED** — its body runs in
  place at the FROM position, like a derived table.
- A CTE referenced **two or more times** (or marked `MATERIALIZED`) is **MATERIALIZED** — its body
  runs **once**, the rows are buffered, and each reference scans the buffer.
- `NOT MATERIALIZED` forces inlining even for a multi-reference CTE; `MATERIALIZED` forces
  buffering even for a single reference.

The reference count is a **static property of the parsed query tree** (how many times the CTE name
appears as a FROM relation in places that can see it — the main query, nested subqueries, and
later CTE bodies), so the inline-vs-materialize decision is identical across the three cores.

**Why this matters for rows, not just cost.** A body with a volatile function (`uuidv7()`,
`now()` via the entropy seam — [entropy.md](entropy.md)) produces *different* values each time it
runs. Inlined and referenced under a correlated subquery, it re-evaluates per outer row; materialized,
it is frozen once into the buffer. So matching PG's decision is what keeps jed's *rows* aligned with
PG, under the fixed entropy seed the corpus injects (so both stay deterministic). An unreferenced
CTE is planned and type-checked (its errors still surface) but **not executed**.

The deterministic cost formula for both paths is specified in [cost.md](cost.md) §3 (the
`cte_scan_row` unit for materialized buffer scans; inlined bodies charge their intrinsic cost).

## 4. Errors

| Condition | Code | Notes |
|---|---|---|
| Duplicate CTE name in one `WITH` list | `42712` | `duplicate_alias` (PG's code). |
| Self / forward reference (non-recursive) | `42P01` | The name is not in scope yet; falls out of §2. |
| MORE column-rename aliases than body columns | `42P10` | `invalid_column_reference`; fewer is a legal partial rename. |
| Outer column referenced inside a body | `42703` / `42P01` | A body is not correlated (§2). |
| `WITH RECURSIVE …` | `0A000` | Deferred (§6). |
| Nested `WITH` (in a subquery / body) | `42601` | Top-level-only narrowing (§1); leftover token. |

## 5. Cross-core determinism

Three contract points the cores must implement identically (called out because the cores' internals
differ — Rust's lazy iterators vs Go/TS materialized slices, TS generators — [cost.md](cost.md) §3):

1. **Buffer order = body plan output order.** A materialized CTE's buffer preserves the order the
   body plan produces, which is already byte-identical across cores under the existing SELECT
   determinism rules (scan in key order, left-deep joins). No `ORDER BY` is needed in the body for
   the result to be deterministic — but, as everywhere, row *order* of the outer query is only
   defined by its own `ORDER BY` (CLAUDE.md §8).
2. **Re-iterable buffer.** A materialized buffer is a stored array re-read per reference (a TS
   `Generator` would be single-shot — a two-reference CTE must observe the *same* rows twice).
3. **Meter continuity.** CTE materialization accrues into the **running statement cost**, not a
   per-CTE meter that resets the ceiling — so a `54P01` cost-limit abort during materialization
   happens at the identical accrued cost in every core.

## 6. Delivery & deferred follow-ons

**This slice:** non-recursive `WITH`, multiple CTEs with forward visibility, the column-rename
list, `[NOT] MATERIALIZED`, set-op / JOIN / aggregate bodies. No on-disk format change (a CTE is
purely a query-plan construct — no `format_version` bump).

**Deferred (unchanged from [TODO.md](../../TODO.md)):**

- **`WITH RECURSIVE`** — the iterate-to-fixpoint executor and a termination story (the `54P01`
  cost ceiling does real work there). The forward-only visibility rule (§2) is what this lifts.
- **Data-modifying CTEs** — `WITH x AS (INSERT … RETURNING …) …`.
- **`WITH` on `UPDATE`/`DELETE`**, and **nested `WITH`** inside a subquery / CTE body.
- **Inline derived-table *syntax*** (`FROM (SELECT …) AS t`) — the inline evaluation path (§3)
  builds this executor seam internally, leaving only the parser surface for a later slice.
