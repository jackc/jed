# `MERGE` — design

> `MERGE` joins a **data source** to a **target table** and, for each joined pair, runs the first
> `WHEN` clause whose condition holds: update or delete a matched target row, insert a row for an
> unmatched source row, or update or delete a target row no source row matched. The **grammar is
> authoritative** for the surface ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `merge`);
> the **error registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes (the
> feature reuses existing ones); this doc is the *why* and the exact behavior the cores reproduce
> identically (CLAUDE.md §2, §8). Behavior is PostgreSQL 18's (CLAUDE.md §1), oracle-probed, except
> the documented divergences (§11). When a decision here changes, change the grammar and this doc in
> the same edit.

`MERGE` builds on the `UPDATE`/`DELETE` target scan ([grammar.md §12](grammar.md)), the `INSERT`
row build ([constraints.md §7](constraints.md)), the end-state constraint model `INSERT … ON
CONFLICT` already uses ([upsert.md](upsert.md) §3), `RETURNING` ([grammar.md §32](grammar.md)), and
writable CTEs ([writable-cte.md](writable-cte.md)). It adds **no on-disk format change** and no new
error code or cost unit.

## 1. Surface

```sql
[ WITH with_query [, ...] ]
MERGE INTO [db.]target [ [ AS ] target_alias ]
USING source_ref ON join_condition
when_clause [ ... ]
[ RETURNING [ WITH ( { OLD | NEW } AS alias [, ...] ) ] { * | expr [ [ AS ] name ] } [, ...] ]

source_ref  ::= a single FROM item: [db.]table or view [ [AS] alias [(cols)] ]
              | ( query ) [AS] alias [(cols)]  | ( VALUES … ) [AS] alias [(cols)]
              | table_function(…) [alias]      | a CTE name
              | a join of FROM items: a JOIN b ON …  (optionally parenthesized, optionally aliased)

when_clause ::= WHEN MATCHED [ AND condition ] THEN { merge_update | DELETE | DO NOTHING }
              | WHEN NOT MATCHED BY SOURCE [ AND condition ] THEN { merge_update | DELETE | DO NOTHING }
              | WHEN NOT MATCHED [ BY TARGET ] [ AND condition ] THEN { merge_insert | DO NOTHING }

merge_update ::= UPDATE SET column = { expr | DEFAULT } [, ...]
merge_insert ::= INSERT [ ( column [, ...] ) ] [ OVERRIDING { SYSTEM | USER } VALUE ]
                 { VALUES ( { expr | DEFAULT } [, ...] ) | DEFAULT VALUES }
```

- `MERGE` is a statement keyword; `MATCHED`, `SOURCE`, `TARGET`, `BY`, `THEN`, `NOTHING`, `DO`
  are **not reserved** (grammar.md §3), recognized positionally. `WHEN` already ends a `SET`
  right-hand side's `DEFAULT` form (grammar.md §16).
- The target alias may omit `AS` (PostgreSQL's `MERGE INTO t x`). It **replaces** the table's name
  as the target's qualifier everywhere in the statement; the table's own name is then `42P01`, as
  for an `INSERT` alias ([upsert.md](upsert.md) §5).
- The target and the source may not carry the same label: **`42712`** (*name "t" specified more
  than once*, PostgreSQL's code).
- At least one `WHEN` clause is required (`42601`). Within one kind, a clause **after an
  unconditional clause** of the same kind can never run: **`42601`** (*unreachable WHEN clause
  specified after unconditional WHEN clause*, PostgreSQL's). Clauses of different kinds may
  interleave in any order; each kind keeps its own written order.
- `merge_action()` is legal only in the `RETURNING` list (§7); anywhere else it is **`42601`**
  (*MERGE_ACTION() can only be used in the RETURNING list of a MERGE command*).
- `WITH` may prefix `MERGE`, and a `MERGE` may be the body of a data-modifying CTE
  ([writable-cte.md](writable-cte.md)): its `RETURNING` rows are the CTE's rows. Both behave
  exactly as for `UPDATE`.

## 2. Name visibility

The statement resolves against two relations: the **target** (labeled by its alias, else its name)
and the **source** (labeled by its alias, else its table/view/CTE/function name; an unaliased
derived table has no label and is reachable only by bare column names).

A **joined source** — `USING a JOIN b ON p ON cond`, or the join in parentheses without an alias —
is one source relation whose inner labels stay visible, exactly an unaliased parenthesized join in
a `FROM` clause ([grammar.md §15](grammar.md)): `a.x` and `b.y` qualify its columns everywhere the
source is visible, a `USING`/`NATURAL` column is one merged column (bare `k`; `*` shows it once), and
its source query is `SELECT *` over the join. The first `ON` after the join belongs to the join, the
next to `MERGE` (PostgreSQL's grammar). An **aliased** parenthesized join is an ordinary derived
table: only its alias qualifies (`a.x` is `42P01`). The target's label is checked only against the
source's own label (`42712`), never against a join's inner labels — PostgreSQL's rule. A target
aliased like one of the join's inner relations is accepted, but a qualified reference to that
label names two relations and is **`42P09`** (*table reference "a" is ambiguous*), never a silent
bind to the target — raised at the reference in the `MERGE`'s own clauses (`ON`, `WHEN`, `SET`,
`RETURNING`, a `t.*`); see §11 for the join's own `ON`. Which relation each part
of the statement sees is PostgreSQL's:

| Part | Target | Source |
|---|---|---|
| `ON` join condition | yes | yes |
| `WHEN MATCHED` condition, `UPDATE SET` right-hand sides | yes | yes |
| `WHEN NOT MATCHED [BY TARGET]` condition, `INSERT VALUES` | no | yes |
| `WHEN NOT MATCHED BY SOURCE` condition, `UPDATE SET` right-hand sides | yes | no |
| `RETURNING` | yes | yes |

A reference to the relation a part cannot see is **`42P01`** (PostgreSQL's *invalid reference to
FROM-clause entry for table "s"*). A bare column both relations carry is **`42702`** wherever both
are visible. `SET` target columns are the target's columns, unqualified (`SET t.v = …` is
`42703`, as in PostgreSQL); `INSERT`'s column list likewise names target columns.

Expressions follow the standalone statements' rules: a condition (and `ON`) must be `boolean`
(**`42804`**); aggregates are **`42803`**, window functions **`42P20`**; subqueries are allowed
and read the pre-statement snapshot; a `SET` right-hand side and an `INSERT` value take the target
column's assignment rule (`42804`, the container rules of composite.md §12, `428C9` for an
identity `GENERATED ALWAYS` column assigned other than `DEFAULT`, `42701` for a column assigned
twice, `42703` for an unknown one); an `INSERT` with more values than target columns is `42601`.

## 3. Execution — the matching pass

`MERGE` is two-phase and all-or-nothing, like every jed write: **phase 1** computes every action
and validates every new row; **phase 2** writes. Nothing phase 1 does is visible to phase 1 itself
— every read (the source, the target probes, subqueries) sees the pre-statement snapshot, so a row
`MERGE` inserts or updates is never matched again, exactly as in PostgreSQL, whose join is
computed against the statement snapshot.

**The source.** The source is evaluated once, first, as the query `SELECT * FROM source_ref`: the
rows, their order, and the cost are exactly that query's ([cost.md](cost.md) §3). The rows are
buffered; the buffer is charged to the query-memory account like a materialized CTE buffer
([memory.md](memory.md)).

**The target probe.** The join condition is used twice. As written, over the combined row
`[target | source]`, it decides whether a target row matches. Separately, the cores plan one
**probe** of the target from it — the `UPDATE`/`DELETE` target access path
([estimator.md](estimator.md) §9.3) selected for the condition with each source column treated as a
per-row constant (a correlated outer reference). The probe narrows which target rows are read; it
never decides a match. For each source row, in source order:

1. the probe runs with that source row's values, charging exactly what an `UPDATE` scan with that
   filter charges: the access path's page and decompression units, `storage_row_read` per entry,
   and the cost ceiling checked per entry;
2. each entry's touched columns (§9) are materialized, and the join condition is evaluated over
   `[target | source]` — metered as a `WHERE` filter; only `TRUE` matches;
3. each matching target row, in the probe's emission order, is a **matched pair**, processed
   (§4) before the next entry;
4. if no entry matched, the source row is a **not-matched** source row (§4).

A `NULL`-bearing or absent join never matches: such a source row is not matched.

**The hash form.** When the probe's selected path is a full scan, the matching loop may instead
take a **hash** form, the deterministic in-memory hash join of [cost.md](cost.md) §3 "JOIN" with the
target as its build side. It applies when the join condition passes that join's structural
non-trapping gate (bare-column comparisons; a trapping or function conjunct keeps the nested loop),
at least one top-level conjunct equates a target column with a source column of the same keyable
type, and its estimate is strictly cheaper than the nested loop's ([estimator.md](estimator.md)
§9.4). The choice is made once, at plan time, so EXPLAIN shows the form execution runs. In the hash
form:

1. before the first source row, one full scan of the target in storage-key order charges the
   access path's page and decompression units and `storage_row_read` per row, materializes each
   row's touched columns, and builds the table over the key columns (`hash_build`); then
2. for each source row, in source order, the probe charges `hash_probe` (per key component, and
   per bucket entry compared), and each exact-key candidate, in scan order, has the complete join
   condition evaluated and, when `TRUE`, is a matched pair processed as in step 3.

A key with a `NULL` component is neither inserted nor probed. The matched pairs and their order are
the nested loop's; only the cost differs.

**Not matched by source.** Only when the statement has a `WHEN NOT MATCHED BY SOURCE` clause,
after every source row: one full scan of the target in storage-key order (the access path of an
unfiltered `UPDATE`, charged the same way); each row that no source row matched in step 2 is a
**not-matched-by-source** row, processed in scan order. A matched row is skipped (it was still
read and charged).

## 4. Choosing and planning an action

For a matched pair, the `WHEN MATCHED` clauses are tried in written order; for a not-matched
source row, the `WHEN NOT MATCHED [BY TARGET]` clauses; for a not-matched-by-source row, the `WHEN
NOT MATCHED BY SOURCE` clauses. The first clause with no condition, or whose condition evaluates
`TRUE`, is chosen (each tried condition is metered). If none is chosen, or the chosen clause is
`DO NOTHING`, the pair does nothing. Conditions and expressions always read the **pre-statement**
target row — never the new version an earlier action planned (PostgreSQL's join row).

**The second-affect rule (`21000`).** A target row may be updated or deleted at most once. When a
pair chooses `UPDATE` or `DELETE` for a target row an earlier pair already chose `UPDATE` or
`DELETE` for, the statement fails **`21000`** (*MERGE command cannot affect row a second time*,
PostgreSQL's code and message). A pair that does nothing — no clause, a false condition, `DO
NOTHING` — affects nothing, so a target row matched by several source rows is legal as long as at
most one of them acts on it (oracle-probed).

The chosen action is planned in phase 1, in the per-row order of the standalone statements:

- **`UPDATE`** — each `SET` right-hand side is evaluated over the pair's combined row
  (`[target | source]`, the source half all-`NULL` for a not-matched-by-source row) and stored
  through the assignment coercion (`22003`, `22001`, the decimal typmod); `SET c = DEFAULT`
  evaluates the column's default per updated row ([constraints.md](constraints.md) §2); `NOT NULL`
  (`23502`) over the whole new row; every `CHECK` (`23514`). Assigning a primary-key column
  **re-keys** the row (as a standalone `UPDATE`, constraints.md §6.5). An unassigned large value is
  carried in its stored form (large-values.md §15).
- **`DELETE`** — the row is recorded.
- **`INSERT`** — the `VALUES` expressions are evaluated over the source row, then the row is built
  exactly as an `INSERT` row: omitted columns take their defaults (expression defaults evaluated per
  row), `OVERRIDING` governs identity columns (sequences.md §13), the store coercion, `NOT NULL`,
  and `CHECK`. `DEFAULT VALUES` builds an all-default row.

Phase 1 thus produces one ordered **action list** — inserts, updates, and deletes interleaved in
the order the pairs were processed. That order is the order of every later step.

## 5. End-state validation

After the matching pass, still before any write, the new rows are validated against the
statement's **end state** — the same model as `UPDATE` and `ON CONFLICT`
([upsert.md](upsert.md) §3, the documented end-state divergence §11). The **vacated** storage keys
are the old keys of every updated row and the keys of every deleted row; the **placed** rows are
every inserted row and every updated row's new version, in action-list order. In this order:

1. **Primary key.** Each placed row's storage key must differ from every other placed row's and
   from every stored key that is not vacated: else **`23505`** on `<table>_pkey`, reported at the
   first placed row (in action-list order) that collides with a stored row or an earlier placed
   row. So `MERGE` may delete key 5 and insert key 5, or swap two keys by updates.
2. **Unique indexes**, in index-name byte order: each placed row's entry prefix (a `NULL`-bearing
   key or a row outside a partial index has none) must differ from every other placed row's and
   from every stored entry whose row is not vacated: else **`23505`** on that index.
3. **Exclusion constraints** ([gist.md](gist.md) §7): each placed row against the stored rows that
   are not vacated and against the other placed rows: else **`23P01`**.
4. **Foreign keys, child side** ([constraints.md](constraints.md) §6.4): every inserted row, and
   every updated row whose clause assigned one of the key's columns, must reference an existing
   parent row — the committed parent state, or, for a self-reference, a placed row. Else
   **`23503`** (queued to the end of the statement under a data-modifying `WITH`,
   writable-cte.md §7).

Then the disposition plan of every placed row is computed and its compression attempts are
metered (`value_compress`), and the cost ceiling is checked — the last point a `54P01` can abort
before a write.

## 6. Phase 2 — writes

Every action is applied, in this fixed order (each step in action-list order), so the copy-on-write
dirty set — and with it the committed bytes — is identical in every core:

1. remove each deleted row, and each re-keyed updated row's old key, from the table;
2. place each inserted row (a table without a primary key allocates its rowid here) and each
   updated row — a re-keyed row inserted at its new key, any other replaced in place;
3. for each index, in the table's index order: first every removed entry (a deleted row's entries
   and the entries an update moves away), then every added entry (an inserted row's entries and the
   entries an update moves to). An update moves only the entries whose key changed.

The referential actions then run against the new state, as a standalone statement's do
([constraints.md](constraints.md) §6.5/§6.6): first the deleted rows' `ON DELETE` actions, then the
updated rows' `ON UPDATE` actions, each over its rows in action-list order. The statement's
affected-row count is the number of inserted, updated, and deleted rows (PostgreSQL's `MERGE n`).

## 7. `RETURNING`

`RETURNING` projects one row per action, in action-list order (row order is unspecified to the user
— the corpus compares `rowsort`). The scope holds the **source** relation and the **target**
relation, in that order, so `*` is the source's columns followed by the target's (PostgreSQL's
order, probed). For each action:

| Action | target columns (bare / qualified) | `old.` | `new.` | source columns |
|---|---|---|---|---|
| `INSERT` | the inserted row | all `NULL` | the inserted row | the source row |
| `UPDATE` | the new row | the pre-update row | the new row | the source row (all `NULL` for not-matched-by-source) |
| `DELETE` | the deleted row | the deleted row | all `NULL` | the source row (all `NULL` for not-matched-by-source) |

`merge_action()` returns the action as `text`: `'INSERT'`, `'UPDATE'`, or `'DELETE'`. The `old` /
`new` qualifiers and `WITH (OLD AS o, NEW AS n)` follow grammar.md §32: an explicit alias hides
the standard name, and an unaliased `old`/`new` is suppressed when the target or source already
holds that label. The projection runs after phase-1 validation and before phase 2, so a subquery
in the list reads the pre-statement snapshot and a `54P01` writes nothing. Each returned row
charges `row_produced` plus its items' evaluation (cost.md §3).

## 8. Targets

The target is a **base table** — in `main`, `temp`, or an attached database (`MERGE INTO
reports.t …`) — or an **automatically updatable view**, which makes the statement the equivalent
`MERGE` into its base table ([views.md](views.md) §15.8): the view's `WHERE` limits which rows are
target rows, its check option checks every inserted and updated row, and each clause's columns are
the view's. A catalog relation is `42809`, a read-only attachment `25006`, and a missing table
`42P01`, as for `UPDATE`. A collated key column whose collation is version-skewed is `XX002`
(collation.md §12).

## 9. Cost

`MERGE` adds no cost unit. Its total is, in order: the source query (§3); per source row, the
probe's access-path units, `storage_row_read` per entry, and the join condition — or, in the hash
form, the one full scan and `hash_build` before the first source row, then per source row
`hash_probe` and the join condition per candidate; the
not-matched-by-source scan when present; every tried `WHEN` condition; each action's `SET`,
`VALUES`, default, and `CHECK` evaluation (`operator_eval`, `decimal_work`, …); the touched large
values' `value_chain`/`value_decompress` units; `value_compress`; and `RETURNING`. Validation probes
and phase-2 writes are unmetered, as for every write. The **touched set** of a target row is every
target column any part of the statement reads — the join condition, every `MATCHED` and `NOT
MATCHED BY SOURCE` condition and `SET` right-hand side, and `RETURNING` (target, `old.`, and
`new.` references). A `max_cost` ceiling (`54P01`) is checked per probed entry, per source row, and before
phase 2, so it trips at the same point in every core.

## 10. Privileges

As in PostgreSQL (session.md §5.3): `MERGE` needs `SELECT` on the target (its join condition
reads it), `INSERT` / `UPDATE` / `DELETE` on the target for each kind of action a clause names (a
`DO NOTHING` clause needs nothing), `SELECT` on every relation the source reads, and what each
expression's sublinks and functions need.

## 11. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **End-state, not per-row, uniqueness** — as `UPDATE` and `ON CONFLICT` (upsert.md §9): a swap of
  unique values or keys between two updated rows, or an insert of a key another action deletes or
  moves away, succeeds; PostgreSQL's per-row check can fail on the transient state. When several
  rows violate constraints, which one is reported follows §5's order.
- **A column assigned twice is `42701`**, the standalone `UPDATE`'s code; PostgreSQL's is `42601`.
- **The join condition must be `boolean`** (`42804`). PostgreSQL does not coerce a `MERGE` join
  condition and accepts, for example, `ON 1`.
- **`SELECT` on the target is always required** (§10); PostgreSQL requires it only for the target
  columns a condition or expression reads.
- **A target alias colliding with a joined source's inner label** is `42P09` at any qualified
  reference in the `MERGE`'s own clauses (§2), as in PostgreSQL. PostgreSQL also raises it for
  such a reference inside the join's own `ON` condition (its target is in scope when the join is
  analyzed); jed plans the joined source before the target exists, so a reference there binds the
  inner relation.
- **Deterministic order.** PostgreSQL's action order is whatever its join plan produces; jed's is §3's
  (source order, then each source row's matches in probe order, then not-matched-by-source rows in
  storage-key order). It is observable only through which error is reported first and through the
  sequence values defaults draw.

## 12. Narrowings and deferred follow-ons

Multi-column assignment `SET (a, b) = (…)` is not in `UPDATE` either, and is a `42601` syntax
error until its own slice.

`EXPLAIN [ANALYZE] MERGE` renders the matching loop ([explain.md](explain.md) §5, estimates in
[estimator.md](estimator.md) §9.4).
