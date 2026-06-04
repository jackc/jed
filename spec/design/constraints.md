# Column constraints — design

> The semantics of the column constraints in a `CREATE TABLE` column definition. The
> **grammar is authoritative** for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `column_def` / `column_constraint`);
> the **error registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes;
> the **on-disk format** ([../fileformat/format.md](../fileformat/format.md)) owns how a
> constraint is persisted in the catalog. This doc is the *why* and the precise behavior the
> three cores must reproduce identically (CLAUDE.md §2, §8). When a decision here changes,
> change the data/grammar and here in the same edit.

A `column_def` is a name, a type, and zero or more **column constraints**:

```
column_def        ::= identifier type_name column_constraint*
column_constraint ::= "PRIMARY" "KEY"
                    | "NOT" "NULL"
                    | "DEFAULT" literal
```

Constraints are **order-free** and idempotent: the parsers accept the keywords in any order
and a repeat is harmless (`x int NOT NULL PRIMARY KEY` ≡ `x int PRIMARY KEY NOT NULL` ≡
`x int NOT NULL NOT NULL`). The constraint keywords are **not reserved** (grammar.md §3) — a
column may be named `not`, `null`, `primary`, or `key`; the parser distinguishes them
positionally.

This grows one constraint at a time (CLAUDE.md §11, [../../TODO.md](../../TODO.md)). `UNIQUE`,
`CHECK`, `FOREIGN KEY`, and a composite `PRIMARY KEY` stay deferred.

## 1. `NOT NULL`

A `NOT NULL` column rejects a NULL value. Any attempt to store NULL into the column — written
directly, applied from a `DEFAULT NULL`, or left as NULL by an omitted column with no default
(§2) — traps **`23502`** (`not_null_violation`):

```
null value in column <name> violates not-null constraint
```

The check lives in the single value-coercion chokepoint (`store_value` in each core's
executor) that INSERT and UPDATE share, so it fires uniformly:

- **INSERT** — a NULL literal, or a NULL arrived at via a default/omission, into a `NOT NULL`
  column traps `23502`. As with the rest of INSERT this is part of the two-phase /
  all-or-nothing pass (grammar.md §12): the violation aborts the whole statement before any
  row is stored.
- **UPDATE** — assigning NULL (directly or via an expression that evaluates to NULL) to a
  `NOT NULL` column traps `23502` in UPDATE's own type-check pass, before any row is written.

**`PRIMARY KEY` implies `NOT NULL`.** A primary-key column is always non-nullable regardless of
whether `NOT NULL` is also written; the executor sets the stored flag to
`primary_key || not_null`. This is why a primary-key column never needs the NULL-key case at
storage time — an omitted or NULL primary-key value traps `23502` first.

**A column without `NOT NULL` is nullable** — it accepts and stores NULL, ordered as the
largest value (the PostgreSQL model; [types.md](types.md) §4).

**Persistence.** Nullability is one bit in the per-column catalog flags byte (`bit1 not_null`
— [../fileformat/format.md](../fileformat/format.md)); it needs no value bytes and was already
round-tripped before the explicit `NOT NULL` syntax existed (it carried the implicit
primary-key nullability).

**Cost.** Declaring or enforcing `NOT NULL` adds nothing to query cost — the check is a branch
inside an evaluation step that is already metered (CLAUDE.md §13).

## 2. `DEFAULT`

A `DEFAULT` gives a column the value to use when a row **omits** it. It is exercised through the
`INSERT` column list and the `DEFAULT` keyword (grammar.md §16): an unlisted column, or a
`DEFAULT` value slot, takes the column's default.

**Literal-only this slice.** A `DEFAULT` takes a single `literal` — exactly the grammar an
`INSERT` value accepts (int / decimal / text / bytea-as-text / boolean / `NULL`, with an
optional leading `-`). A general-expression default (`DEFAULT 1 + 1`, `DEFAULT now()`) is
deferred; literal-only keeps the value a deterministic constant with no evaluation at INSERT.

**Evaluated + coerced once, at CREATE TABLE.** The default literal is converted to a value and
**type-coerced to the column** at `CREATE TABLE` — the same `store_value` path INSERT uses — and
the coerced value is stored in the catalog. So a bad default fails *at CREATE TABLE*, not at the
first INSERT:

- a default outside the column type's range traps **`22003`** (`DEFAULT 99999` on `int16`);
- a cross-family default traps **`42804`** (`DEFAULT 'x'` on `int32`);
- a decimal default is rounded to the column's typmod there (`DEFAULT 1.5` on `numeric(6,2)`
  stores `1.50`), so the stored default is already in the column's exact form.

**`NOT NULL` is *not* checked at CREATE TABLE.** The default is coerced with the NOT NULL check
disabled, so `DEFAULT NULL` on a `NOT NULL` column is **accepted at CREATE** and stored as NULL
(matching PostgreSQL). The `23502` fires only if that default is actually *applied* — when a row
omits the column or uses the `DEFAULT` keyword (§1).

**Applying a default.** At INSERT, the candidate value for each column is: the value the row
provides; or, for a `DEFAULT` slot or an omitted column, the column's stored default; or NULL
when the column has no default. That candidate then goes through the one `store_value`
chokepoint, which re-applies the column's real `NOT NULL` — so an applied `DEFAULT NULL`, or an
omitted no-default `NOT NULL` column (including an omitted `PRIMARY KEY`, which is NOT NULL),
traps **`23502`**. A column with **no default** that is omitted is simply NULL (allowed iff the
column is nullable).

**Persistence.** A default is stored in the per-column catalog entry: flags **`bit2 has_default`**
plus, when set, the coerced value via the row value codec, written after the decimal typmod
([../fileformat/format.md](../fileformat/format.md)). A `DEFAULT NULL` is the lone presence tag
`0x01`. The default survives serialize→load and is applied to inserts after a reload.

**Cost.** A default is a pre-evaluated constant, so applying one evaluates no expression tree —
an `INSERT` with defaults accrues the same zero cost as one with literal values (grammar.md §12,
CLAUDE.md §13).
