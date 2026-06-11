# Column constraints — design

> The semantics of the column constraints in a `CREATE TABLE` column definition. The
> **grammar is authoritative** for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `column_def` / `column_constraint`);
> the **error registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes;
> the **on-disk format** ([../fileformat/format.md](../fileformat/format.md)) owns how a
> constraint is persisted in the catalog. This doc is the *why* and the precise behavior the
> three cores must reproduce identically (CLAUDE.md §2, §8). When a decision here changes,
> change the data/grammar and here in the same edit.

A `column_def` is a name, a type, and zero or more **column constraints**; a `CREATE TABLE`
element may also be a **table constraint** — the composite-capable `PRIMARY KEY` list, or an
(optionally named) `CHECK`:

```
table_element     ::= column_def | table_constraint
table_constraint  ::= "PRIMARY" "KEY" "(" identifier ("," identifier)* ")"
                    | ["CONSTRAINT" identifier] "CHECK" "(" expr ")"
                    | ["CONSTRAINT" identifier] "UNIQUE" "(" identifier ("," identifier)* ")"
column_def        ::= identifier type_name column_constraint*
column_constraint ::= "PRIMARY" "KEY"
                    | "NOT" "NULL"
                    | "DEFAULT" literal
                    | ["CONSTRAINT" identifier] "CHECK" "(" expr ")"
                    | ["CONSTRAINT" identifier] "UNIQUE"
```

Constraints are **order-free** and idempotent: the parsers accept the keywords in any order
and a repeat is harmless (`x int NOT NULL PRIMARY KEY` ≡ `x int PRIMARY KEY NOT NULL` ≡
`x int NOT NULL NOT NULL`; repeated `CHECK`s all collect — each is a distinct constraint).
The constraint keywords are **not reserved** (grammar.md §3) — a column may be named `not`,
`null`, `primary`, `key`, `check`, or `constraint`; the parser distinguishes them
positionally.

This grows one constraint at a time (CLAUDE.md §11, [../../TODO.md](../../TODO.md)).
`FOREIGN KEY` stays deferred; the composite `PRIMARY KEY` landed (§3), `CHECK` landed (§4),
`UNIQUE` landed (§5 — backed by a unique secondary index, [indexes.md §8](indexes.md)).

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

## 3. Composite `PRIMARY KEY` (the table constraint)

`PRIMARY KEY (a, b, …)` declares the table's key over **one or more** named columns. It is
the engine's first **table-level** constraint and may appear anywhere among the column
definitions, interleaved like any other element (PostgreSQL's shape). The single-column
forms are equivalent: `PRIMARY KEY (a)` ≡ a column-level `a … PRIMARY KEY`.

**Resolution (at CREATE TABLE, deterministic order):** each named column must exist —
**`42703`** (`column <name> named in key does not exist`) — and may appear only once —
**`42701`** (`column <name> appears twice in primary key constraint`). A table has **at most
one** primary key across *both* forms: a second table constraint, or a table constraint plus
any column-level `PRIMARY KEY`, traps **`42P16`** (`multiple primary keys for table <name>
are not allowed`). All three codes and messages match PostgreSQL (CLAUDE.md §1,
oracle-checked).

**Every member is a key column.** Each member is implicitly `NOT NULL` (§1) and must be of a
key-encodable type — the same per-column rule as the column-level form: the integer types,
`uuid`, `timestamp`, `timestamptz`; a `text`/`decimal`/`bytea`/`boolean` member is the same
documented `0A000` narrowing ([types.md](types.md) §9/§11/§12/§13). The UPDATE narrowing
extends naturally: assigning **any** member column traps `0A000` (grammar.md §14 — the
storage key never changes).

**The key bytes are the concatenation** of the members' bare encodings, in **key order**
([encoding.md](encoding.md) §2.3 — now exercised). Every keyable type is fixed-width, so the
concatenation is self-delimiting and `memcmp` over the composite key equals the tuple's
lexicographic logical order; the stored scan order is `ORDER BY a, b, …` for free, and the
`ORDER BY` full-tie break "by primary key" (grammar.md §10) is the composite tuple.
Uniqueness is over the **whole tuple**: a duplicate `(a, b, …)` traps **`23505`** in
INSERT's two-phase pass; two rows sharing a prefix are distinct rows.

**Key order is the list order — any order.** `PRIMARY KEY (b, a)` keys the table by `b`
then `a`, independent of declaration order (PostgreSQL's behavior). *History:* the original
slice required list order to match declaration order (`0A000`) because the catalog persisted
the key only as per-column flag bits — a member *set* with no independent order. The
secondary-index catalog reshape (`format_version` 5 — [indexes.md §6](indexes.md),
[../fileformat/format.md](../fileformat/format.md)) records the key as an explicit ordinal
list in key order, which lifted the narrowing.

**Planner.** The primary-key pushdown (cost.md §3) recognizes **single-column** keys only; a
composite-PK table scans whole this slice (sound and deterministic — the bound is an
optimization, never a semantic). Composite point-lookup/prefix pushdown is a follow-on
optimization slice and carries the NoREC growth obligation with it (conformance.md §8).

**Persistence.** Since `format_version` 5 the catalog records the primary key as an explicit
**ordinal list in key order** (`pk_count` + column ordinals — format.md); the old per-column
flag `bit0` is retired (reserved, written 0). The cross-core byte fixture is the
`composite_pk_table.jed` golden; the out-of-declaration-order case is pinned by
`index_table.jed`.

## 4. `CHECK`

A `CHECK` constraint is a **row predicate**: a boolean expression over the table's columns
that every stored row must not falsify. It is enforced at the two write paths (INSERT and
UPDATE) on each candidate row; a row for which the expression evaluates to **FALSE** traps
**`23514`** (`check_violation`):

```
new row for relation <table> violates check constraint <name>
```

**TRUE and NULL both pass** (the SQL-standard rule PostgreSQL follows: only FALSE violates —
so `CHECK (a > 0)` admits a NULL `a`; combine with `NOT NULL` to forbid it). Every behavior
in this section is oracle-checked against PostgreSQL (CLAUDE.md §1).

### 4.1 Surface

Both positions, same constraint: a **column-level** `CHECK` (among a column's constraints)
and a **table-level** `CHECK` (a table element) are semantically identical — either may
reference **any** of the table's columns, including columns defined later in the statement
(PostgreSQL's model). The position feeds only the constraint's *textual definition order*
(naming, §4.3). An optional **`CONSTRAINT <name>`** prefix names the constraint; the prefix
is accepted only immediately before `CHECK` this slice (PostgreSQL allows it before any
constraint — a relaxable jed narrowing).

The expression is a general scalar expression (the full `expr` grammar: arithmetic,
comparisons, `AND`/`OR`/`NOT`, `IS NULL`, `IS DISTINCT FROM`, `IN`, `BETWEEN`, `LIKE`,
`CASE`, `CAST`, scalar function calls), restricted at CREATE TABLE:

- it must be **boolean-typed** (NULL counts as boolean) — else **`42804`**
  (`argument of CHECK must be boolean`);
- a **subquery** is rejected — **`0A000`** (`cannot use subquery in check constraint`);
- an **aggregate** is rejected — **`42803`**
  (`aggregate functions are not allowed in check constraints`);
- a **bind parameter** `$N` is rejected — **`42P02`** (`there is no parameter $N`);
- column references resolve against this table only: an unknown column is **`42703`**, a
  qualifier other than the table's name is the resolver's usual **`42P01`**. A reference may
  be table-qualified (`CHECK (t.a > 0)` inside `CREATE TABLE t`).

All codes and messages match PostgreSQL (oracle-probed), with one documented divergence: jed
checks the **structural** rejections (subquery / aggregate / parameter, a single
depth-first pre-walk) before resolving names and types, while PostgreSQL interleaves them in
parse order — so a statement containing *both* kinds of error in one expression may report a
different (equally valid) error than PG. Recorded in the oracle-override ledger.

### 4.2 Validation order at CREATE TABLE (deterministic, PG-matched)

1. Columns are processed as before (duplicate name `42701`, type resolution, the PK gates,
   `DEFAULT` coercion — §2). Both `CHECK` forms are *collected* in textual order, not yet
   validated.
2. The table-level `PRIMARY KEY` constraints resolve (§3) — PK errors fire before any check
   expression is examined (PG's order: index constraints transform first).
3. Each check **validates** in textual definition order: the §4.1 pre-walk, then name/type
   resolution, then the boolean gate.
4. Each check is **named** in textual definition order (§4.3); a name collision traps
   **`42710`** (`constraint <name> for relation <table> already exists` — the template
   generalized to PG's wording when `UNIQUE` joined the per-table constraint namespace,
   §5). All validation (step 3) precedes all naming — a `42703` in a later check fires
   before a `42710` between earlier ones (oracle-probed).

**`DEFAULT` is not checked against `CHECK` at CREATE TABLE** (matching §2's `NOT NULL` rule
and PostgreSQL): `a int DEFAULT -5 CHECK (a > 0)` is accepted; the `23514` fires only when
the default is applied to an inserted row.

### 4.3 Naming

An explicit `CONSTRAINT <name>` is used as written (names follow the engine's identifier
convention: original case round-trips, comparisons are case-insensitive). Otherwise the name
is **derived, PostgreSQL's algorithm**: let the *referenced columns* be the distinct columns
the expression mentions —

- exactly one → `<table>_<column>_check`;
- zero or several → `<table>_check`;
- if that name is taken (by any earlier-named check, explicit or derived), append the
  smallest positive integer that frees it (`<table>_<column>_check1`, `…2`, …).

Derived names are built from the **lowercased** table/column names (what PostgreSQL's
identifier folding produces). Naming is a single pass in textual definition order — an
explicit name colliding with an *earlier* derived name is `42710` (derived names never
yield; oracle-probed).

### 4.4 Enforcement

At INSERT (both sources, including `INSERT … SELECT`) and UPDATE, **per candidate row**,
inside the existing two-phase / all-or-nothing pass (grammar.md §12): after the row's values
are coerced (`22003`) and `NOT NULL` is applied (`23502` — NOT NULL fires before CHECK,
PG's order), and **before** the storage key is built and checked (`23505` — CHECK fires
before a duplicate key, PG's order), every check constraint is evaluated against the
candidate row **in name order** (ascending byte order of the lowercased name — PostgreSQL
evaluates checks sorted by name, oracle-probed; `aa` fires before `zz` regardless of
definition order). The first FALSE aborts the statement with `23514`; nothing is written.

- **UPDATE** evaluates **every** check on each post-assignment row (not only checks
  mentioning assigned columns — same observable result as PG, and the deterministic-cost
  contract needs the fixed rule). Rows the WHERE does not match evaluate nothing.
- A runtime error inside a check expression propagates as itself (`22012` on a division by
  zero, etc.) — same as any expression evaluation.
- Checks reference at most the candidate row, so evaluation needs no storage reads; on
  UPDATE the row is already fully resident at evaluation time (the §14 large-values
  resolve-on-rewrite precedes it).

**Cost.** A check evaluation is ordinary expression evaluation through the metered
evaluator: `operator_eval` per interior node (and `decimal_work` where decimal arithmetic
fires), per check, per candidate row ([cost.md](cost.md) §3). An `INSERT … VALUES` into a
checked table therefore accrues nonzero cost — the documented exception to "VALUES inserts
cost zero". The ceiling (`max_cost`) aborts mid-validation deterministically like any other
expression work.

### 4.5 Persistence

A check constraint is stored in the table's catalog entry as its **name** plus its
**expression text**, under `format_version` **4** ([../fileformat/format.md
](../fileformat/format.md)): after the column entries, a `check_count` and per-check
`(name, expr_text)` pairs, ordered by the **evaluation order** (lowercased-name byte order).
The expression text is the **re-rendered source token sequence** (format.md defines the
per-token rendering — a closed, recursion-free byte contract identical across cores); on
load each core re-parses the stored text with its ordinary expression parser, and a commit
writes the retained text back verbatim, so the catalog bytes are stable across
create → commit → load → commit. Unparseable stored text in an otherwise-valid file is
**`XX001`** (`data_corrupted`) at open.

## 5. `UNIQUE`

A `UNIQUE` constraint forbids two rows from sharing a value tuple over its member columns.
**A UNIQUE constraint IS a unique secondary index** — jed materializes the constraint as
nothing but an [indexes.md](indexes.md) index with the `unique` flag, named per PG's
constraint convention. There is no separate constraint object: the index's name is the
constraint's name (it is what the `23505` message reports), and the enforcement semantics
live with the index ([indexes.md §8](indexes.md) — *NULLS DISTINCT*, the write-time checks,
`CREATE UNIQUE INDEX`). This section is the CREATE TABLE surface. Everything here is
oracle-probed against PostgreSQL 18 except the documented divergences.

### 5.1 Surface and resolution

Both positions, same constraint: a **column-level** `[CONSTRAINT name] UNIQUE` is the
one-member form over its own column; a **table-level**
`[CONSTRAINT name] UNIQUE (a, b, …)` lists one or more members
([grammar.md §31](grammar.md)). Constraints are collected in **textual definition order**
(a column-level one at its column's position), like checks.

**Resolution** (per constraint, in textual order, after the table-level `PRIMARY KEY`
constraints and **before** any `CHECK` validates — PG's order, probed): each member must
exist — **`42703`** (`column <name> named in key does not exist`, the PK wording) — appear
once — **`42701`** (`column <name> appears twice in unique constraint`) — and be of a
key-encodable type — **`0A000`** (the same documented narrowing as a PK member / index key
column; *unlike* a PK member, a UNIQUE member stays **nullable**).

### 5.2 Dedup and the PK fold (PG-matched, probed)

A uniqueness requirement already guaranteed elsewhere is **folded away**, matching
PostgreSQL:

- a UNIQUE constraint whose member list is **identical to the primary key's** (same
  columns, same order) creates nothing — the PK already enforces it (`a int PRIMARY KEY
  UNIQUE` and `PRIMARY KEY (a, b) … UNIQUE (a, b)` each yield one index-free PK); a
  *differing order* (`UNIQUE (b, a)` against `PRIMARY KEY (a, b)`) is a distinct
  constraint and stays;
- two UNIQUE constraints with **identical member lists** fold into one. The surviving
  name is the **first explicitly-named** one's, if any (`a int UNIQUE, CONSTRAINT named
  UNIQUE (a)` keeps `named` — probed); with no explicit name the one auto-name derives.
  Identical lists with two *different* explicit names also fold (PG keeps the first —
  `CONSTRAINT x UNIQUE (a), CONSTRAINT y UNIQUE (a)` keeps `x`).

A repeated bare `UNIQUE` on one column is the trivial case of the same fold.

### 5.3 Naming the backing index

After the folds, each surviving constraint names its index, in textual order:

- an **explicit** `CONSTRAINT <name>` is used as written; it is checked against the
  **relation namespace** first — tables (including the one being created) and indexes,
  `42P07` — then against the table's **constraint names** (its checks) — `42710`
  (`constraint <name> for relation <table> already exists`). Relation before constraint
  is PG's probed order;
- an **omitted** name derives PostgreSQL's choice: the lowercased
  `<table>_<col>_<col>…_key` (members in list order), suffix-walked past **both**
  namespaces (a check named `t_a_key` pushes the derived name to `t_a_key1` — probed).

The `_key` suffix (vs `CREATE INDEX`'s `_idx`) is the only naming difference from a plain
index; the walk rule is otherwise [indexes.md §2](indexes.md)'s.

### 5.4 Enforcement and the violation message

Enforcement is the unique index's ([indexes.md §8](indexes.md)): at INSERT and UPDATE,
inside the two-phase / all-or-nothing pass, **after** the primary-key duplicate check. A
violation traps **`23505`** (`unique_violation`):

```
duplicate key value violates unique constraint: <name>
```

`<name>` is the violated unique index's name. The **primary key's own** `23505` reports
the derived `<table>_pkey` (lowercased — PostgreSQL's auto-name for the PK index). jed
does **not** persist or reserve that name: `CREATE INDEX t_pkey ON t (a)` is legal in jed
where PG would collide — a documented divergence (the PK has no index object here).

When one row violates several uniqueness constraints, the reported one is: the **primary
key first**, then the unique indexes in the catalog's **ascending lowercased-name order**
(jed's standing deterministic order — checks fire in name order the same way, §4.4).
PostgreSQL reports in index *creation* order, which jed does not persist — a documented
divergence (the code is identical; only the choice of reported name differs).

### 5.5 Persistence

Nothing constraint-specific is persisted: the backing index is stored like any other, with
its **`unique` flag** in the per-index catalog flags byte (`format_version` **6** —
[../fileformat/format.md](../fileformat/format.md), [indexes.md §6](indexes.md)). A
reloaded database enforces exactly as the creating session did. The byte fixture is the
`unique_table.jed` golden.
