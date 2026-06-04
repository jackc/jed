# SQL grammar — design

> The reasoning behind the SQL grammar. The **grammar is authoritative**
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf)); this doc is the *why*. When a
> decision here changes, change it in the grammar and here in the same edit, and update
> [CLAUDE.md](../../CLAUDE.md) if it revises a load-bearing commitment.

There is **one EBNF grammar** and the per-language parsers are hand-written from it
(CLAUDE.md §5, §6). The grammar — not any parser — is the shared contract for the SQL
surface. This doc explains the notation, the deliberate narrowings the current surface
makes, and the rule for growing it.

## 1. Role: the grammar is the contract, the parsers descend from it

CLAUDE.md §2 forbids a reference implementation: no language's core is canonical, so an
implementation accident must never become the de-facto spec. The grammar is the
language-neutral statement of *what is parseable*, and [impl/rust](../../impl/rust),
[impl/go](../../impl/go), and [impl/ts](../../impl/ts) are **downstream consumers** of
it, the same way each is a consumer of the type tables and the error registry.

This grammar was **backfilled**: the three parsers were written in lockstep first and an
authored grammar followed, so the first version is *descriptive* — it documents exactly
the surface those parsers already accept and reject, nothing more. From here the
ordering inverts to match CLAUDE.md §10/§11: a new feature grows the grammar **first**,
in the same change that adds its conformance entries and its parser code (§7 below).
The grammar must stay descriptive — it must not document syntax a parser rejects, nor
omit syntax a parser accepts.

## 2. Notation: W3C-style EBNF

The grammar uses the EBNF dialect of the XML / XPath / XQuery specifications
(`Symbol ::= expr`, juxtaposition for concatenation, `?` / `*` / `+`, `( ... )`
grouping, `"..."` terminals, `[a-z]` character classes, slash-star comments) rather than
ISO/IEC 14977. The W3C form reads closer to the railroad-style grammars common to SQL
references: optional/repeat postfix operators and juxtaposition are quieter on the page
than 14977's comma-concatenation, `{ ... }` repetition, and `;` rule terminators. The
notation legend is duplicated at the top of
[grammar.ebnf](../grammar/grammar.ebnf) so that file is self-contained — a reader never
needs this doc to *read* the grammar, only to understand *why* it is shaped as it is.

## 3. Keywords are not reserved; matching is case-insensitive

The lexer has **no reserved-keyword table**: it emits one `identifier` token for every
bare word, and the parser recognises keywords purely by grammatical position, comparing
case-insensitively (`SELECT` = `select` = `SeLeCt`). Two consequences the grammar
encodes:

- A keyword spelling is a **legal identifier** wherever the grammar expects one — e.g. a
  column may be named `select`. There is no quoted-identifier escape because none is
  needed yet.
- Keyword terminals in the grammar (`"SELECT"`, `"FROM"`, …) denote a case-insensitive
  match, while punctuation terminals (`"("`, `"="`) match literally.

This is a CLAUDE.md §8 divergence hotspot: if one core folded case differently, or
reserved a word another did not, the corpus would diverge. Recording the rule in the
grammar keeps all cores honest. (Canonical *output* names — `int16` not `smallint` — are
a separate determinism rule owned by the type system, see [types.md](types.md) §2.)

## 4. Lexical edges: the minus operator and two-character operators

Two lexer facts are easy to get subtly wrong across cores, so the grammar pins them:

- **`-` is a unary/binary operator, not part of the literal.** An `integer` token is an
  *unsigned* magnitude of digits; `-5` is the unary-minus operator applied to `5`, and
  `- 5` with a space is now legal (it was a lex error when the sign was lexed into the
  literal). The parser folds unary-minus-of-a-literal into a single negative `Literal`
  value, so the negative-literal range checks (types.md §6) are unchanged.
  - **Magnitude range.** A magnitude must be `<= 2^63` (`9223372036854775808`); a larger
    one is a syntax error (`42601`), not a silent wrap. So that `int64`'s minimum is
    reachable, the lexer carries the magnitude *unsigned* (Rust `u64`, Go `uint64`, TS
    `bigint`) — `i64`/`int64` cannot hold `2^63`. The value `2^63` is in range **only** as
    the operand of unary minus, where it folds to `-9223372036854775808` (`int64::MIN`); a
    bare `2^63` fits no signed integer type and traps `22003` at resolve time (deterministic,
    before any row is scanned).
- **`<=` and `>=` are single tokens**, lexed greedily. The comparison operators are
  `=`, `<`, `>`, `<=`, `>=`; **`<>` and `!=` still do not exist** in this surface. The
  arithmetic operators `+ - * / %` are each single-character tokens; `*` is shared with the
  `SELECT *` glob and disambiguated by grammatical position (only the first select item).
- **A `.` makes a `number` a decimal literal** (§14), *or* is the `Dot` token of a qualified
  column reference (`t.col`, §15). The lexer scans one run of digits and, if a `.` follows (or
  leads, `.5`), continues into the fractional digits and emits a **decimal** token; with no `.`
  it emits an **integer** token. So the `2^63` magnitude bound applies to the *integer* form
  only — a decimal literal's size is bounded by `max_precision` / `max_scale` and checked at
  resolve (`22003`), not by `42601`. A second `.` in one number is a `42601`. A `.` that is
  **not** part of a number — i.e. with no digit immediately after it — is the standalone **`Dot`**
  token (`t.col`); the disambiguation is on the **following byte alone** (a digit after → numeric;
  else `Dot`), with **no preceding-token context**, so the rule is trivially byte-identical across
  the three lexers (§15). The lone overlap, an identifier immediately followed by `.<digit>`
  (`t.5`), is invalid either way (a column name is never numeric) and is left to lex as
  `<word> <decimal>` and rejected at parse. A bare `.` with no digit after and not between two
  identifiers is still a `42601`.

## 5. Deliberate narrowings (each relaxable later)

The current surface is intentionally minimal. Every omission below is a future feature,
tracked in [../../TODO.md](../../TODO.md), not an oversight:

- **Column aliases via explicit `AS` only** (`expr AS name`); see §8 for the output-name
  rule. Select-list **implicit** aliases (`expr name`, no `AS`) remain deferred, and `AS`
  aliasing in `ORDER BY` is not yet visible (ORDER BY resolves a bare/qualified table column).
  Before the joins slice the only `AS` in the surface was inside `CAST(expr AS type)` and a
  select-item alias; `table_ref` now adds the optional `AS` of a **table** alias (§15).
- **Single-table `UPDATE` / `DELETE`** — those two still take one table (no `JOIN`, no `USING`).
  `SELECT` is now **multi-table** via `JOIN` (§15): `INNER JOIN ... ON`, `CROSS JOIN`, and the
  `LEFT`/`RIGHT`/`FULL [OUTER] JOIN` family all execute. **Subqueries** (derived tables,
  `IN`/`EXISTS`, correlated) and **`USING`/`NATURAL`** join forms remain deferred.
- **`INSERT` values are *literals only*** (not general expressions; see the `literal`
  production) — but the `DEFAULT` keyword is now also a value slot, and an explicit **column
  list** (`INSERT INTO t (a, c) VALUES ...`) landed alongside `DEFAULT` (§12, §16). What stays
  deferred is `INSERT ... SELECT` and general expressions in a value slot.
- **`ORDER BY` keys are bare columns** — a sort key is a table column, never a general
  expression (`ORDER BY a + 1`), an output alias, or an ordinal position (`ORDER BY 1`);
  those stay deferred. The richer surface that *did* land — multiple keys, per-key
  `ASC` / `DESC`, and per-key `NULLS FIRST | LAST` — is described in §10.
- **`LIMIT` / `OFFSET` take a non-negative integer literal**, not a general expression
  (the same literal-only narrowing `INSERT` makes). The two clauses may appear in either
  order, each at most once (§9). There is **no `LIMIT ALL`**, **no `OFFSET … ROWS` /
  `FETCH NEXT … ROWS ONLY`**, and **no SQLite `LIMIT off, cnt` comma form**.
- **ASCII-only identifiers**, no quoted identifiers (§3).
- **Literal forms.** Integer, **decimal** (`1.50`, `.5` — §14), **single-quoted string**
  (the `text` type, `'alice'`, with `''` for an embedded quote), `TRUE`/`FALSE`, and `NULL`.
  Scientific `e`-notation for decimals (`1.5e3`) is **deferred**. `boolean` exists only as an
  *expression* type this slice — there are boolean literals and comparison/logical results,
  but no boolean *column* (see [types.md](types.md) §1).
- **Function calls — aggregates only.** The expression grammar now has a `function_call`
  production (`name ( * | expr )`), but it resolves **only** the five aggregate functions
  (`COUNT`/`SUM`/`MIN`/`MAX`/`AVG`; §17, [aggregates.md](aggregates.md)). **Scalar**
  functions (`length`, `lower`, …) and **`COUNT(DISTINCT x)`** stay deferred; an unknown
  function name is `42883`, and `DISTINCT` inside a call is `42601`.
- **No `;` statement terminator** and **no SQL comment syntax** in the input.
- **No parameter placeholders** (`$1`, `?`). The conformance corpus uses literal SQL by
  design — see [conformance.md](conformance.md); bound parameters are an
  implementation-API concern, not part of the parsed grammar.

## 6. Type names: an `identifier` plus an optional type modifier

The grammar parses a column's and a `CAST`'s type as a bare `identifier` — the catalog
owns the type lattice and resolves the name case-insensitively, rejecting unknowns at
execution time (`42704`). Keeping resolution out of the grammar means the scalar set can
grow ([types.md](types.md)) without touching the grammar, and a misspelled type yields a
clean structured error instead of a parse failure. The accepted names are listed as an
informative comment beside the `type_name` rule.

With `decimal` the rule gains an **optional parenthesized type modifier** —
`type_name ::= identifier ("(" integer ("," integer)? ")")?` — the engine's **first
parameterized type**. The grammar accepts the typmod *shape* for any type name (it is one
production), but the **semantics** are owned by resolution: a typmod is meaningful only for
`decimal`/`numeric` (precision, optional scale; §14), and a typmod on a type that takes
none — `int32(5)` — is rejected at resolve. Empty parens (`numeric()`) and a non-integer
inside are `42601`. This mirrors §6's standing split: the grammar stays small and
permissive about *shape*, the type system enforces *meaning*.

## 7. Growth rule

The grammar grows **one production at a time, spec-first**. When a feature lands it
edits this grammar and [grammar.ebnf](../grammar/grammar.ebnf) in the *same change* that
adds the parser code in all cores and the conformance entries that exercise it
(CLAUDE.md §10/§11). The general expression substrate — operator precedence,
parenthesization, integer arithmetic, the `boolean` type, and the `AND`/`OR`/`NOT`
connectives — landed together as the `expr` tower above; [../../TODO.md](../../TODO.md)
is the roadmap of what comes next (richer `ORDER BY`, more predicate forms, and onward).
Because the parser is hand-written rather than
generated, "conform to the grammar" is verified by cross-reading each production against
the three parsers and confirming every corpus statement is derivable from the grammar,
not by a generator step.

## 8. Output column names

Every result column has a **name**. The name is a determinism surface (CLAUDE.md §8): all
three cores must compute the byte-identical name for the same query, so the rule is fixed
here and asserted in the corpus via the `# names:` directive
([conformance.md](conformance.md) §1). The resolver derives each select item's name in
this order:

1. **`expr AS alias`** → the `alias`, **as written**. The alias is a pure output label, so
   it is *not* case-folded and *not* entered into any resolution namespace — WHERE,
   ORDER BY, and sibling select items never see it. Aliases may collide with a real column
   name or with each other (no uniqueness check); this is harmless precisely because they
   are never looked up.
2. **A bare column reference** (no alias) → the **catalog's canonical column name** at the
   resolved index, i.e. the spelling from `CREATE TABLE`, *not* the spelling typed in the
   SELECT. So with `c int32` declared, `SELECT C FROM t` names the column `c`. (Identifiers
   match case-insensitively — §3 — so the user's casing must not leak into the output.)
3. **`*`** → expands to each underlying column's canonical name, in column order — the same
   expansion that produces the projections.
4. **An un-aliased aggregate function call** → the **lowercased function name**
   (`COUNT(*)` and `COUNT(a)` → `count`, `SUM(x)` → `sum`, likewise `min`/`max`/`avg`),
   matching PostgreSQL (CLAUDE.md §1). This is the one expression form that gets a
   meaningful default name rather than `?column?`, because the name is the catalog
   surface lowercased — no expression printer is needed (§17, [aggregates.md](aggregates.md)).
5. **Any other un-aliased expression** (arithmetic, comparison, `CAST`, a literal, `IS NULL`,
   a unary/logical expression, …) → the fixed literal **`?column?`**.

Case 4 is deliberately a constant placeholder rather than a re-rendering of the expression
text. Echoing normalized SQL text (the SQLite behaviour) would require a canonical
expression printer that is byte-identical across Rust, Go, and TS — a new §8 divergence
hotspot for no present benefit. A column whose name matters can be given one with `AS`. A
normalized-name printer remains a possible later refinement.

## 9. `LIMIT` / `OFFSET`

`LIMIT n` caps the result at `n` rows; `OFFSET m` skips the first `m`; together they skip
`m` then take `n`. The grammar (`limit_offset`) accepts the two clauses in **either order**
and **each at most once** — `LIMIT n OFFSET m` and `OFFSET m LIMIT n` are equivalent, and a
duplicate (`LIMIT 1 LIMIT 2`) is a syntax error (`42601`). PostgreSQL accepts both orders;
matching it costs only a tiny order-independent parse loop and avoids a gratuitous
incompatibility.

**Where it applies.** The slice runs **after `ORDER BY` and before projection**, the only
correct point: the rows must be filtered and ordered before "the first `n`" is meaningful,
and slicing before projection means the skipped/excluded rows never accrue `row_produced`
or projection cost. So `OFFSET`/beyond-`LIMIT` rows are scanned and filtered (they pay
`storage_row_read` + filter `operator_eval`) but **not produced** — the cost contract falls
straight out of the existing `row_produced`-at-projection rule
([cost.md](cost.md) §3), with the slice itself unmetered like the sort. Output column names
are derived from the select list and are unaffected by the window (§8).

**The count is a non-negative integer literal**, not a general expression (§5). This is a
determinism surface (CLAUDE.md §8): the sign is known at parse time, so a negative count is
rejected **before any row is scanned** with a precise structured error — `2201W`
(`invalid_row_count_in_limit_clause`) for `LIMIT`, `2201X`
(`invalid_row_count_in_offset_clause`) for `OFFSET` ([../errors/registry.toml](../errors/registry.toml)),
the PostgreSQL SQLSTATEs. The value `-0` folds to `0` and is accepted. The shared integer
lexer's magnitude rules still hold: a magnitude `> 2^63` is a `42601` syntax error, and a
positive magnitude of `2^63` (over `int64`'s max) traps `22003` (§4). `LIMIT 0` is valid and
yields the empty result; an `OFFSET` past the end yields the empty result.

Without `ORDER BY`, **which rows a `LIMIT` returns is unspecified** — `LIMIT` windows an
unordered result, so it selects an arbitrary subset (SQL-standard and PostgreSQL behavior —
CLAUDE.md §1/§8). To pin *which* rows (not just how many), add an `ORDER BY` that fully
determines the order; the corpus does this for every `LIMIT`/`OFFSET` query whose specific
rows are asserted.

## 10. `ORDER BY`

`ORDER BY` is **one or more sort keys** (`order_by` / `sort_key` in the grammar), each a
**bare table column** with an optional direction (`ASC` / `DESC`, default `ASC`) and an
optional explicit NULL placement (`NULLS FIRST | LAST`). Keys apply **left to right**: the
first is primary, the next breaks its ties, and **a full tie across all keys is broken by the
primary key** — so `ORDER BY` fixes the order *completely*, ties included. (That last tie-break
is a deliberate, documented determinism choice beyond the SQL standard — CLAUDE.md §8/§10:
unlike row order *without* `ORDER BY` (now unspecified), order *under* `ORDER BY` is fully
deterministic. Today it is realized by a **stable** sort over the primary-key scan; under
future parallel execution it is the same observable result via an implicit primary-key
tie-break, so it stays parallelism-compatible.) Resolution is against the *table's* columns and
is independent of the select list — an `AS` alias is invisible here (§8), and a key need not
appear in the projection.

**Still narrowed (§5):** a key is a column name only — not a general expression
(`ORDER BY a + 1`), an output alias, or an ordinal (`ORDER BY 1`). `expect_identifier` (not the
expression parser) consumes each key, so those forms are a `42601` syntax error; all remain
relaxable later.

**NULL placement and the default.** The physical key order ratifies NULL as the **largest**
value ([types.md](types.md) §4, `null_ordering = "nulls-last-ascending"` in
[../types/compare.toml](../types/compare.toml)): NULLs sort last ascending, and descending
inverts that to first. So when a key gives **no** `NULLS` clause the default **follows the
direction** — `ASC` → `NULLS LAST`, `DESC` → `NULLS FIRST` — and a plain `ORDER BY col` mirrors
the engine's index-iteration order. This is the **PostgreSQL** model (NULL is the largest
value, PG defaults `ASC` to `NULLS LAST`), reached under the standing "match PostgreSQL unless
there's an overriding reason" guideline (CLAUDE.md §1); it is a deliberate **divergence from
SQLite**, where NULL is the *smallest* value (SQLite defaults `ASC` to `NULLS FIRST`). An
**explicit** `NULLS FIRST | LAST` overrides the default **regardless of direction** (so
`ORDER BY a ASC NULLS FIRST` keeps non-NULL values ascending but lifts NULLs to the front).

This makes NULL placement a CLAUDE.md §8 determinism surface: the per-key comparator must keep
NULL placement **decoupled** from the value-direction reversal (the `nulls_first` flag is
resolved at parse time to `explicit ? … : descending` and applied independently of the
`ASC`/`DESC` value flip), so all three cores order NULLs byte-identically. The sort itself is
**unmetered**, like `LIMIT`/`OFFSET` slicing ([cost.md](cost.md) §3); only the scanned and
produced rows accrue cost.

## 11. `DISTINCT`

`SELECT DISTINCT` removes duplicate rows from the result by **deduplicating the projected
output** — the select-list values, *not* the source rows. So `SELECT DISTINCT a FROM t`
collapses rows that share an `a` even when their other columns differ, and
`SELECT DISTINCT a, b` keys on the `(a, b)` pair. `DISTINCT` with no qualifier is the only
form; `DISTINCT ON (...)` (the PostgreSQL extension) is out of scope.

**Where it applies — before the window, after the sort.** Dedup is the SQL "is this output
row new?" step, so it must run on projected values and **before** `LIMIT`/`OFFSET`:
`SELECT DISTINCT x FROM t LIMIT 2` returns *two distinct* rows, so the window slices the
**distinct** rows, not the scanned rows. This is the reverse of the un-`DISTINCT` pipeline
(which windows the sorted source rows and projects last). The executor keeps the existing
`ORDER BY` sort of the source rows, then — when `DISTINCT` is set — projects every filtered
row, dedups by **first occurrence**, windows the distinct rows, and emits.

**NULL-safe equality.** Two rows are duplicates under the engine's NULL-safe equality (the
`IS NOT DISTINCT FROM` semantics — [functions.md](functions.md) §3, [types.md](types.md) §4),
*not* the three-valued `=`: two NULLs **are** the same for `DISTINCT`, so all-NULL rows
collapse to one. This is the standard SQL `DISTINCT` rule and the same total NULL handling
the engine already uses for `IS [NOT] DISTINCT FROM`.

**Output order follows the general rule** (CLAUDE.md §8/§10). With no `ORDER BY`, the distinct
rows come out in an **unspecified order** (the corpus compares them `rowsort`); the *set* of
distinct rows is of course exact and identical across cores. With `ORDER BY`, the keys order
the distinct rows; a tie on all keys keeps the **stable first-occurrence order** over the
source scan — the same retained determinism `ORDER BY` has generally (§10).

**`ORDER BY` under `DISTINCT` — the PostgreSQL restriction.** Once duplicates collapse, an
`ORDER BY` key that is *not* in the select list no longer has a single value per output row
(which of the merged rows' values would it use?). So, matching PostgreSQL, **every `ORDER BY`
key must appear as a bare column in the select list** (or the list is `*`); otherwise it is
`42P10` (`invalid_column_reference`, [../errors/registry.toml](../errors/registry.toml)),
*"for SELECT DISTINCT, ORDER BY expressions must appear in select list."* An alias does not
satisfy this — `ORDER BY` resolves against table columns, never aliases (§8), so
`SELECT DISTINCT a AS b FROM t ORDER BY b` orders by the real column `b` and is rejected
unless `b` is itself bare-projected, while `SELECT DISTINCT a AS x FROM t ORDER BY a` is
accepted (`a` is bare-projected; the alias is just its output label). This is one more place
the engine follows PostgreSQL, alongside its **PostgreSQL NULL ordering** (NULL largest,
ASC → NULLS LAST, §10).

**`DISTINCT` is not a reserved word** (§3): a column may be named `distinct`, and
`SELECT distinct FROM t` must keep selecting it. Because `DISTINCT` is the lone modifier
*before* the select list, the parsers resolve it with a **two-token lookahead** — the leading
`DISTINCT` is the modifier iff the next token is **not** `FROM` and not end-of-input. So
`SELECT DISTINCT a FROM t` is the modifier, `SELECT distinct FROM t` is the column,
`SELECT DISTINCT distinct FROM t` is the modifier over a column named `distinct`, and
`SELECT DISTINCT FROM t` (the only valid parse being the column) selects `distinct`. This
lookahead is a CLAUDE.md §8 determinism surface: it must be byte-identical across the three
hand-written parsers.

## 12. Multi-row `INSERT`

`INSERT INTO t VALUES (...)` accepts **one or more** parenthesized rows
(`insert` / `row` in the grammar): `INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)` inserts
three rows in one statement. It is the obvious PostgreSQL surface and a near-free extension
of the single-row form — one extra parse loop and one validation pass. The optional **column
list** and the **`DEFAULT` keyword** are covered in §16; `INSERT ... SELECT` and general
expressions in a value slot stay deferred (§5, [../../TODO.md](../../TODO.md)).

**Every row has the same arity.** Each `row` is validated against the catalog independently; a
row whose arity differs from the column count (or, with a column list, the list length) is a
syntax error (`42601`), the same code the single-row form already raised for a count mismatch.
The column list (if any) is shared by all rows, so all rows necessarily map to the same column
set.

**Two-phase / all-or-nothing — the UPDATE precedent.** A multi-row `INSERT` is atomic per
statement, mirroring `UPDATE`'s two-phase pass (CLAUDE.md §11 step 6) and PostgreSQL: the
engine **fully validates every row before inserting any**. Phase one checks each row's arity,
type-checks and range-checks every value (an out-of-range integer traps `22003`, a `NULL`
into a `NOT NULL` column traps `23502`), computes each row's storage key, and checks that key
for a duplicate — **both against the already-stored rows and against earlier rows in the same
statement** (a collision traps `23505`). Only once all rows pass does phase two insert them.
So `INSERT INTO t VALUES (1, 5), (1, 6)` (a key repeated *within* the batch) traps `23505`
and stores **nothing**, and a batch whose third row overflows leaves the first two unstored.
This matters because the §3 staging buffer is still future: without the pre-validation pass a
mid-batch failure would leave a partial insert, breaking statement atomicity. Validation is
left-to-right by row then by column, so the *first* failing row's error wins
deterministically (CLAUDE.md §8/§10).

**Synthetic rowids are allocated in phase two, in row order.** For a table with no primary
key, each row's key is a fresh monotonic rowid (CLAUDE.md §11 step 6). Allocation happens in
phase two, after every row has validated, and proceeds in `VALUES` order — so a batch that
fails validation burns no rowids, and a batch that succeeds assigns consecutive rowids
left-to-right. This keeps the assignment deterministic and identical across the three cores.

**Cost is unchanged — zero.** A row's values are literals and **pre-evaluated constant
defaults** (folded to a value at CREATE TABLE — §16), so an `INSERT` reads no storage and
evaluates no expression tree: it accrues the same zero cost as before
([cost.md](cost.md)). Only a future *expression* default would change this.

## 13. `DROP TABLE`

`DROP TABLE t` removes a table — **its definition and all its rows** — from the catalog
(`drop_table` in the grammar). It is the inverse of `CREATE TABLE`: where CREATE registers
a name in the catalog (and rejects a name already taken, §1, `42P07`), DROP removes one
(and rejects a name not present). Both stores the table touches — the catalog entry and the
per-table row store — are dropped together, keyed by the table's lower-cased name (§3,
case-insensitive: `DROP TABLE T` drops `t`). After a drop the name is free again, so
`DROP TABLE t` then `CREATE TABLE t (...)` re-creates it from empty.

**A missing table is an error — no `IF EXISTS`.** Dropping a table that does not exist
raises `42P01` (`undefined_table`, *"table does not exist: t"*) — the same code a
`SELECT` / `INSERT` / `UPDATE` / `DELETE` against an unknown table already raises. This
mirrors `CREATE TABLE`'s `42P07`-on-duplicate (§1) and matches PostgreSQL's bare
`DROP TABLE`. The idempotent **`IF EXISTS`** form (PostgreSQL turns the missing-table error
into a notice) is **deliberately deferred** this slice, kept symmetric with the
still-missing `CREATE TABLE IF NOT EXISTS`; both `IF [NOT] EXISTS` forms can land together
later ([../../TODO.md](../../TODO.md)).

**Deliberate narrowings (each relaxable later, §5).** As with the rest of the surface, the
form is minimal:

- **One table per statement** — no `DROP TABLE a, b, c`. (When multi-table drop lands it
  inherits the two-phase / all-or-nothing discipline §12 uses for multi-row work: validate
  every name exists before removing any.)
- **No `CASCADE` / `RESTRICT`** — there are no dependent objects yet (no views, foreign
  keys, or secondary indexes), so PostgreSQL's default `RESTRICT` is vacuous and the
  keywords are simply not part of the surface. They become meaningful only once
  dependencies exist (Phase 4, [../../TODO.md](../../TODO.md)).

**Cost is zero.** Like `CREATE TABLE`, a drop reads no rows and evaluates no expression
tree — it is a pure catalog edit — so it accrues zero cost ([cost.md](cost.md)). Removing a
populated table does **not** charge per dropped row: the cost model meters query/row
*work*, and a drop discards the store wholesale rather than scanning it.

**Persistence.** Within a session the drop mutates the live catalog directly (the §3
single-committed-state model; the staging-buffer commit is still future), exactly as
`CREATE TABLE` and the DML statements do today. On the whole-image on-disk format
([../fileformat/format.md](../fileformat/format.md)) a subsequent commit simply serializes
the post-drop catalog, so the dropped table's bytes are gone from the next image — no
free-list or page-reclamation work is involved (that is deferred until incremental
copy-on-write, Phase 6).

## 14. Decimal literals and the `numeric(p,s)` type modifier

The `decimal` type ([types.md](types.md) §12, [decimal.md](decimal.md)) adds two pieces of
surface syntax, both pinned here because they are CLAUDE.md §8 determinism surfaces the three
hand-written lexers/parsers must agree on byte-for-byte.

**The decimal literal** (`decimal` token, §4). A numeric literal containing a `.` is a
decimal: `1.5`, `1.50`, `1.`, `.5`, `0.00`. Its written form fixes **both** its value and its
**scale** — `1.50` is the coefficient `150` at scale `2`, distinct in *display* from `1.5`
(scale `1`) though equal in *value*. `1.` is the integer `1` at scale `0`; `.5` is `5` at
scale `1` (an empty integer part reads as `0`). Like a bare integer literal, a decimal literal
is an **untyped constant** that adapts to its context ([types.md](types.md) §6, extended to
decimal): into a `numeric(p,s)` target it is rounded to scale `s` (half away from zero) and
precision-checked (`22003`); with no decimal context it keeps its written scale. A decimal
literal against an **integer** column is well-typed (the integer promotes to decimal —
[../types/compare.toml](../types/compare.toml)), so `WHERE int_col = 1.5` simply never matches
rather than erroring; but a decimal literal **stored into** an integer column is a `42804`
type error (the strict matrix has no decimal→integer assignment cast —
[../types/casts.toml](../types/casts.toml)). Scientific `e`-notation (`1.5e3`) is **deferred**;
a coefficient beyond `max_precision` significant digits, or a scale beyond `max_scale`
([../types/scalars.toml](../types/scalars.toml)), traps `22003` at resolve.

**The `numeric(p,s)` type modifier** (§6). `numeric` (unconstrained), `numeric(p)`
(= `numeric(p,0)`), and `numeric(p,s)` are the three forms, in both a column type and a
`CAST` target. `p` is the total significant digits (`1 ≤ p ≤ 1000`) and `s` the digits after
the point (`0 ≤ s ≤ p`); an out-of-range or malformed typmod — `numeric(0)`, `numeric(1001)`,
`numeric(5,7)` — traps **`22023`** (`invalid_parameter_value`,
[../errors/registry.toml](../errors/registry.toml)), PostgreSQL's SQLSTATE. The grammar
accepts the typmod shape on *any* type name (one production, §6); a typmod on a type that
takes none (`int32(5)`, `text(10)`) is a resolve-time error this slice (`0A000` — `varchar(n)`
length limits and other parameterized types are deferred, [types.md](types.md) §11). The
limits, the p/s interaction (integer-part digits ≤ `p − s`), and the rounding-on-coercion rule
are the type system's, detailed in [decimal.md](decimal.md) §2–3; the grammar fixes only that
the *syntax* is `identifier "(" integer ("," integer)? ")"`.

## 15. Multi-table `FROM` and `JOIN`

The `SELECT` `FROM` clause grows from a single table name to a **left-deep chain** —
`from_clause ::= table_ref join_clause*` — adding table aliases, qualified column references
(`t.col`), and the first multi-table relational operators. The engine executes **`INNER JOIN
... ON`**, **`CROSS JOIN`**, and the **`LEFT`/`RIGHT`/`FULL [OUTER] JOIN`** family (outer joins
landed as an executor-only follow-on — see "Outer joins" below). The reasoning lives here; the
cost contract is in [cost.md](cost.md) §7.

**Table references and aliases.** `table_ref ::= identifier ("AS"? identifier)?` — a table name
with an optional alias, the `AS` optional (`FROM orders o` = `FROM orders AS o`). The alias, or
the table name when there is none, is the relation's **label**. Labels qualify columns and must
be **distinct**: two relations with the same label — a self-join written without aliases
(`FROM t JOIN t ...`) — is **`42712`** (`duplicate_alias`, *"table name t specified more than
once"*, [../errors/registry.toml](../errors/registry.toml)), matching PostgreSQL. A self-join is
therefore written with two distinct aliases (`FROM t AS a JOIN t AS b ON ...`). Comparison is
case-insensitive (§3), like every other identifier.

**Qualified column references.** `column_ref ::= identifier ("." identifier)?` replaces the bare
`identifier` in `primary` (and in `sort_key`, so `ORDER BY t.a` parses). The `.` is the **`Dot`**
token (§4). Resolution (the executor's `Scope` — an ordered list of `(label, table, column
offset)`):

- A **bare** `col` is searched across **every relation in scope**: **no** match is `42703`
  (`undefined_column`), a match in **two or more** relations is **`42702`** (`ambiguous_column`,
  *"column reference col is ambiguous"*, a new code), **exactly one** match resolves.
- A **qualified** `rel.col` names exactly one relation: an unknown `rel` is `42P01`
  (`undefined_table`, reused — *"missing FROM-clause entry for table rel"*), a known `rel` with
  no `col` is `42703`. A qualified reference is **never** ambiguous. The qualifier never appears
  in the **output name** (§8) — `SELECT t.c` names the column `c`, its catalog canonical name.

`SELECT *` expands across **all** relations in FROM order, each relation's columns in catalog
order (PostgreSQL behaviour); duplicate output names across tables are allowed (the `# names:`
directive asserts them positionally). There is **no `t.*`** qualified-star this slice.

**The join operators.** `join_clause ::= "CROSS" "JOIN" table_ref | join_type? "JOIN" table_ref
"ON" expr`. A bare `JOIN` is `INNER` (the keyword optional). The `ON` predicate is a general
expression that **must resolve to boolean** — a non-boolean `ON` is `42804`, the same rule WHERE
takes — and is evaluated **at the join** over the combined (left-concatenated-with-right) row;
only a **TRUE** result keeps the pair (three-valued, so a `NULL` join key never matches, matching
PostgreSQL inner-join semantics). `CROSS JOIN` is the Cartesian product (no `ON`). An `INNER`/bare
`JOIN` with **no `ON`** is `42601` (this slice requires it; `USING`/`NATURAL` are deferred), and a
`CROSS JOIN ... ON ...` is likewise `42601`.

Evaluating each `ON` **at its own join node** (not folding all `ON`s into the trailing WHERE) is
deliberate: for INNER it is observationally identical to a WHERE, but it is the executor shape the
deferred OUTER joins need (an unmatched row is NULL-extended *at the node*, before any later
filter — the classic ON-vs-WHERE distinction). WHERE stays the separate trailing filter it
already is. With **no** `ORDER BY` the join's output order is **unspecified** (CLAUDE.md §8/§10
— the corpus compares such joins `rowsort`); the produced row *set* is exact and identical
across cores. Add `ORDER BY` to pin a sequence.

**Keywords stay non-reserved (§3).** `JOIN`, `INNER`, `CROSS`, `ON`, `LEFT`, `RIGHT`, `FULL`,
`OUTER`, and `AS` are **not** reserved — a column or table may be named any of them. The
hand-written parsers disambiguate **positionally**, the same mechanism `DISTINCT`/`AS` already
use, and the lookahead must be **byte-identical** across cores (a CLAUDE.md §8 surface):

- The `FROM` loop, after a `table_ref`, treats the next word as a join keyword only when it
  begins a `join_clause` — `CROSS`/`INNER`/`LEFT`/`RIGHT`/`FULL` immediately followed by the
  `JOIN` keyword (a two-token lookahead), or a bare `JOIN` immediately following the `table_ref`.
  Any other word ends the `FROM` clause (it must be `WHERE`/`ORDER`/`LIMIT`/`OFFSET` or EOF).
- A `table_ref`'s **implicit** alias is taken only when, after the table name, the next token is
  a word that is **not** a clause/join keyword (`as`/`where`/`order`/`limit`/`offset`/`on`/`join`/
  `inner`/`cross`/`left`/`right`/`full`/`outer`); an explicit `AS` takes the next identifier
  unconditionally. So `FROM t WHERE ...` (no alias) and `FROM t x JOIN ...` (alias `x`) both parse.
  This is the same precedent as the select-item `AS` and the `SELECT DISTINCT` two-token lookahead.

**Outer joins (`LEFT`/`RIGHT`/`FULL [OUTER] JOIN`).** An outer join preserves rows that an inner
join would drop, **NULL-extending the absent side**. The `OUTER` keyword is optional noise
(`LEFT JOIN` = `LEFT OUTER JOIN`). It is an **executor-only** addition over the INNER/CROSS slice —
the grammar, AST, and parser already carried the join kind, and the flat-row model (a joined row is
each relation's row concatenated) plus the per-node three-valued `ON` already support it; no
grammar/AST/parser reshape was needed. Semantics (PostgreSQL by default, [../../CLAUDE.md](../../CLAUDE.md) §1):

- **`LEFT`** keeps every left (running) row: a left row that matches no right row is emitted once with
  every right-side column **NULL**. **`RIGHT`** is the mirror (every right row kept, left side
  NULL-extended). **`FULL`** keeps both — matched pairs, then unmatched-left rows, then unmatched-right
  rows. In a left-deep chain the "left" side of join *k* is the **entire accumulated result** of the
  joins before it, so a RIGHT/FULL join NULL-extends *all* prior columns; the pad widths come from the
  scope (the right relation's flat offset and column count), so an empty intermediate result pads
  correctly rather than crashing.
- **The `ON` is three-valued and unchanged.** Only a `TRUE` result is a match; a NULL join key (or any
  `NULL`/`FALSE` `ON`) is a non-match, so in an outer join it **NULL-extends** exactly as it is dropped
  in an inner join. Outer joins evaluate `ON` over the same candidate set as the inner join would, so
  their cost matches except for the extra preserved rows ([cost.md](cost.md) §3).
- **`WHERE` still applies after the join**, to the combined rows including the NULL-extended ones — so a
  `WHERE` predicate on the nullable side (`WHERE b.x = 5`) sees `NULL` for an unmatched row and drops it,
  the familiar PostgreSQL behavior where a `WHERE` on the outer side effectively downgrades the outer
  join to an inner one; put the predicate in the `ON` to preserve the unmatched rows, or test the
  nullable key with `IS NULL` for an anti-join. No special-casing — column resolution is positional and
  never folds on a column's declared nullability.

**Deliberate narrowings (each relaxable later, [../../TODO.md](../../TODO.md)).**

- **No comma-`FROM`.** `FROM a, b` (the old implicit cross join) is **dropped**, not deferred:
  `CROSS JOIN` covers the same semantics and comma-`FROM`'s precedence-vs-`JOIN` interaction is a
  future trap. A `,` after the first `table_ref` is a `42601`.
- **No `USING` / `NATURAL`** join forms (they need column-name matching / merge semantics), **no
  `t.*`** qualified-star, **no parenthesized / derived-table FROM**, **no subqueries**.
- **`UPDATE` / `DELETE` stay single-table** — they keep one table name and gain nothing here
  (though a qualified `WHERE t.a = 1` referencing their sole table now resolves, harmlessly).

## 16. `INSERT` column list and the `DEFAULT` keyword

`INSERT` gained two related, PostgreSQL-faithful surfaces (`insert` / `insert_value` in the
grammar) so a column can be **omitted** and take its `DEFAULT` ([constraints.md](constraints.md)
§2). The constraint semantics — when a default is evaluated, the `DEFAULT NULL`/`NOT NULL`
interaction — live in that doc; this section is the grammar/mapping rule.

**The optional column list** names the target columns: `INSERT INTO t (a, c) VALUES (1, 3)`.
The values map to the *named* columns, in list order (not declaration order), so the list may
reorder and may omit columns. With **no list**, the values map positionally to every column in
declaration order (the prior behavior). Either way the engine builds each stored row in
declaration order; a column that the list omits takes its default, else NULL, else `23502` if
it is `NOT NULL`.

**The `DEFAULT` keyword** is a value slot: `INSERT INTO t VALUES (1, DEFAULT, 'x')` puts the
target column's declared default in that position (or NULL, then `23502` if NOT NULL and no
default). It works at any position, including under a reordering column list. `DEFAULT` is not
reserved (§3) — a column may be named `default`; it is a keyword only in a value slot.

**Errors, resolved deterministically left-to-right.** Statement-level, once before the rows: an
unknown column name in the list is `42703` (`undefined_column`), a column named twice is `42701`
(`duplicate_column`). Per row: an arity that does not match the list length (or, with no list,
the column count) is `42601`. Then the usual per-value checks apply in declaration order
(`22003` / `42804` / `23502`), inside the same two-phase / all-or-nothing pass as §12.

**Defaults are literal-only this slice** and pre-evaluated at CREATE TABLE, so applying one at
INSERT is substituting a constant — no expression is evaluated and cost stays zero (§12). A
general-expression default (`DEFAULT now()`) and `INSERT ... SELECT` stay deferred
([../../TODO.md](../../TODO.md)).

## 17. Function-call syntax and aggregate functions

The `primary` rule gains a `function_call` production — `function_call ::= identifier "("
( "*" | expr ")"` — the engine's **first** call syntax. The *semantics* (what each
aggregate computes, the SUM/AVG widening, the NULL / empty-set rules, the grouping rules)
live in [aggregates.md](aggregates.md); this section is the **syntax** and the
disambiguation, the established grammar.md/semantics split (§15 does the same for joins).

**Aggregates only.** Only the five aggregate functions resolve — `COUNT`, `SUM`, `MIN`,
`MAX`, `AVG` ([../functions/catalog.toml](../functions/catalog.toml), `kind = "aggregate"`).
Any other function name is **`42883`** (`undefined_function`,
[../errors/registry.toml](../errors/registry.toml)), resolved like an unknown type name
(§6). **Scalar** functions are a later slice (they will fit the operator result/NULL mold —
[functions.md](functions.md) §8); aggregates do not, so they are their own catalog kind.

**The argument.** `COUNT(*)` is the row counter — the `*` argument is accepted **only** by
`COUNT`; `*` to any other aggregate is a resolve error. Every other aggregate takes exactly
one **general expression** (`SUM(a + 1)`, `MIN(t.c)`). `COUNT(expr)` also takes one
expression. There are no zero-argument or multi-argument aggregates this slice, and
**`DISTINCT` inside the parens** (`COUNT(DISTINCT x)`) is **deferred** — the parsers reject
the `DISTINCT` token in an argument position as `42601` (it is added as a follow-on).

**The `*` token.** `*` is the same token as the `SELECT *` glob and the `mul` operator,
disambiguated by position (§4): inside a function call's argument it is the `COUNT(*)`
row-count form, nothing else. A `*` argument to a non-`COUNT` aggregate, or `*` mixed with
other arguments, never parses to anything meaningful and is rejected.

**Names are not reserved — a one-token lookahead.** Like `DISTINCT`/`AS`/the join keywords
(§3, §11, §15), aggregate names are ordinary identifiers: a column may be named `count`,
and `SELECT count FROM t` must keep selecting it. In `primary`, after reading an
`identifier`, the parser peeks **one** token — if it is `"("`, this is a `function_call`;
otherwise it falls back to `column_ref`. So `SELECT count FROM t` is the column (no `(`
follows) and `SELECT count(*) FROM t` is the aggregate. The lookahead is a CLAUDE.md §8
determinism surface — **byte-identical** across the three hand-written parsers. A qualified
name followed by `(` (`t.count(...)`) is **not** a call (the call form binds only a *bare*
identifier immediately followed by `(`); it is left to fail as a malformed reference.

**Where aggregates may not appear.** An aggregate folds a *set* of rows, so it is undefined
per input row: an aggregate in a `WHERE` clause, a `JOIN ON`, or a `GROUP BY` key, and an
aggregate **nested** in another aggregate, are all **`42803`** (`grouping_error`). Filtering
on an aggregate is `HAVING`'s job (a later slice). The output name of an un-aliased aggregate
is its lowercased function name (§8).

## 18. `GROUP BY`

`group_by ::= "GROUP" "BY" column_ref ("," column_ref)*` slots between `WHERE` and
`ORDER BY`. It partitions the post-`WHERE` rows into groups sharing a value on every grouping
key and produces **one result row per group**; the select list then projects the grouping
keys and aggregates over each group. Semantics live in [aggregates.md](aggregates.md) §5–§6;
this section is the syntax + the two narrowings.

**Keys are bare/qualified columns only.** A grouping key is a `column_ref` (`g` or `t.g`) —
**not** a general expression (`GROUP BY a + 1`), an output alias, or an ordinal
(`GROUP BY 1`). This is exactly the `ORDER BY` narrowing (§5/§10), and relaxable later. A
key that names no column is `42703`; an ambiguous bare key across joined relations is
`42702` (the usual column resolution, §15).

**The grouping-error rule.** With `GROUP BY` present, every column in the select list (and
in `HAVING`/`ORDER BY`) that is **not** inside an aggregate must appear among the grouping
keys, else **`42803`** (`grouping_error`). Membership is by resolved column identity (the
flat index), so `SELECT g, COUNT(*) … GROUP BY g` is legal and `SELECT g, a … GROUP BY g`
is `42803` on `a`. The PostgreSQL functional-dependency relaxation (a column dependent on a
grouped primary key) is **deferred** — the rule is a simple set-membership check. This
generalizes the no-`GROUP BY` degenerate case (§17: with no keys, only aggregates and
constants are legal outside an aggregate).

**Group emission order and NULL/decimal grouping** (CLAUDE.md §8/§10): with no `ORDER BY`,
groups emit in an **unspecified order** (the corpus compares them `rowsort` or adds an explicit
`ORDER BY`). The grouping *itself* stays deterministic and semantic, independent of emission
order: `NULL` forms its **own single
group** (NULL groups with NULL — the NULL-safe equality `DISTINCT` uses, not three-valued
`=`), and `decimal` keys bucket by **value-canonical** form (`1.5` and `1.50` share one
group — [decimal.md](decimal.md) §5); the group's displayed key is the first occurrence's
value. `GROUP BY` over an **empty** table produces **zero** rows (contrast §17's
whole-table single row). The grouping itself is unmetered, like the sort and `DISTINCT`
dedup ([cost.md](cost.md) §3); `row_produced` is charged per emitted group.

**`ORDER BY` over the grouped output.** In an aggregate query, an `ORDER BY` key resolves
against the **grouping keys** (not the raw FROM columns): a key that is a grouping column
sorts the output rows by that group value; a key that is **not** a grouping column is
`42803` (the grouping-error rule again). The sort runs on the group rows after aggregation,
before `LIMIT`/`OFFSET`. (`ORDER BY` by an aggregate or an ordinal stays out — `sort_key` is
a bare `column_ref`, §10.) **Still narrowed:** `SELECT DISTINCT` in an aggregate query
(needs output-row dedup) is deferred (`0A000`).

## 19. `HAVING`

`having_clause ::= "HAVING" expr` slots between `GROUP BY` and `ORDER BY`. It is a boolean
predicate over the **grouped** rows — evaluated **after** grouping/aggregation and **before**
`ORDER BY` — keeping a group iff the predicate is `TRUE` (three-valued: `FALSE`/`NULL` drop
it, like `WHERE`). Semantics live in [aggregates.md](aggregates.md) §8; this is the rule:

- **It may reference aggregates and grouping keys.** `HAVING COUNT(*) > 1` and
  `HAVING SUM(x) > 10` are the point of the clause — filtering on an aggregate is what `WHERE`
  *cannot* do (`WHERE` filters input rows, before grouping; an aggregate there is `42803`,
  §17). A HAVING aggregate need **not** appear in the SELECT list (it is computed regardless).
  A non-aggregated column that is not a grouping key is `42803` — the same grouping-error rule
  as the SELECT list (§18). It resolves against the same synthetic group row, so its
  aggregates collect into the same set and its column references map to grouping-key slots.
- **It must resolve to boolean** (`42804` otherwise), exactly like `WHERE`.
- **HAVING with no `GROUP BY`** filters the single whole-table group: `SELECT COUNT(*) FROM t
  HAVING COUNT(*) > 5` yields one row or zero. **HAVING makes a query an aggregate query**
  even with no `GROUP BY` and no select-list aggregate (`SELECT a FROM t HAVING true` is
  `42803` on `a`, just as a bare `a` alongside an aggregate is).
- **Cost** ([cost.md](cost.md) §3): the HAVING predicate's `operator_eval`s are charged per
  **group** evaluated (every group, since the filter must test each); a dropped group then
  charges no `row_produced` — the same project-vs-produce asymmetry `DISTINCT` has. The filter
  itself (the keep/drop decision) is unmetered.

## 20. `IN (list)` / `NOT IN`

`x IN (v1, v2, …)` is the **membership predicate**: TRUE iff `x` equals any list element. It
extends `comparison` with `"NOT"? "IN" "(" additive ("," additive)* ")"`, a **non-associative
postfix form at the comparison level** (precedence 35), alongside `=`/`IS NULL`/`IS DISTINCT
FROM`. `NOT IN` is the negation. The whole form is the first of the Phase-2 predicate forms
(§20–§23); it is built on the Phase-1 expression substrate and adds no new value type.

- **Semantics = the OR-chain PostgreSQL defines it as.** `x IN (a,b,c)` is exactly
  `x = a OR x = b OR x = c`, and `x NOT IN (a,b,c)` is its negation `NOT (x = a OR …)` =
  `x <> a AND x <> b AND …`. The engine **desugars** to that tree at resolve time
  (`Expr::In` → `Binary{Or, …}` of `Binary{Eq}`, wrapped in `Unary{Not}` when negated), so
  every property below is inherited from `=`/`OR`/`NOT` rather than re-specified.
- **Three-valued NULL** falls out of the Kleene OR. A NULL `x`, or a non-matching list with a
  NULL element, yields UNKNOWN (rendered NULL): `1 IN (2, NULL)` is NULL, `NULL IN (1,2)` is
  NULL. But a matching element still wins (TRUE dominates): `1 IN (1, NULL)` is TRUE. `NOT IN`
  propagates the same way: `1 NOT IN (2, NULL)` is NULL, `1 NOT IN (1, NULL)` is FALSE. This is
  the classic SQL `NOT IN`-with-NULL gotcha, and it matches PostgreSQL by construction.
- **Per-element typing** reuses the `=` operand contract (the promotion tower + literal
  adaptation): each element is paired with `x`, so a bare integer literal element adapts to
  `x`'s type and a value too wide for it traps **22003** at resolve (`int16col IN (100000)`),
  and a cross-family element (`intcol IN (1, 'a')`) is **42804**. A decimal element compares by
  exact value (`1.5 IN (1.50)` is TRUE), an int↔decimal mix promotes, text compares by the `C`
  collation.
- **The list is non-empty.** `x IN ()` is a **42601** syntax error (the parser requires at
  least one element; PostgreSQL rejects the empty list too). The `IN (subquery)` form is a
  separate, later feature (Phase 4 subqueries) — this slice is the value-list form only.
- **Precedence narrowing.** PostgreSQL binds `IN` slightly tighter than the comparison
  operators; the engine collapses it into the single non-associative comparison level. This is
  unobservable here because chaining comparisons (`a = b IN (…)`) is already a 42601 syntax
  error regardless of the relative precedence.
- **Cost** ([cost.md](cost.md) §3): the desugared tree's interior nodes are charged normally —
  `n` `eq` nodes + `n−1` `or` nodes (+1 `not` for `NOT IN`). Because the OR-chain re-uses `x`
  in every comparison, **the LHS is evaluated once per list element** (a deliberate consequence
  of the desugar model); for a bare-column `x` that is a free leaf, so the cost is just the
  comparison/connective nodes. Output column name for a bare `SELECT x IN (…)` is the fixed
  `?column?` (§8), like any non-column expression.

## 21. `BETWEEN` / `NOT BETWEEN`

`x BETWEEN lo AND hi` is the **range predicate**: TRUE iff `lo <= x <= hi`. It extends
`comparison` with `"NOT"? "BETWEEN" additive "AND" additive`, the same non-associative
comparison-level (precedence 35) postfix slot as `IN` (§20). `NOT BETWEEN` is the negation.

- **Semantics = the AND form.** `x BETWEEN lo AND hi` is exactly `x >= lo AND x <= hi`, and
  `x NOT BETWEEN lo AND hi` is its negation `NOT (x >= lo AND x <= hi)` (= `x < lo OR x > hi`
  under Kleene). The engine **desugars** to that tree at resolve (`Expr::Between` →
  `Binary{And, Binary{Ge}, Binary{Le}}`, wrapped in `Unary{Not}` when negated), inheriting
  every property from `>=`/`<=`/`AND`/`NOT`.
- **Three-valued NULL via the Kleene AND** — and this is the subtle case. The connective is the
  three-valued AND, where a FALSE operand **dominates** (it is not plain propagation). So
  `5 BETWEEN 10 AND NULL` is `(5 >= 10) AND (5 <= NULL)` = `FALSE AND UNKNOWN` = **FALSE**,
  whereas `5 BETWEEN 1 AND NULL` is `TRUE AND UNKNOWN` = **NULL**. A NULL `x` makes both
  comparisons UNKNOWN, so the whole thing is NULL. This matches PostgreSQL exactly (verified
  against the live oracle) and is why BETWEEN cannot be a naive null-propagating macro.
- **The `BETWEEN`/`AND` ambiguity** is resolved by parsing **both bounds at the `additive`
  level**. `additive` never consumes the `AND` keyword (a looser precedence level owned by the
  `AND` connective), so the `AND` separating the two bounds is matched structurally by BETWEEN,
  and a trailing logical `AND` binds outside: `x BETWEEN a AND b AND c` parses as
  `(x BETWEEN a AND b) AND c`. The bounds are therefore not full expressions — they stop at the
  comparison level — exactly PostgreSQL's `b_expr` restriction.
- **Typing** reuses the `>=`/`<=` operand contract per bound: an integer-literal bound adapts to
  `x`'s type (a too-wide one traps **22003**), a cross-family bound is **42804**, decimal/int
  mixes promote, text compares by the `C` collation.
- **Cost** ([cost.md](cost.md) §3): the desugared `And(Ge, Le)` is three interior nodes (1
  `and` + 2 `compare`); **the LHS is evaluated twice** (once per bound — the desugar
  consequence). Output name for a bare `SELECT x BETWEEN …` is `?column?` (§8).

## 22. `LIKE` / `NOT LIKE`

`s LIKE pattern` is the **text pattern match**: TRUE iff the whole subject `s` matches
`pattern`, where `%` matches any (possibly empty) run of characters and `_` matches exactly
one character. It extends `comparison` with `"NOT"? "LIKE" additive`, the same
non-associative comparison-level (precedence 35) postfix slot as `IN`/`BETWEEN`. `NOT LIKE`
is the negation. Unlike `IN`/`BETWEEN`, `LIKE` is **not** desugared — it is a genuine
operator (one `[[operator]]` catalog row, `name = "like"`, text×text → boolean,
`null = "propagates"`) with a dedicated resolved node and a hand-written matcher.

- **Text only.** Both operands must be `text` (a single-quoted string literal stays text); a
  non-text operand (`5 LIKE '5'`) is **42804** — `compare.toml` lists only text×text for the
  pattern operator, exactly like the text comparisons. NULL on either side yields NULL
  (`null = "propagates"`): `NULL LIKE 'a'` and `'a' LIKE NULL` are both NULL, and a NULL
  operand short-circuits to NULL **before** the matcher runs (so a malformed pattern against a
  NULL subject is still NULL, not an error — verified against PostgreSQL 18).
- **Wildcards and the default `\` escape.** `%` = any run, `_` = one character. The default
  escape character is **backslash** (PostgreSQL's default): `\%`, `\_`, and `\\` match a
  literal `%`, `_`, and `\`; a `\` before any other character matches that character literally
  (`'a' LIKE '\a'` is TRUE). String literals have no backslash escapes
  (`standard_conforming_strings`, §3 / [types.md](types.md) §11), so a `\` written in a
  pattern literal is a literal backslash byte the matcher then interprets. The explicit
  `ESCAPE 'c'` clause, `ILIKE`, and `SIMILAR TO` are deferred (relaxable later).
- **Code-point matching — a §8 determinism surface.** `_` matches one **Unicode code point**,
  not one byte and not one UTF-16 unit, so `'😀x' LIKE '_x'` is TRUE. Every core iterates the
  subject and pattern by code point (Rust `chars()`, Go `[]rune`, **TS `Array.from` / spread —
  never `str[i]`/`charCodeAt`**, the same UTF-8-vs-UTF-16 trap text comparison already avoids,
  [types.md](types.md) §11). Pinned by an astral-character conformance case.
- **Trailing-escape error (22025), raised lazily during matching.** A pattern whose escape
  character is its **last** character is invalid — but PostgreSQL only raises it when the
  matcher actually **reaches** that escape with subject still to match. So `'ax' LIKE 'a\'`
  traps **22025** (`invalid_escape_sequence`), but `'a' LIKE 'a\'` is FALSE (the subject runs
  out first) and `'x' LIKE 'a\'` is FALSE (the leading `a` mismatches before the escape is
  reached). The matcher therefore raises 22025 from the eval walk, data-dependently and
  deterministically (the trapping case is fixed by the subject/pattern), **not** as a
  pre-validation of the pattern. (Verified against PostgreSQL 18.)
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the `like` node (like a `compare`);
  the match loop itself is unmetered, like `eq3` and the `ORDER BY` sort. Output name for a
  bare `SELECT s LIKE …` is `?column?` (§8).

## 23. `CASE`

`CASE` is the SQL conditional expression, a primary like `CAST`
(`case_expr ::= "CASE" expr? ( "WHEN" expr "THEN" expr )+ ( "ELSE" expr )? "END"`). It comes in
two forms and is the **first deliberately lazy** expression in the engine.

- **Two forms.** The **searched** form `CASE WHEN cond THEN r … [ELSE e] END` has no operand
  before the first `WHEN`; each `cond` must resolve to **boolean** (`42804` otherwise, like
  `WHERE`). The **simple** form `CASE x WHEN v THEN r … [ELSE e] END` has an operand `x`; each
  branch matches when **`x = v`**. The simple form desugars each branch to the equality
  `x = v` at resolve, reusing the `=` operand pairing and comparability check (the value `v`
  adapts to `x`'s type; an incomparable `v` is `42804`). At least one `WHEN` is required (a
  `CASE … END` with none is a `42601` syntax error).
- **Lazy first-match evaluation — the one short-circuit.** Conditions are evaluated in source
  order and evaluation **stops at the first TRUE** branch, returning that `THEN`. A FALSE or
  NULL/UNKNOWN condition falls through (a NULL `WHEN` is *not* true — like `WHERE`). With no
  matching branch, the `ELSE` result is returned, or **NULL** if there is no `ELSE` (an implicit
  `ELSE NULL`). Later arms are **never evaluated**, so `CASE WHEN a = 0 THEN 0 ELSE 1 / a END`
  does not divide by zero on the `a = 0` rows — this is the sanctioned exception to the
  no-short-circuit cost rule ([cost.md](cost.md) §3), and it stays deterministic because the
  order is fixed.
- **Result-arm type unification.** The `THEN` results and the `ELSE` (or NULL for an implicit
  ELSE) unify to one **common type** — the CASE's output type. The rule: NULL-typed arms are
  dropped (they adapt); an **all-NULL CASE is `text`** (PostgreSQL — verified against the live
  oracle); the remaining arms must share a family — all numeric unify to `decimal` if any is
  decimal else the widest integer (the promotion tower), and a numeric integer result widens to
  decimal at eval when the common type is decimal (so `CASE WHEN c THEN 1 ELSE 1.5 END` renders
  `1` / `1.5`); a non-numeric family (text/boolean/bytea) must be homogeneous. A **cross-family**
  mix — e.g. an integer `THEN` and a text `ELSE` — is **`42804`** ("CASE types … cannot be
  matched"). Bare integer-literal arms keep their natural width (defaulting to int64), so width
  differences from PostgreSQL are unobservable (every integer renders under the `I` tag).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the CASE node, plus the
  `operator_eval`s of the conditions tested up to the match and of the selected result only
  (the lazy-eval exception). Output name for a bare `SELECT CASE … END` is `?column?` (§8) —
  any non-column expression.
