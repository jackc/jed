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
                    | ["CONSTRAINT" identifier] "FOREIGN" "KEY"
                          "(" identifier ("," identifier)* ")" references_clause
column_def        ::= identifier type_name column_constraint*
column_constraint ::= "PRIMARY" "KEY"
                    | "NOT" "NULL"
                    | "DEFAULT" expr
                    | ["CONSTRAINT" identifier] "CHECK" "(" expr ")"
                    | ["CONSTRAINT" identifier] "UNIQUE"
                    | references_clause
references_clause  ::= "REFERENCES" identifier ("(" identifier ("," identifier)* ")")?
                       ("ON" ("DELETE" | "UPDATE") referential_action)*
referential_action ::= "NO" "ACTION" | "RESTRICT" | "CASCADE" | "SET" "NULL" | "SET" "DEFAULT"
```

Constraints are **order-free** and idempotent: the parsers accept the keywords in any order
and a repeat is harmless (`x int NOT NULL PRIMARY KEY` ≡ `x int PRIMARY KEY NOT NULL` ≡
`x int NOT NULL NOT NULL`; repeated `CHECK`s all collect — each is a distinct constraint).
The constraint keywords are **not reserved** (grammar.md §3) — a column may be named `not`,
`null`, `primary`, `key`, `check`, or `constraint`; the parser distinguishes them
positionally.

This grows one constraint at a time (CLAUDE.md §11, [../../TODO.md](../../TODO.md)). The
composite `PRIMARY KEY` landed (§3), `CHECK` landed (§4), `UNIQUE` landed (§5 — backed by a
unique secondary index, [indexes.md §8](indexes.md)), and `FOREIGN KEY` landed (§6 — the
referential constraint, `format_version` 11). The `table_constraint` and `column_constraint`
grammar above show the full set.

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

A `DEFAULT` takes any scalar `expr` (the full expression grammar — arithmetic, `CASE`, `CAST`,
scalar function calls), with the same structural restrictions a `CHECK` carries plus one more —
a default may **not reference a column** (it is computed before the row exists):

- a **column reference** is **`0A000`** (`cannot use column reference in DEFAULT expression`);
- a **subquery** is **`0A000`** (`cannot use subquery in DEFAULT expression`);
- an **aggregate** is **`42803`** (`aggregate functions are not allowed in DEFAULT expressions`);
- a **bind parameter** `$N` is **`42P02`** (`there is no parameter $N`).

(Like `CHECK` (§4.1), jed runs these structural rejections as a single pre-walk before
name/type resolution — a documented micro-order divergence from PostgreSQL recorded in the
oracle-override ledger.)

**Two representations, decided by syntactic form.** The default's *form* picks how it is stored
and when it is evaluated:

- **A constant `literal`** (a bare literal — int / decimal / text / bytea-as-text / boolean /
  `NULL`, with an optional leading `-`, which the parser folds into the literal) is
  **pre-evaluated + type-coerced once at `CREATE TABLE`** via the same `store_value` path INSERT
  uses, and the coerced value is stored in the catalog. This is the literal fast-path: applying
  it at INSERT evaluates no expression and costs zero.
- **Any other expression** (a function call like `uuidv7()`, arithmetic like `1 + 1`, a typed
  literal like `interval '1 day'`) is stored as **expression text** — the parsed token sequence
  re-rendered by the closed token table ([../fileformat/format.md](../fileformat/format.md)
  "Check-expression text"), exactly as a `CHECK` is (§4.5) — and **evaluated per row at INSERT**.

**Validated once, at CREATE TABLE.** Either way a bad default fails *at CREATE TABLE*, not at the
first INSERT. For a literal the value is checked there (out of range **`22003`**, cross-family
**`42804`**, decimal rounded to the column's typmod — `DEFAULT 1.5` on `numeric(6,2)` stores
`1.50`). For an expression the default is resolved against an **empty scope** (no columns) and
its result type is checked for assignability to the column — a cross-family result traps
**`42804`** (`column <name> is of type <t> but default expression is of type <u>`); the
per-value range/rounding (`22003`) then happens at INSERT through `store_value` (its value is
unknown until evaluated).

**`NOT NULL` is *not* checked at CREATE TABLE.** A literal default is coerced with the NOT NULL
check disabled, so `DEFAULT NULL` on a `NOT NULL` column is **accepted at CREATE** and stored as
NULL (matching PostgreSQL); the `23502` fires only when it is *applied*. An expression default is
likewise not NULL-checked at CREATE — a default expression that evaluates to NULL into a
`NOT NULL` column traps `23502` only when applied.

**Applying a default.** At INSERT, the candidate value for each column is: the value the row
provides; or, for a `DEFAULT` slot, an omitted column, or every column of an `INSERT ... DEFAULT
VALUES` row, the column's default — the stored
constant, or the **expression evaluated for that row**; or NULL when the column has no default.
That candidate then goes through the one `store_value` chokepoint, which re-applies the column's
real `NOT NULL` (so an applied `DEFAULT NULL`, or an omitted no-default `NOT NULL` column —
including an omitted `PRIMARY KEY`, which is NOT NULL — traps **`23502`**), the per-value range
check (`22003`), and the table's `CHECK`s. A column with **no default** that is omitted is simply
NULL (allowed iff the column is nullable).

An expression default is evaluated through the **per-statement entropy/clock seam**
([entropy.md](entropy.md)), so a multi-row `INSERT` of `DEFAULT uuidv7()` produces **distinct,
monotonic, time-ordered** UUIDs — every default evaluation in one statement shares one `StmtRng`
(the statement clock is read once; the `uuidv7` counter advances across rows).

**Persistence.** A default is stored in the per-column catalog entry's flags byte
([../fileformat/format.md](../fileformat/format.md)): **`bit2 has_default`** for a constant
(the coerced value via the row value codec, after the typmod; a `DEFAULT NULL` is the lone
presence tag `0x01`), or **`bit3 default_is_expr`** for an expression (the expr-text as a
length-prefixed UTF-8 string, after the typmod, re-rendered by the same token table a `CHECK`
uses and re-parsed on load — unparseable stored text in an otherwise-valid file is **`XX001`**).
The two bits are mutually exclusive. The default survives serialize→load and is applied to
inserts after a reload. (This is `format_version` **8**.)

**Cost.** A constant default is pre-evaluated, so applying one evaluates no expression tree — an
`INSERT` with only literal defaults accrues the same zero cost as one with literal values. An
**expression default** evaluates a tree per row, so each application accrues `operator_eval` per
interior node (and the function's own units) — the same documented exception `CHECK` carries to
"VALUES inserts cost zero" (§4.4, grammar.md §12, CLAUDE.md §13). The ceiling (`max_cost`) aborts
mid-evaluation deterministically.

**Explicit all-default INSERT.** `INSERT INTO t DEFAULT VALUES` inserts exactly one row with every
column omitted. It is the direct spelling of the existing omitted-column path: constant defaults
stay free, expression defaults evaluate once for the row through the statement seam, no-default
columns take NULL, and the ordinary `23502`/CHECK/uniqueness/FK validations apply before the write.
It composes with `ON CONFLICT` and `RETURNING`. Matching PostgreSQL's grammar, an explicit column
list or `OVERRIDING` cannot precede this source (`42601`).

**Resetting a column on UPDATE.** `UPDATE t SET x = DEFAULT [WHERE ...]` applies `x`'s declared
default once per matched row, or NULL when the column has no default. It uses the same resolved
constant/expression default and per-statement entropy/clock seam as INSERT; the resulting value goes
through UPDATE's ordinary storage coercion, `NOT NULL`, CHECK, end-state uniqueness/FK validation,
index maintenance, and primary-key re-keying. Multiple DEFAULT assignments share the statement RNG
and every assignment still reads the old row, just like an ordinary multi-assignment UPDATE. A
constant/no default adds no expression cost; an expression default accrues its ordinary per-row
evaluation cost. Privilege preflight includes every named function in the selected default expression,
so each still requires the session's ordinary `EXECUTE` privilege before any row is evaluated.
`ON CONFLICT DO UPDATE SET x = DEFAULT` remains a separate deferred follow-on.

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
documented `0A000` narrowing ([types.md](types.md) §9/§11/§12/§13). Assigning a member column
in an UPDATE is allowed and **re-keys** the row (CLAUDE.md §11 step 6, §6.5 below — the
storage key is recomputed from the post-assignment members and the row moves; a resulting key
collision traps `23505`, like an INSERT).

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

## 6. `FOREIGN KEY`

A `FOREIGN KEY` constraint is a **referential** constraint: the *referencing* (child) table's
foreign-key columns must, in every row, hold a value tuple that exists as a key of the
*referenced* (parent) table — or be (partly) NULL, which is exempt. It is the engine's first
**cross-table** constraint, so unlike `CHECK`/`UNIQUE` it is enforced at **four** write sites: a
child `INSERT`/`UPDATE` must find its key in the parent, and a parent `DELETE`/`UPDATE` must not
strand a child. A violation traps **`23503`** (`foreign_key_violation`). Everything here is
oracle-probed against PostgreSQL 18 (CLAUDE.md §1) except the documented divergences (§6.7).

### 6.1 Surface

Both constraint positions, same constraint:

- **column-level** `col type [CONSTRAINT name] REFERENCES parent [( refcol )] [actions]` — the
  one-member form; the local column is the column being defined, and the referenced-column list is
  optional (defaults to the parent's primary key);
- **table-level** `[CONSTRAINT name] FOREIGN KEY (a, b, …) REFERENCES parent (x, y, …) [actions]` —
  one or more local columns, paired positionally with the referenced columns.

Constraints are collected in **textual definition order** (a column-level one at its column's
position), like checks and uniques. The local-column and referenced-column lists reuse the
`PRIMARY KEY` list shape (bare names, non-empty). `actions` is zero or more
`ON DELETE` / `ON UPDATE referential_action` clauses, each at most once, in either order
(grammar.md §43).

### 6.2 Resolution (at CREATE TABLE, deterministic, PG-matched)

FK constraints resolve **after** the table-level `PRIMARY KEY`, the `UNIQUE` constraints, and the
`CHECK` constraints (PostgreSQL's order, probed), each in **textual definition order**:

1. **Local columns.** Each local column must exist in the table being created — **`42703`**
   (`column <name> named in key does not exist`, the PK wording) — and appear once — **`42701`**
   (`column <name> appears twice in foreign key constraint`). A local column may be of any type
   here; the type gate is the cross-check in step 5.
2. **Parent table.** Look it up case-insensitively (CLAUDE.md §1). It may be the table being
   created — a **self-reference** — in which case it resolves against the in-progress definition
   (its columns, its just-resolved PK, its just-named unique indexes). A missing parent is
   **`42P01`** (`table does not exist: <name>`).
3. **Referenced columns.** An omitted `(refcol …)` list defaults to the **parent's primary key**
   (in PK key order); a parent with no primary key is **`42704`**
   (`there is no primary key for referenced table <parent>`) — PG's code for the omitted-list case
   specifically (the default is the PK, so a parent UNIQUE does not satisfy it), distinct from the
   explicit-no-match `42830` in step 4. An explicit list resolves against the parent — unknown
   column **`42703`**, repeated **`42701`**. The referencing and referenced counts must **agree** —
   else **`42830`** (`number of referencing and referenced columns for foreign key disagree`).
4. **The referenced columns must be a unique key of the parent.** Their *set* must equal the
   parent's primary-key column set **or** the column set of one of the parent's UNIQUE constraints
   (a unique index, §5 / [indexes.md §8](indexes.md)) — else **`42830`** (`there is no unique
   constraint matching given keys for referenced table <parent>`). This is what makes the existence
   probe (§6.4) a point/prefix lookup rather than a scan, and matches PG (which requires a unique or
   primary-key constraint on the referenced columns). When several match, the **primary key wins**,
   then unique indexes in ascending lowercased-name order (jed's standing deterministic order) — the
   choice only fixes which physical tree the probe walks; the result is identical.
5. **Types must match.** Each local column and its paired referenced column must be the **same**
   scalar type — else **`42804`** (`foreign key constraint <name> cannot be implemented: key columns
   <local> and <ref> are of incompatible types`). This is **stricter than PostgreSQL** (which allows
   any pair sharing a btree equality operator, e.g. `i32`↔`i64`); the strict static type system
   (CLAUDE.md §4) is the overriding reason, recorded in §6.7. Because the referenced columns are a
   PK/unique key, they are already key-encodable; same-type pairing makes the local columns
   key-encodable too, so no separate `0A000` type gate is needed.
6. **Referential actions.** `NO ACTION` (the default) and `RESTRICT` are accepted and stored;
   `CASCADE`, `SET NULL`, `SET DEFAULT` parse but are rejected — **`0A000`** (`ON DELETE/UPDATE
   <action> is not supported`) — a documented narrowing (§6.6).
7. **Naming.** An explicit `CONSTRAINT <name>` is used as written; otherwise the name is derived,
   PostgreSQL's algorithm: lowercased `<table>_<localcol>_<localcol>…_fkey` (every local column in
   list order, joined by `_`), suffix-walked past the table's **constraint namespace** (its checks
   and earlier FKs) with the smallest positive integer that frees it (`…_fkey1`, `…_fkey2`, …). An
   explicit name colliding with an existing check or FK name is **`42710`** (`constraint <name> for
   relation <table> already exists`). The FK shares the per-table **constraint** namespace with
   `CHECK` (§4.3); it creates **no** relation object, so — unlike a `UNIQUE` constraint's backing
   index — its name is **not** in the relation namespace and never collides with a table/index name
   (a minor divergence, §6.7).

All resolution precedes any write; a failure aborts CREATE TABLE having created nothing.

### 6.3 MATCH SIMPLE (the NULL rule)

jed implements **MATCH SIMPLE**, PostgreSQL's default: a child row is **exempt** from the FK check
if **any** of its foreign-key columns is NULL (the whole tuple passes, even when the non-NULL
components match no parent). Only a row whose FK columns are **all non-NULL** is checked. `MATCH
FULL` (exempt only when *all* are NULL) and `MATCH PARTIAL` are not implemented and not in the
grammar. Mechanically the exemption reuses the [indexes.md §3](indexes.md) NULLS-DISTINCT machinery:
a row's **probe key** is built only when every FK component is present; a `None`/`null` probe (any
NULL) skips the check — the same primitive `indexPrefixKey` returns for a NULL unique-index member.

### 6.4 Enforcement — the child side (INSERT / UPDATE of the referencing table)

Inside the existing two-phase / all-or-nothing pass (grammar.md §12), **per candidate row**, after
the row's own constraints (NOT NULL `23502`, CHECK `23514`, the PK/UNIQUE duplicate checks `23505` —
FK fires **last**, PG's order), each FK on the table is checked **in name order**:

- build the probe tuple from the candidate row's FK-column values; if any is NULL, **skip**
  (§6.3);
- otherwise the key must exist in the parent — **either** in the parent's committed rows **or** in
  the **end state of this statement's own batch** (so a multi-row `INSERT` whose later rows supply
  the parent key for earlier rows succeeds, and a self-referential `INSERT` resolves within the
  batch — PG's end-of-statement semantics, probed). Absent → **`23503`**:

  ```
  insert or update on table <table> violates foreign key constraint <name>
  ```

The probe is a **point lookup** when the referenced columns are the parent's PK (encode the FK
values in the parent's PK key order — §6.8 — and `parentStore.get(key)`), or a **unique-prefix
range probe** when they are a UNIQUE index (build the index prefix in index-key order and probe the
parent's index store, [indexes.md §8](indexes.md)). The batch end-state is consulted by scanning the
statement's already-prepared candidate rows for a matching referenced tuple.

**UPDATE (child side).** An FK is re-checked on a row **only when the statement assigns one of its
local columns** (otherwise the value is unchanged and still valid — the same skip the index
maintenance uses, indexes.md §4). The check runs on the post-assignment row, in UPDATE's phase 1,
against the **committed parent state plus the statement's end state**, exactly as INSERT.

### 6.5 Enforcement — the parent side (DELETE / UPDATE of the referenced table)

A parent mutation must not leave a child pointing at a key that no longer exists. For each FK in any
**other** table (and the table itself, for a self-reference) that references this table, in phase 1:

- **DELETE.** For each deleted parent row, the referenced tuple **disappears** unless another
  (non-deleted) parent row still carries it — but the referenced columns are unique, so a deleted
  row's tuple is unique to it. If any child row references that tuple → **`23503`**:

  ```
  update or delete on table <table> violates foreign key constraint <name>
  ```

- **UPDATE.** A referenced column — **primary-key** (now re-keyable, §11 step 6 / CLAUDE.md) or
  **UNIQUE** — may change. jed computes the set of referenced tuples that were present in the
  updated rows' **old** values but are **absent from the statement's end state** (`old_tuples −
  new_tuples`, over the updated rows; untouched rows keep their values and, by uniqueness, cannot
  hold a disappearing tuple). For each such **disappearing** tuple, if a child references it →
  **`23503`**. For a **self-referencing** FK the child *is* this table, so "references it" includes
  an updated row's **own new** local-column value: re-keying a row away from an id it still points
  at strands itself (`23503`), since the committed child-scan reads the pre-update parent and
  cannot see the row's new key. A referenced-value **swap** (or a key cascade) therefore succeeds
  (the end state still contains every referenced tuple) where PostgreSQL's per-row check fails on
  the transient — the same end-state divergence `UNIQUE` carries (§6.7, [indexes.md §7](indexes.md)).

Finding the children is the **reverse** of the child-side probe and is **not** index-accelerated:
the child's FK columns are not necessarily indexed (PostgreSQL does not auto-index them either), so
jed **full-scans** each referencing table for a row whose FK tuple equals the disappearing parent
tuple (MATCH SIMPLE: a child row with any NULL FK column references nothing and is skipped). This is
O(child rows) per parent mutation; an opt-in backing index on FK columns is a follow-on optimization
slice ([../../TODO.md](../../TODO.md)). When more than one FK is violated, the reported one is
deterministic: referencing tables in ascending lowercased-name order, then FKs in name order.

### 6.6 Referential actions

The grammar accepts the full `ON DELETE` / `ON UPDATE` action set; this slice **supports only**
`NO ACTION` and `RESTRICT`, which are **identical in jed**: both reject a parent mutation that would
orphan a child (§6.5). PostgreSQL distinguishes them by *deferrability* (NO ACTION may be deferred to
end-of-statement, RESTRICT is immediate), but jed has no deferrable constraints and validates every
constraint at the statement boundary anyway, so the distinction is unobservable. `CASCADE`,
`SET NULL`, `SET DEFAULT` — which would *write* the child during a parent mutation — are rejected at
CREATE TABLE (`0A000`) and deferred to a later slice; supporting them means threading cascading
child writes through the parent's two-phase pass. The stored action is persisted (§6.9) so the
catalog is forward-compatible, but only `NO ACTION`/`RESTRICT` are ever written today.

### 6.7 Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **Stricter type matching.** Corresponding FK columns must be the **same** scalar type (`42804`);
  PG allows any pair with a shared btree equality operator (e.g. `i32`↔`i64`). The strict static
  type system (CLAUDE.md §4) is the overriding reason.
- **End-state, not per-row, parent-side checks.** A parent UPDATE that swaps referenced unique
  values, or otherwise keeps every referenced tuple present in the end state, succeeds where PG's
  per-row check fails on the transient — the two-phase / all-or-nothing model (CLAUDE.md §11
  step 6), the same divergence `UNIQUE` carries ([indexes.md §7](indexes.md)).
- **Referential actions.** Only `NO ACTION`/`RESTRICT`; `CASCADE`/`SET NULL`/`SET DEFAULT` are
  `0A000` (§6.6).
- **No DETAIL line.** The `23503` message is a single line in jed's house style (no PG `DETAIL: Key
  (…)=(…) is …` second line); the code matches.
- **Constraint namespace only.** An FK name lives in the per-table **constraint** namespace (with
  CHECK), not the relation namespace — `CREATE TABLE fk_name (…)` does not collide with an FK named
  `fk_name`. PG keeps constraint names per-table too, so this matches PG; the note is only that, like
  the PK's `_pkey`, jed does not reserve FK names in the relation namespace.
- **No system catalog surface / `ALTER TABLE`.** FKs are observable only via their enforcement and
  the per-core host catalog; there is no `pg_constraint`, no `ALTER TABLE … ADD/DROP CONSTRAINT`, and
  no `ON DELETE … CASCADE` at `DROP TABLE` (see §6.10).

### 6.8 Key-order mapping

The referenced columns need not be listed in the parent's key order: `REFERENCES p (b, a)` against
`PRIMARY KEY (a, b)` pairs the FK's first local column with `b`, second with `a`. To probe, jed
rebuilds the tuple in the **parent key's order**: for each parent key position (a PK ordinal in key
order, or a unique index's column in index-key order) it finds the referenced-list slot naming that
parent column, takes the paired local column's value from the child row, and encodes it with the
parent column's type ([encoding.md §2.3](encoding.md)). The concatenation is exactly the parent's
storage key (PK case) or unique-index prefix (UNIQUE case), so the probe is `memcmp`-correct with no
special-casing.

### 6.9 Persistence (`format_version` 11)

Each table's catalog entry gains a **foreign-key list**, after the index list and before the
trailing root-page pointer ([../fileformat/format.md](../fileformat/format.md)): an `fk_count`
followed, per FK, by the **constraint name**, the **local-column ordinal list** (count + ordinals,
in declaration/list order), the **referenced table name**, the **referenced-column ordinal list**
(count + ordinals, in list order — ordinals into the *parent* table), and a **flags/action byte**
(`on_delete` and `on_update` actions, two bits each; remaining bits reserved, written 0 and
read-validated → `XX001`). The list is held in **ascending lowercased-name order** (the catalog's
standing deterministic order, like checks and indexes), which is also the §6.4 child-side
name-evaluation order. An FK creates no B-tree, so — unlike an index — it stores no root page. On
load the referenced-table/column names and ordinals are read back verbatim; a dangling reference in
an otherwise-valid file is `XX001` (`data_corrupted`). The cross-core byte fixture is the
`fk_table.jed` golden (`rust == go == ts == ruby`).

### 6.10 DROP TABLE and dependencies

A table that is **referenced** by another table's FK cannot be dropped — **`2BP01`**
(`dependent_objects_still_exist`): `cannot drop table <name> because other objects depend on it`.
This reuses the `DROP TYPE` dependency machinery (§composite, error 2BP01): `DROP TABLE` scans every
*other* table's FK list for one whose parent is the target and rejects if found. A **self-reference
does not block** the drop (a table's own FK on itself disappears with it). There is no
`DROP TABLE … CASCADE` (the grammar has none — grammar.md §13); dropping the *referencing* table is
always fine and takes its FK with it. (`DROP TABLE` cost stays zero — a pure catalog edit.)

### 6.11 Cost

FK enforcement is **unmetered** validation work, like the primary-key duplicate check and the
uniqueness probes (cost.md §3 "What is NOT metered"): a child `INSERT`/`UPDATE` accrues the same cost
whether or not the table has FKs, and a parent `DELETE`/`UPDATE`'s child scan is not charged. (The
`max_cost` ceiling therefore does not bound the parent-side child scan — acceptable because the child
table's size is itself bounded by the metered work that populated it, the same reasoning `UNIQUE`
relies on, §5; the backing-index follow-on would make the scan a probe.) A runtime error inside an
expression along the way (none arise from FK checks themselves) propagates as itself.
