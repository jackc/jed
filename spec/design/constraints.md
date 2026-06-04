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
