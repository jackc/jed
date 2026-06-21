# Recursive common table expressions (`WITH RECURSIVE`) — design

> The reasoning behind `WITH RECURSIVE`. The grammar is authoritative
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) `with_clause` / `cte`); the non-recursive
> foundation this lifts is [cte.md](cte.md); the cost contract lives in [cost.md](cost.md) §3.
> This doc is the *why*. When a decision here changes, change it in the grammar / corpus in the
> same edit, and update [CLAUDE.md](../../CLAUDE.md) / [TODO.md](../../TODO.md) if it revises a
> load-bearing commitment.

`WITH RECURSIVE` lets a common table expression **reference itself**, turning the named relation
into an iterate-to-fixpoint computation — graph reachability, hierarchy walks, running series:

```sql
WITH RECURSIVE c(n) AS (
    SELECT 1                              -- the non-recursive (anchor) term
  UNION ALL
    SELECT n + 1 FROM c WHERE n < 5       -- the recursive term (references c)
)
SELECT n FROM c          -- → 1, 2, 3, 4, 5
```

This builds directly on the non-recursive CTE slice ([cte.md](cte.md)). Everything there still
holds — a CTE is a named, statement-local relation; the column-rename list; the buffer/scope
machinery — and `WITH RECURSIVE` lifts exactly **one** rule: the forward-only visibility that made
a self-reference a `42P01` ([cte.md](cte.md) §2). The rest of this doc is the recursion on top.

## 1. Surface

```
with_clause ::= "WITH" "RECURSIVE"? cte ("," cte)*
```

`RECURSIVE` is a flag on the whole `WITH` list (PostgreSQL). It does **not** force any CTE to be
recursive; it *enables* self-reference. A CTE in a `RECURSIVE` list that does not reference its own
name is an ordinary non-recursive CTE, planned and run exactly as in [cte.md](cte.md). A CTE that
*does* reference itself is **recursive** and must take the well-formed shape below.

A **recursive** CTE's body must be a top-level `UNION` or `UNION ALL`:

```
( non_recursive_term UNION [ALL|DISTINCT] recursive_term )
```

- The **non-recursive term** (the `UNION`'s left side) does **not** reference the CTE. It seeds the
  computation. It may itself be any non-self-referencing query — including a further `UNION` (a
  left-deep `a UNION ALL b UNION ALL <recursive>` makes `a UNION ALL b` the combined anchor).
- The **recursive term** (the `UNION`'s right side) references the CTE **exactly once**, as a direct
  `FROM`/`JOIN` relation. It is re-evaluated against the rows the previous iteration produced.
- **`UNION ALL`** keeps every row; **`UNION`** (≡ `UNION DISTINCT`) discards rows that duplicate any
  row already emitted (which is also what makes a cyclic graph walk terminate).

The optional **column-rename list** (`c(n)`) and `[NOT] MATERIALIZED` hint are parsed as for any
CTE. The hint is **accepted but inert** on a recursive CTE: a recursive CTE is **always
materialized** (the working-table algorithm requires it), matching PostgreSQL, which honors neither
`MATERIALIZED` nor `NOT MATERIALIZED` as a behavior change there.

## 2. The column types come from the non-recursive term

A recursive CTE's output column types are fixed by the **non-recursive term alone**. The recursive
term must produce columns **assignable to** those anchor types — a literal adapts, an exactly-equal
type passes, and a *wider* type is rejected. This is **not** the symmetric type unification a plain
`UNION` does (where `int UNION bigint` widens to `bigint`): the anchor type is the fixed target.

| Anchor | Recursive term | Result | Why |
|---|---|---|---|
| `int` | `int` | `int` | equal |
| `decimal` | `int` | `decimal` | `int` is assignable to `decimal` |
| `int` | `bigint` | **`42804`** | `bigint` does not fit the fixed `int` column |

The `42804` message matches PostgreSQL: *recursive query "c" column 1 has type integer in
non-recursive term but type bigint overall*. Mechanically jed computes the would-be `UNION` unified
type of the two and requires it to **equal** the anchor type; any widening of the anchor is the
error.

> jed's bare integer literal defaults to `i64` (a pre-existing documented divergence —
> [types.md](types.md) §6), so `SELECT 1` seeds an `i64` column where PostgreSQL would use
> `integer`. This affects only the render *width* of an all-literal recursion, never a value or a
> row count; a recursion seeded from a typed base-table column takes that column's exact type.

## 3. Visibility — self, not forward

`WITH RECURSIVE` changes one thing in the [cte.md](cte.md) §2 scope rules: a recursive CTE's own
name **is** in scope inside its own body (so the recursive term resolves the self-reference to the
CTE's synthetic relation, whose columns are the anchor's). Every other rule is unchanged — earlier
CTEs are visible, the catalog is shadowed, a body is an independent (non-correlated) query.

**Mutual recursion is not supported.** A reference to a *later* CTE in the same list stays out of
scope and resolves to `42P01` (the forward-only rule). PostgreSQL detects the mutual-recursion case
specifically and raises `0A000` *mutual recursion between WITH items is not implemented*; jed
surfaces the same unsupported case as `42P01` instead — a **documented divergence** on the error
code for a case neither engine executes.

## 4. The fixpoint algorithm

A recursive CTE is materialized by iterating to a fixpoint, the PostgreSQL working-table method:

1. Evaluate the **non-recursive term**. These rows are the initial **result** and the initial
   **working table**. For `UNION` (distinct), de-duplicate them and seed a **seen** set.
2. While the working table is **non-empty**, repeat:
   a. Evaluate the **recursive term** with the CTE's self-reference bound to the **current working
      table** (not the full result). 
   b. Coerce the produced rows to the anchor column types.
   c. **`UNION ALL`**: append all produced rows to the result; the produced rows become the next
      working table. **`UNION`**: keep only rows not already in *seen*; append those to the result
      and to *seen*; the kept rows become the next working table.
   d. Accrue cost and check the ceiling (§5).
3. The CTE's materialized buffer — what the main query and later CTEs scan — is the full
   **result**.

The crux, and the one place a recursive CTE differs from a plain materialized one: **the
self-reference inside the recursive term reads the working table** (the previous iteration's
output), while references **everywhere else** (the main query, a later CTE, even a non-recursive
reference in the recursive term — there is none, by the once-only rule) read the full accumulated
result. jed implements this by pointing the CTE's buffer slot at the working table for the duration
of each recursive-term evaluation, then restoring it to the full result for the outer scans. Because
the self-reference is, mechanically, a materialized-CTE buffer scan, it charges `cte_scan_row` per
working-table row through the existing cost path — no new cost unit (§5).

## 5. Termination is the cost ceiling

jed sets **no fixed iteration cap**. A recursion with no terminating condition (a `UNION ALL` whose
recursive term always produces a row) runs until the **per-statement cost ceiling** (`max_cost`)
trips `54P01` — exactly the §13 untrusted-query mechanism *doing real work*. On an unlimited handle
(`max_cost = 0`) such a recursion loops forever, precisely as PostgreSQL does. The host serving
untrusted SQL sets a ceiling; that is what bounds the recursion.

For this to be safe and cross-core-identical, the meter is **continuous across iterations**: each
iteration's cost accrues into one running total and the ceiling is checked at the iteration
boundary (and within an iteration by the ordinary per-row guards). A non-terminating recursion of
cheap iterations therefore still trips `54P01` — at the **same accrued cost and the same iteration
in every core** (the iteration count and per-iteration working-table sizes are deterministic, so the
accrued total is too). The abort is the deterministic-cost contract (cost.md §6, CLAUDE.md §13)
applied to recursion.

**Cost is reused, not invented.** A recursive CTE's cost is: the anchor's intrinsic cost, plus, per
iteration, the recursive term's intrinsic cost (its working-table scan charges `cte_scan_row` per
working row; its output charges `row_produced`; any base-table reads charge `page_read` /
`storage_row_read`), plus, per outer reference, `cte_scan_row` per result row. The `# cost:` corpus
directive pins these cross-core ([cost.md](cost.md) §3).

## 6. Errors

The structural well-formedness of a recursive CTE is validated on the parsed AST before planning,
the way PostgreSQL's `checkWellFormedRecursion` does. A CTE that does not reference itself skips all
of this (it is non-recursive). For one that does:

| Condition | Code | Notes |
|---|---|---|
| Body is not a top-level `UNION [ALL]` | `42P19` | *recursive query "c" does not have the form non-recursive-term UNION [ALL] recursive-term* |
| Self-reference in the non-recursive (anchor) term | `42P19` | *recursive reference to query "c" must not appear within its non-recursive term* |
| Self-reference appears more than once | `42P19` | *recursive reference to query "c" must not appear more than once* |
| Self-reference inside an expression subquery (sublink) | `42P19` | *…must not appear within a subquery* |
| Self-reference on the nullable side of an outer join | `42P19` | *…must not appear within an outer join* |
| Aggregate in the recursive term | `42P19` | *aggregate functions are not allowed in a recursive query's recursive term* |
| `ORDER BY` / `LIMIT` / `OFFSET` on a recursive body | `0A000` | Not implemented (PostgreSQL also `0A000`). |
| Recursive term is itself a set operation | `0A000` | jed narrowing — PostgreSQL sometimes allows; deferred. |
| Self-reference inside a `FROM` derived table | `0A000` | jed narrowing — PostgreSQL allows a `FROM (… c …)`; deferred. |
| Mutual recursion (reference to a later CTE) | `42P01` | Surfaces via forward-only visibility (§3); PostgreSQL uses `0A000`. |
| Recursive-term column wider than the anchor's | `42804` | §2; matches PostgreSQL. |

`42P19` (`invalid_recursion`) is registered in [../errors/registry.toml](../errors/registry.toml).

## 7. Cross-core determinism

The same three contract points as [cte.md](cte.md) §5, plus recursion-specific ones:

1. **Result order** is the concatenation of the anchor rows then each iteration's rows in body-plan
   output order — deterministic and byte-identical cross-core, like any materialized buffer. The
   *main query's* row order is still defined only by its own `ORDER BY` (CLAUDE.md §8).
2. **`UNION` de-duplication** uses the NULL-safe row equality every `DISTINCT`/`UNION` uses, against
   the full accumulated *seen* set (a row equal to any earlier result row, in any iteration, is
   dropped) — identical across cores.
3. **Iteration count and abort point** are functions only of the data and the body plan, so a
   `54P01` abort fires at the identical accrued cost in every core (§5).

## 8. Delivery & deferred follow-ons

**This slice:** `WITH RECURSIVE` for a single self-referencing CTE of the form *anchor UNION [ALL]
recursive-term*, the recursive self-reference as a direct `FROM`/`JOIN` relation (joinable to base
tables and earlier CTEs), `UNION`/`UNION ALL` semantics, the anchor-fixed column types (`42804`),
the structural `42P19` checks, cost-ceiling termination, and ordinary (non-self-referencing) CTEs in
a `RECURSIVE` list. No on-disk format change (a CTE is a query-plan construct — no `format_version`
bump).

**Deferred (each its own slice):**

- **`SEARCH` / `CYCLE` clauses** — PostgreSQL's breadth/depth ordering and cycle-detection sugar.
- **Recursive reference inside a `FROM` derived table** or **a set-operation recursive term**
  (`0A000` today; PostgreSQL allows both).
- **Mutual recursion** between WITH items (`0A000` in PostgreSQL; `42P01` in jed today).
- **`ORDER BY` / `LIMIT` in a recursive query** (`0A000` in both engines today).
- The inherited [cte.md](cte.md) §6 follow-ons — data-modifying CTEs, `WITH` on `UPDATE`/`DELETE`,
  nested `WITH`.
