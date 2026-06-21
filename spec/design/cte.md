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

**Nested `WITH` is supported (§7).** Besides the statement top level, a `WITH` may prefix a
*parenthesized* query expression — a subquery, a derived table, or another CTE's body
(`spec/design/cte.md` §7). Its CTEs are visible only inside that nested query; the enclosing
statement's CTE bindings are **not** inherited (a documented narrowing — §7). CTE *references*
inside nested subqueries (without a nested `WITH`) are fully supported (§2).

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
| Data-modifying CTE in a nested `WITH` | `0A000` | DML-`WITH` is top-level only (§7), matching PostgreSQL. |
| Enclosing CTE referenced inside a nested `WITH` | `42P01` | The nested scope does not inherit enclosing CTEs (§7) — a documented divergence from PG. |

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

- ~~**`WITH RECURSIVE`**~~ — ✅ **landed** ([recursive-cte.md](recursive-cte.md)): the
  iterate-to-fixpoint (working-table) executor, anchor-fixed column types (`42804`), the structural
  `42P19` checks, and cost-ceiling termination (the `54P01` ceiling does real work there). It lifts
  exactly the forward-only visibility rule (§2) — a recursive CTE sees its own name.
- ~~**Data-modifying CTEs**~~ — ✅ **landed** ([writable-cte.md](writable-cte.md)):
  `WITH x AS (INSERT/UPDATE/DELETE … [RETURNING …]) …`. The body/scope machinery here is reused
  unchanged; the new layer is the **shared pre-statement read pin** (every sub-statement reads the
  one snapshot, data crosses only via `RETURNING` buffers), the **always-materialize-to-completion**
  rule, and one all-or-nothing transaction.
- ~~**`WITH` on `UPDATE`/`DELETE`**~~ — ✅ **landed** with the above ([writable-cte.md](writable-cte.md)):
  the `WITH`-prefixed primary may be an `INSERT`/`UPDATE`/`DELETE`.
- ~~**Nested `WITH`** inside a subquery / CTE body~~ — ✅ **landed** (§7): a `WITH` may now prefix any
  parenthesized query expression. The one residual narrowing is enclosing-CTE *visibility* (an inner
  `WITH` does not inherit the enclosing statement's CTE bindings — §7), a documented follow-on.

**Landed since:**

- **Inline derived-table *syntax*** (`FROM (SELECT …) AS t`) — the parser surface over this slice's
  inline evaluation path (§3); a derived table is mechanically an anonymous, always-inlined
  single-reference CTE. See [grammar.md §42](grammar.md#42-derived-tables-from--query_expr--as-t).

## 7. Nested `WITH` — a `WITH` inside a subquery / derived table / CTE body

A `WITH` clause may prefix **any parenthesized query expression**, not only the statement top level:

```
SELECT * FROM (WITH r AS (SELECT 1 AS n) SELECT n FROM r) s          -- derived table
WITH outer AS (WITH inner AS (SELECT 1) SELECT * FROM inner)         -- another CTE's body
  SELECT * FROM outer
SELECT (WITH c AS (SELECT count(*) k FROM t) SELECT k FROM c)        -- scalar subquery
SELECT id FROM t WHERE n IN (WITH c AS (…) SELECT n FROM c)          -- IN / EXISTS / ANY/ALL subquery
```

A nested `WITH` is recognized by a shape-based lookahead — `WITH RECURSIVE …`, `WITH <name> ( …`, or
`WITH <name> AS …` — so `with` stays a legal identifier elsewhere (`x IN (with)` is still a value
list). It is reached at every position that already admits a parenthesized subquery: a derived table
(`FROM ( … )`), a scalar subquery, `IN`/`EXISTS`/`ANY`/`ALL ( … )`, a set-operation operand, and a
CTE body.

**Own scope, no inheritance (the one narrowing).** A nested `WITH` establishes its **own** CTE scope.
Inside it, the nested CTEs are visible — to each other (forward-only) and to the inner main query —
with the full §2/§3 machinery (duplicate name `42712`, self/forward reference `42P01`, the
column-rename list, `[NOT] MATERIALIZED`, and `WITH RECURSIVE`). But the **enclosing** statement's CTE
bindings are **not** inherited: an enclosing CTE name referenced inside a nested `WITH` resolves to a
base table (or `42P01` if none), *not* the enclosing CTE. PostgreSQL inherits them, so this is a
**documented divergence** (per-core unit tests pin it); full enclosing-scope visibility is a scoped
follow-on. Outside the nested `WITH`, the enclosing CTEs are intact — only the inner query loses
visibility of them.

**Planning & execution.** A nested `WITH` plans to a `QueryPlan::With { bindings, modes, body }`: the
nested CTE bindings (planned against each other only, via the same `plan_cte_bindings`), their
inline/materialize modes ([cost.md](cost.md) §3), and the inner body plan. At execution the node
materializes its CTEs **once**, builds a fresh CTE context over them, and runs the body — the same
materialize-then-execute shape as the top-level `run_with`, so the deterministic cost is identical
across cores. A nested `WITH` adds **no** correlation frame: its body sees the same outer-row
environment as the node's position (so a `LATERAL` derived-table body whose `WITH` wraps it still
correlates to its left siblings), while the CTE bodies stay independent (`parent = None`).

**Data-modifying CTEs stay top-level only.** A nested `WITH` whose CTE body is an
`INSERT`/`UPDATE`/`DELETE` is rejected `0A000` (`is only supported at the top level`) — this
*matches* PostgreSQL, which also restricts a data-modifying `WITH` to the outermost statement.

No on-disk format change (a nested `WITH`, like every CTE construct, is purely a query-plan
construct). Capability `query.cte_nested`.
