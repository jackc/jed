# `INSERT ... ON CONFLICT` (UPSERT) — design

> The `ON CONFLICT` clause turns a uniqueness violation from an error into a controlled
> action: skip the offending row (`DO NOTHING`) or update the existing conflicting row
> (`DO UPDATE`). The **grammar is authoritative** for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `on_conflict`); the **error
> registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes (no new
> code — the feature reuses existing ones); this doc is the *why* and the precise behavior
> the three cores reproduce identically (CLAUDE.md §2, §8). Everything here is
> oracle-probed against PostgreSQL 18 (CLAUDE.md §1) except the documented divergences
> (§9). When a decision here changes, change the grammar and this doc in the same edit.

`ON CONFLICT` sits between `INSERT`'s source and its `RETURNING` clause
([grammar.md §46](grammar.md)), building directly on the UNIQUE-index enforcement
([indexes.md §8](indexes.md), [constraints.md §5](constraints.md)) and the `INSERT`/`UPDATE`
two-phase machinery ([grammar.md §12/§32](grammar.md)). It adds **no on-disk format change**
(pure DML/execution — no catalog, value-codec, or `format_version` change; no golden/Ruby
move) and **no new error code** (matching is by code — §8).

## 1. Surface

```sql
INSERT INTO t [(cols)] { VALUES ... | SELECT ... }
  ON CONFLICT [ conflict_target ] conflict_action
  [ RETURNING ... ]

conflict_target ::= ( col [, ...] )            -- index inference (column SET)
                  | ON CONSTRAINT name          -- a named unique index, or <table>_pkey

conflict_action ::= DO NOTHING
                  | DO UPDATE SET col = expr [, ...] [ WHERE condition ]
```

- `ON`, `CONFLICT`, `DO`, `NOTHING`, `EXCLUDED` are **not reserved** (grammar.md §3),
  recognized positionally. The clause follows the source and precedes `RETURNING`.
- Both INSERT sources take the clause (`VALUES` and `INSERT ... SELECT`).
- The conflict_action's `SET` assignments and `WHERE` reuse the `UPDATE` `assignment` /
  `where_clause` productions; **`excluded`** is a pseudo-relation naming the row *proposed
  for insertion* (after column-list / `DEFAULT` fill-in and coercion). A **bare** or
  **table-qualified** column reference in `SET`/`WHERE` names the **existing** (conflicting)
  row; `excluded.col` names the proposed row.

## 2. The arbiter (conflict target) and which conflicts it covers

The **arbiter** is the specific uniqueness constraint whose violation triggers the action.
A constraint here is a **unique index** ([indexes.md §8](indexes.md) — a `UNIQUE`
constraint *is* one, constraints.md §5) **or the primary key**. (A plain, non-unique index
is never an arbiter.)

**Resolution (at plan time, before execution — so an arbiter error precedes any `23505`):**

- **`( col [, ...] )` — index inference.** Each named column must exist (`42703` otherwise).
  The arbiter is the unique index / primary key whose **key-column set equals** the named
  set — **order-independent** (`ON CONFLICT (b, a)` matches `UNIQUE (a, b)`, probed). If no
  unique index / PK has exactly that column set → **`42P10`** (*there is no unique or
  exclusion constraint matching the ON CONFLICT specification*). When more than one matches
  (jed folds identical-list uniques, so this is rare), the **primary key wins**, then unique
  indexes in ascending lowercased-name order (the catalog's standing deterministic order,
  constraints.md §5.4).
- **`ON CONSTRAINT name`.** Case-insensitively (CLAUDE.md §1): the synthesized
  **`<table>_pkey`** names the primary key (jed does not persist that name but accepts it as
  an arbiter spelling — symmetric with the `23505` message, constraints.md §5.4); otherwise
  a **unique index** of that name. A name matching neither — including a *non-unique* index —
  is **`42704`** (*constraint "name" for table "table" does not exist*).
- **No target.** Legal **only** with `DO NOTHING`, where the arbiter is **any** uniqueness
  constraint: the row is skipped if it conflicts on the PK or *any* unique index. `DO UPDATE`
  with no target is **`42601`** (*ON CONFLICT DO UPDATE requires inference specification or
  constraint name*) — PostgreSQL's message, raised at plan time.

**The arbiter only arbitrates its own constraint.** A conflict on a *different* uniqueness
constraint than the arbiter is **not** caught by `ON CONFLICT` — it traps **`23505`** as a
normal violation (probed: `ON CONFLICT (id) DO UPDATE` on a row whose `email` duplicates an
existing row's still errors on `t_email_key`). This holds for `DO NOTHING` *with a target*
too (a non-arbiter conflict is `23505`, not a skip — probed). Only the no-target `DO NOTHING`
treats *every* constraint as an arbiter.

## 3. The conflict model — two-phase, sequential planning

`INSERT` is two-phase / all-or-nothing (grammar.md §12): every row is validated before any
is written. `ON CONFLICT` keeps that contract and adds a **sequential planning** pass so a
later proposed row observes earlier ones' effects — reproducing PostgreSQL's row-at-a-time
visibility while still writing nothing until all rows are planned and validated.

**Phase 1 walks the candidate rows in source order**, maintaining:

- the set of **arbiter keys already proposed** by this statement (for the §4 second-affect
  rule);
- the planned **inserts** and **updates** (keyed by storage key), forming an overlay on the
  committed snapshot that later rows' conflict probes consult.

For each candidate row `R` (built with defaults filled, coerced, `NOT NULL` `23502` and
`CHECK` `23514` applied first — the existing per-row order, constraints.md §4.4):

1. Compute `R`'s **arbiter key** `ak` (the arbiter index/PK prefix over `R`'s values). A
   **NULL-bearing** `ak` (a nullable arbiter column is NULL) never conflicts (NULLS DISTINCT,
   indexes.md §8) — `R` is an ordinary insert (step 4), never a duplicate-proposed (step 2).
2. **`ak` already proposed by this statement?** (a second candidate row with the same arbiter
   key):
   - `DO UPDATE` → **`21000`** (*ON CONFLICT DO UPDATE command cannot affect row a second
     time*). This fires on *duplicate proposed arbiter keys* regardless of whether either row
     actually conflicts with a committed row, and **regardless of a `WHERE` that would skip
     the update** (probed). 
   - `DO NOTHING` → **skip** `R`.
3. Else record `ak` as proposed, then **look up `ak` in the committed snapshot + the overlay**:
   - **Conflict with an existing row `E`:**
     - `DO NOTHING` → **skip** `R`.
     - `DO UPDATE` → evaluate the optional `WHERE` against `[E | excluded=R]`; if **false**,
       skip the update (`E` unchanged, not returned by `RETURNING`); if true/absent, apply
       the `SET` to `E` (§5) → `E'`, and plan an **update** of `E`'s storage key.
   - **No conflict on the arbiter** → `R` is an **insert**. It still passes the ordinary
     PK + every-unique-index validation (against committed + overlay), so a conflict on a
     **non-arbiter** constraint traps **`23505`** (§2). Plan an **insert**.

**Phase 2** applies the planned inserts and updates to the store and maintains every index,
exactly as `INSERT`/`UPDATE` phase 2 do. A ceiling abort (`54P01`) or any validation error
in phase 1 leaves the database untouched (all-or-nothing).

**End-state uniqueness (the documented divergence shared with `UPDATE`).** Planned updates
are validated for uniqueness against the **statement end state** — an index entry belonging
to a row being rewritten does not conflict with that row's new values, and a value *swap*
across two updated rows succeeds — exactly as `UPDATE` (indexes.md §8, the §9 divergence).
The no-target `DO NOTHING` skip decision and the arbiter conflict probe likewise consult the
overlay (committed + planned), not a per-row transient.

## 4. The second-affect rule (`21000`)

PostgreSQL forbids a single `INSERT ... ON CONFLICT DO UPDATE` from affecting the same row
twice. jed detects this as **two proposed rows sharing one arbiter key** (§3 step 2):

- it fires whether the first proposal was an insert or an update of an existing row (probed:
  `VALUES (5,…),(5,…)` with neither `5` existing → `21000`; with `5` existing → `21000`);
- it fires **before** the `DO UPDATE` `WHERE` is consulted, so a `WHERE false` does not
  suppress it (probed);
- it is specific to `DO UPDATE`. Under `DO NOTHING` a duplicate proposed arbiter key is
  simply **skipped** (probed: `VALUES (6,1),(6,2) ON CONFLICT (id) DO NOTHING` keeps
  `(6,1)`), which is why `DO NOTHING` needs no target to be well-defined.

A NULL-bearing arbiter key is exempt (two `(NULL)` proposals do not "affect the same row").

## 5. `DO UPDATE` — the `SET`/`WHERE` scope and assignment

The `SET` assignments and `WHERE` resolve against a **two-relation scope** over the combined
row `[existing | proposed]`:

- the **target table** at offset 0 — a bare column (`v`) or table-qualified column (`t.v`)
  reads the **existing** conflicting row;
- the **`excluded`** pseudo-relation at offset *n* — **qualifier-only** (like `old`/`new` in
  `RETURNING`, grammar.md §32): `excluded.v` reads the **proposed** row; bare `excluded` is
  an ordinary identifier (never the pseudo-relation), and a target table literally named
  `excluded` shadows it (the same rule `old`/`new` follow).

Each `SET col = expr` is resolved, type-checked assignable to `col` (`42804` otherwise),
evaluated against `[E | R]`, and coerced through the same `store_value` chokepoint `UPDATE`
uses (range `22003`, `NOT NULL` `23502`, decimal typmod). The post-assignment row `E'` is
then validated like any `UPDATE` row: every `CHECK` (`23514`), end-state uniqueness over the
non-arbiter unique indexes (`23505`), and the FK child-side (`23503`).

**Narrowings on `SET` (each relaxable, §9):** assigning a **primary-key column** is
**`0A000`** — a deferred follow-on (the standalone `UPDATE` re-keying has landed, CLAUDE.md §11
step 6, but extending it to the conflict path is separate); assigning a
**`GENERATED ALWAYS AS IDENTITY`** column is **`428C9`** (the standing
`UPDATE` rule, sequences.md §13); `SET col = DEFAULT` is not supported on this conflict-action
path (standalone UPDATE now supports it, but this RHS remains a general expression, and `DEFAULT`
is not reserved (grammar.md §3), so a bare `DEFAULT` there resolves as a column reference →
**`42703`**, a documented divergence from PG, which supports `SET col = DEFAULT`). An unknown
`col` is also `42703`.

## 6. `RETURNING`

`RETURNING` binds to the outer `INSERT` and projects each **affected** row — the **inserted**
rows and the **updated** rows — using the ordinary returning scope (grammar.md §32). Rows
**skipped** by `DO NOTHING`, or by a `DO UPDATE` `WHERE` that evaluated false, contribute
**nothing** (probed). `excluded` is **not** visible in `RETURNING` (only in the
conflict_action) — there, the PG-18 `old`/`new` qualifiers apply: for an updated row
`old` is the pre-update existing row and `new` the updated row; for an inserted row `old` is
the all-NULL row and `new` the inserted row. Row order is unspecified (`rowsort`).

## 7. Cost

`ON CONFLICT` adds no cost unit. The arbiter probe and the conflict lookups are **unmetered**
validation work, like the uniqueness probes they reuse (cost.md §3 "what is NOT metered").
`DO UPDATE`'s `SET`/`WHERE` expression evaluation is metered exactly as `UPDATE`'s
(`operator_eval` per interior node, `decimal_work` where decimal arithmetic fires), and
`RETURNING` charges `row_produced` plus its items' evaluation per returned row (grammar.md
§32, cost.md §3). The `max_cost` ceiling (`54P01`) bounds the `DO UPDATE` evaluation and the
`RETURNING` projection deterministically.

## 8. Error codes (all pre-existing — registry unchanged)

Matching in the conformance corpus is by **code** (conformance.md §7), so reusing a code with
an `ON CONFLICT`-specific message needs no registry edit. The codes:

| Code | When |
|---|---|
| `21000` | `DO UPDATE` would affect a row a second time (§4). Message: *ON CONFLICT DO UPDATE command cannot affect row a second time*. |
| `42601` | `DO UPDATE` with no conflict target (§2). Message: *ON CONFLICT DO UPDATE requires inference specification or constraint name*. |
| `42P10` | A column-list target matches no unique index / PK (§2). Message: *there is no unique or exclusion constraint matching the ON CONFLICT specification*. |
| `42704` | `ON CONSTRAINT name` names no unique constraint (§2). Message: *constraint "name" for table "table" does not exist*. |
| `42703` | An unknown column in the target list or `SET` (§2, §5). |
| `23505` | A conflict on a **non-arbiter** constraint (§2), or any conflict with no `ON CONFLICT` clause. |
| `23502` / `23514` / `22003` / `42804` / `23503` / `0A000` / `428C9` | The ordinary `INSERT`/`UPDATE` per-row checks, applied to proposed and updated rows (§3, §5). |

## 9. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **`DO UPDATE` cannot assign a primary-key column** (`0A000`); PostgreSQL allows it. The
  standalone `UPDATE` re-keying has landed (CLAUDE.md §11 step 6), but extending it to the
  conflict path (the existing row would move, with its own arbiter/secondary-index re-probe)
  is a separate deferred follow-on (§10).
- **End-state, not per-row transient, uniqueness** for planned updates and the overlay-based
  conflict probe — the same divergence `UNIQUE`/`UPDATE` carry (indexes.md §7), from the
  two-phase / all-or-nothing model. A value *swap* under `DO UPDATE` succeeds where PG's
  per-row check could fail on the transient.
- **Which constraint name a `23505` reports** when several are violated follows jed's standing
  deterministic order (PK first, then unique indexes by lowercased name), not PG's
  index-creation order (constraints.md §5.4). The code is identical.
- **No `INSERT INTO t AS alias`.** Table aliasing on `INSERT` is not in the grammar this
  slice, so the existing row is referenced by the table's own name (PG allows an alias). A
  relaxable narrowing.
- **No single-line `DETAIL`.** jed's messages are single-line house style; the codes match.

## 10. Deferred (each its own later slice)

`SET col = DEFAULT` on the conflict-action path; `INSERT INTO t AS alias`;
the partial-index `WHERE index_predicate` in a conflict target (jed has no partial indexes)
and `COLLATE`/opclass inference decorations; multi-column `SET (a,b) = (...)`; and the GIN /
ordered-index acceleration of the arbiter probe (the probe reuses the unique-index point/range
lookup, which is already a probe, so this is a non-issue for unique arbiters).
