# Data-modifying (writable) common table expressions — design

> The reasoning behind data-modifying CTEs and `WITH` attached to a data-modifying primary
> statement. The grammar is authoritative ([../grammar/grammar.ebnf](../grammar/grammar.ebnf)
> `query_statement` / `with_clause` / `cte`); the non-recursive foundation this builds on is
> [cte.md](cte.md); `RETURNING` is [grammar.md §32](grammar.md); the cost contract lives in
> [cost.md](cost.md) §3. This doc is the *why*. When a decision here changes, change it in the
> grammar / corpus in the same edit, and update [CLAUDE.md](../../CLAUDE.md) / [TODO.md](../../TODO.md)
> if it revises a load-bearing commitment.

A **data-modifying CTE** (PostgreSQL's "writable CTE") lets a `WITH` item's body be an
`INSERT` / `UPDATE` / `DELETE` instead of a query, and lets the `WITH`-prefixed primary statement
itself be an `INSERT` / `UPDATE` / `DELETE`. The rows a data-modifying CTE feeds forward are its
`RETURNING` rows:

```sql
-- move rows from one table to another in one atomic statement
WITH moved AS (
  DELETE FROM inbox WHERE ready RETURNING *
)
INSERT INTO archive SELECT * FROM moved;

-- fan a write out to a second table, keyed on the first write's generated ids
WITH ins AS (
  INSERT INTO orders (sku) VALUES ('abc'), ('def') RETURNING id
)
INSERT INTO order_log (order_id) SELECT id FROM ins;
```

This lifts two of the [cte.md](cte.md) §6 follow-ons together (they share all the machinery):
**data-modifying CTEs** and **`WITH` on `INSERT`/`UPDATE`/`DELETE`**. Everything in
[cte.md](cte.md) still holds — a CTE is a named, statement-local relation; the column-rename
list; forward-only visibility; the buffer/scope machinery. This doc is the data-modifying layer
on top.

## 1. Surface

A CTE body, and the `WITH` primary, may now be a data-modifying statement:

```
cte_body        ::= query_expr | insert | update | delete
cte             ::= identifier ("(" ident ("," ident)* ")")? "AS"
                    ("NOT"? "MATERIALIZED")? "(" cte_body ")"
query_statement ::= with_clause? ( query_expr | insert | update | delete )
```

- A **data-modifying CTE** is a CTE whose body is an `INSERT`/`UPDATE`/`DELETE`. It may carry a
  `RETURNING` clause; if it does, the CTE is a relation of the `RETURNING` output columns (named,
  scoped, referenceable exactly like a query CTE — including the column-rename list). If it does
  **not**, the CTE produces no columns and is **side-effect-only**: it still executes (§3), but a
  `FROM` reference to it is an error (§5).
- **`WITH` on a data-modifying primary**: the statement after the `WITH` list may be an
  `INSERT`/`UPDATE`/`DELETE` (with its own optional `RETURNING`). The CTEs are visible to it
  exactly as they are to a query primary — in the `INSERT … SELECT` source, the `UPDATE`/`DELETE`
  `WHERE`, the `SET` right-hand sides, and any `RETURNING` subquery.
- **Target tables are catalog tables.** A data-modifying statement's target (`INSERT INTO t`,
  `UPDATE t`, `DELETE FROM t`) is resolved against the **catalog**, never a CTE binding. A CTE
  named `t` does **not** make `t` a writable relation; `INSERT INTO t` with no base table `t` is
  `42P01` (matching PostgreSQL — a CTE is not a modifiable relation).
- `[NOT] MATERIALIZED` on a data-modifying CTE is **accepted but inert** — a data-modifying CTE is
  always materialized (§3), like a recursive one ([recursive-cte.md](recursive-cte.md) §1).
- A data-modifying CTE is **top-level only**, exactly like every CTE this milestone: it may appear
  only in the `WITH` list directly prefixing the statement, never inside a subquery or another CTE
  body (the nested-`WITH` narrowing, [cte.md](cte.md) §1, is unchanged). PostgreSQL imposes the
  same restriction — data-modifying statements are allowed only in a `WITH` attached to the
  top-level statement.

## 2. The shared snapshot — the central semantic

Every sub-statement of a `WITH` — each data-modifying CTE, each query CTE, and the primary — reads
the **single pre-statement snapshot**. They **cannot observe each other's effects on the target
tables.** The only channel that carries data from one sub-statement to another is a CTE's buffered
output (its `RETURNING` rows, or a query CTE's rows), referenced by name. This is PostgreSQL's
behavior verbatim, oracle-probed:

```sql
-- foo starts with 3 rows
WITH t AS (INSERT INTO foo VALUES (4) RETURNING *)
SELECT count(*) FROM foo;          -- → 3, NOT 4: the main query reads the pre-statement foo
-- after the statement, foo has 4 rows
```

```sql
WITH a AS (INSERT INTO foo VALUES (5) RETURNING *),
     b AS (SELECT count(*) FROM foo)   -- b does NOT see a's insert
SELECT * FROM b;                       -- → the pre-statement count
```

So the classic *move-rows* pattern works because the `INSERT` reads the **CTE buffer** `moved`, not
the table `inbox` — both `inbox` and `archive` are observed in their pre-statement state, and
`moved` carries the deleted rows across.

**Why jed matches PG here rather than a simpler read-your-writes model.** jed is the
PostgreSQL-behavior engine (CLAUDE.md §1); the shared-snapshot rule is *observable* (the count
example above), so a sequential read-your-writes model would be a silent divergence on a behavior
users rely on, with no overriding reason. jed's immutable-snapshot architecture
([transactions.md](transactions.md) §2) makes the faithful model cheap: a single frozen read
snapshot serves every sub-statement's reads while their writes accumulate into the one working set.

### Implementation: the read pin

The statement runs inside one autocommit (or block) transaction
([transactions.md](transactions.md) §4): a `working` snapshot is forked from `committed` and every
sub-statement's writes land in it, exactly as a lone DML statement's do. The new piece is a
**read pin**: for the duration of a `WITH` statement that contains any data-modifying part, the
executor's read path is pinned to the **pre-statement snapshot** of **every database domain** — the
main database, the session-local temp domain ([temp-tables.md](temp-tables.md) §2), and each host
attachment ([attached-databases.md](attached-databases.md) §5) — a clone of each domain's read
snapshot taken before the first sub-statement runs. So:

- **Reads** — every base-table scan, point lookup, `WHERE`/`SET`/`RETURNING` subquery, and
  PK / `UNIQUE` existence probe — resolve against the **pin**, whichever database the table lives
  in. A sub-statement therefore sees neither an earlier sub-statement's inserts/updates/deletes nor
  its own (a single DML's two-phase pass already needs no read-your-writes — §6 of
  [grammar.md](grammar.md)). A key collision the pin hides is still caught when the write meets the
  earlier sub-statement's row in `working` (`23505`, §7).
- **Foreign keys are the exception: they are checked against the working state, not the pin.**
  PostgreSQL runs its RI checks as end-of-statement triggers that see the statement's writes, so a
  parent a CTE inserts satisfies a child the primary writes (`WITH p AS (INSERT INTO parent …
  RETURNING id) INSERT INTO child SELECT … FROM p`), and a parent a CTE deletes no longer satisfies
  one (`23503` — checking against the pin would commit an orphan). jed checks each sub-statement's
  child-side probes (constraints.md §6.4) and parent-side `NO ACTION`/`RESTRICT` searches and
  referential actions (§6.5–§6.6) against the transaction's working state when that sub-statement
  validates, so they see every **earlier** sub-statement's writes. See §7 for the one ordering this
  cannot reproduce.
- **Writes** accumulate into each domain's working set in sub-statement order (§3).
- The pin is cleared when the statement finishes (success or error), so the next statement reads
  normally.

For a `WITH` statement that contains **no** data-modifying part (an ordinary read-only CTE query),
nothing changes: no pin is set and [cte.md](cte.md) governs unchanged.

## 3. Execution order — deterministic, always-to-completion

PostgreSQL does not specify the order in which the data-modifying parts of a `WITH` run (only that
each runs exactly once and always to completion). jed **pins a deterministic order** — the
cross-core cost and effect contract (CLAUDE.md §10) requires it:

1. **Data-modifying CTEs execute first, in lexical (`WITH`-list) order**, each exactly once and
   **always to completion** — regardless of whether, or how many times, it is referenced. A
   data-modifying CTE is thus **always materialized**: its body runs once, its `RETURNING` rows are
   buffered, and every reference scans that buffer (charging `cte_scan_row`, [cost.md](cost.md) §3).
   A side-effect-only data-modifying CTE (no `RETURNING`) runs for its effect and buffers nothing.
2. **Query CTEs** keep their [cte.md](cte.md) evaluation rule (inline a single-reference one,
   materialize a multi-reference / `MATERIALIZED` one), but every read they do is against the pin
   (§2) — so a query CTE cannot see a data-modifying CTE's table writes either. An **inlined** query
   CTE runs in place when the primary executes; a **materialized** one runs in list order, after the
   data-modifying CTEs in front of it have already run (the buffers a later body sees are complete).
3. **The primary statement runs last**, against the same pin, and produces the statement's result
   (§4).

Lexical order respects the forward-only visibility rule ([cte.md](cte.md) §2) for free — a CTE can
reference only earlier ones, which have already run when it executes. This determinism is a
**documented divergence** from PostgreSQL's unspecified order; it is observable only when two
sub-statements write to overlapping keys/rows (§7), which PostgreSQL itself declares unsupported.

## 4. The statement result (command tag)

The `WITH` statement's result is the **primary's** result; the data-modifying CTEs' row counts are
not surfaced (PostgreSQL's command tag is the primary's):

- **Query primary** (`SELECT`/set-op) → a query result (rows), exactly as [cte.md](cte.md).
- **`INSERT`/`UPDATE`/`DELETE` primary without `RETURNING`** → a statement result reporting the
  primary's affected-row count.
- **`INSERT`/`UPDATE`/`DELETE` primary with `RETURNING`** → a query result projecting the primary's
  affected rows (the `RETURNING` rows of the *primary*, not of any CTE).

A data-modifying CTE's effect is committed regardless of the primary's shape (it always runs to
completion, §3); only its *count* is invisible.

## 5. Errors

| Condition | Code | Notes |
|---|---|---|
| `FROM` reference to a data-modifying CTE with no `RETURNING` | `0A000` | *WITH query "name" does not have a RETURNING clause* — matches PostgreSQL (it has no columns to scan). The CTE still executes for its effect. |
| Data-modifying statement targets a relation that is not a base table (e.g. a CTE name with no base table) | `42P01` | The target resolves against the catalog only (§1). A `db.table` target resolves in that database — `main`, `temp`, or a host attachment — like a standalone statement's (an unknown attachment is `42P01` *database … is not attached*). |
| Two data-modifying parts insert the same key | `23505` | A staged write colliding with an earlier staged write of the same statement (§7). Matches PostgreSQL. |
| `WITH RECURSIVE` with a data-modifying CTE | — | **Allowed.** `RECURSIVE` only *enables* self-reference ([recursive-cte.md](recursive-cte.md) §1); a non-self-referencing data-modifying CTE in a `RECURSIVE` list is an ordinary data-modifying CTE (a data-modifying body is never the `anchor UNION recursive` shape, so it is never analyzed as recursive). PostgreSQL agrees. |
| Data-modifying CTE in a nested `WITH` | `0A000` | Data-modifying CTEs remain statement-top-level only; a query-only nested `WITH` is supported with inherited enclosing CTE visibility ([cte.md](cte.md) §7). |

Every other error is inherited unchanged: a duplicate CTE name is `42712`; a self/forward reference
is `42P01`; a too-long rename list is `42P10`; the data-modifying body's own resolution/validation
(unknown column `42703`, type/range `22003`/`42804`, NOT NULL `23502`, CHECK `23514`, FK `23503`,
its own `RETURNING` rules — [grammar.md §32](grammar.md)) is exactly the standalone statement's.

## 6. Atomicity

A `WITH` statement is **one transaction** (CLAUDE.md §3). Under autocommit it is one implicit
transaction; inside a `BEGIN … COMMIT` block it is one statement of the block. Either way every
sub-statement's effect lands in the single `working` set and is published **all-or-nothing**: any
error in any sub-statement (or the primary) aborts the whole statement, and `committed` is never
touched ([transactions.md](transactions.md) §6). There is no partial application — a `WITH` that
inserts into three tables and then fails the third leaves all three unchanged.

## 7. Write/write conflicts — jed's deterministic resolution

Two data-modifying parts of the same statement can target the same key or row. PostgreSQL declares
this unsupported ("only one of the modifications takes place, but it is not easy and sometimes not
possible to reliably predict which one"). jed gives a **deterministic** resolution, since the
cross-core contract demands one; the cases split:

- **Insert/insert on the same key** (two parts insert the same primary-key / unique value) →
  **`23505`**. Each part's phase-1 validation reads the pin (§2), so neither sees the other's
  staged row; the collision is caught when the second part's phase-2 write meets the first's already
  in `working`. This **matches PostgreSQL** (which raises the same unique violation).
- **Update/update or update/delete of the same row** (PostgreSQL's genuinely-unspecified case) →
  jed applies each part's writes to `working` in **sub-statement (lexical) order, last-write-wins**.
  Each part independently computes its `RETURNING` rows from the **pin** (the pre-statement row), so
  both parts return a row, and the table's final state is the later part's write. This is a
  **documented divergence** from PostgreSQL (which applies and returns only one), justified by the
  determinism requirement on a case PostgreSQL itself leaves undefined. These cases are pinned by
  per-core unit tests, not the oracle corpus (which is PostgreSQL-clean — CLAUDE.md §10).

A delete and an insert that do **not** overlap on a key compose without conflict (the move-rows
pattern across two tables; a delete of a pre-existing key and an insert of a new key in the same
table).

### Foreign keys across sub-statements

A foreign key satisfied only by a **later** sub-statement is a known divergence, not yet removed:
jed raises `23503` where PostgreSQL accepts. jed checks a sub-statement's foreign keys when that
sub-statement validates (§2), against the writes of the sub-statements before it, while PostgreSQL
defers every RI check to the end of the whole statement. So `WITH c AS (INSERT INTO child VALUES
(5, 5)), p AS (INSERT INTO parent VALUES (5)) SELECT 1` and `WITH d AS (DELETE FROM parent WHERE
id = 1), u AS (UPDATE child SET pid = 2 WHERE pid = 1) SELECT 1` fail in jed and succeed in
PostgreSQL. Listing the sub-statement that makes the reference valid first (the parent insert before
the child insert, the child re-point before the parent delete) gives PostgreSQL's result. Deferring
these checks to the end of the statement is a tracked follow-on ([TODO.md](../../TODO.md)).

## 8. Cost

No new cost unit. A data-modifying CTE charges its **intrinsic** DML cost once into the running
statement total — `page_read` / `storage_row_read` for the rows it scans, `operator_eval` for its
expressions, `row_produced` per `RETURNING` row (exactly a standalone DML statement, [cost.md](cost.md)
§3) — and each reference to its buffer charges `cte_scan_row` per buffered row (the existing
materialized-CTE unit). The primary charges its own intrinsic cost. The meter is continuous across
the whole statement, so a `max_cost` ceiling (`54P01`) trips at the identical accrued cost in every
core (CLAUDE.md §13), and a side-effect-only data-modifying CTE still charges its scan/write
validation cost (it runs to completion regardless of references, §3).

## 9. Cross-core determinism

The [cte.md](cte.md) §5 contract points hold, plus:

1. **Sub-statement order** is the lexical `WITH`-list order, data-modifying CTEs then the primary
   (§3) — identical across cores, so the committed end state and every buffer are byte-identical.
2. **The read pin** is the same pre-statement snapshot of every database domain in every core, so
   every sub-statement reads the identical rows (§2) and a write/write conflict resolves
   identically (§7); foreign keys read the identical working state.
3. **Buffer order** for a data-modifying CTE is its `RETURNING` output order — the affected-row
   order of the underlying DML, already a cross-core contract ([grammar.md §32](grammar.md);
   unspecified to the *user* without `ORDER BY`, but byte-identical across cores, compared `rowsort`
   in the corpus).

## 10. Delivery & deferred follow-ons

**This slice:** data-modifying CTEs (`WITH x AS (INSERT/UPDATE/DELETE … [RETURNING …]) …`), `WITH`
on an `INSERT`/`UPDATE`/`DELETE` primary, the shared read pin, the always-materialize-to-completion
rule, the deterministic order and conflict resolution, and side-effect-only (no-`RETURNING`)
data-modifying CTEs. New capabilities `query.cte_data_modifying` (a data-modifying CTE feeding any
primary) and `dml.with_clause` (a `WITH` clause on a data-modifying primary). No on-disk format
change — a CTE is a query-plan construct.

**Deferred (unchanged from [cte.md](cte.md) §6 / [TODO.md](../../TODO.md)):**

- The recursive-CTE deferrals ([recursive-cte.md](recursive-cte.md) §8) — a set-op /
  `FROM`-subquery recursive term, mutual recursion. (`SEARCH` / `CYCLE` has since landed, §9.)
- `ON CONFLICT` referential-action follow-ons are inherited from their own slices unchanged; a
  data-modifying CTE may use `ON CONFLICT` exactly as a standalone `INSERT` does.
