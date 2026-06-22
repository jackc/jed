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
  column may be named `select`. There is no quoted-identifier escape for *identifiers* (a
  bare keyword-as-name suffices). A double-quoted token (`Token::QuotedIdent`) **does** exist,
  but only as a **collation name** after `COLLATE` (§47, [collation.md](collation.md) §1); it is
  not a general identifier escape, so a `"…"` in any other position is a `42601` syntax error.
- Keyword terminals in the grammar (`"SELECT"`, `"FROM"`, …) denote a case-insensitive
  match, while punctuation terminals (`"("`, `"="`) match literally.

This is a CLAUDE.md §8 divergence hotspot: if one core folded case differently, or
reserved a word another did not, the corpus would diverge. Recording the rule in the
grammar keeps all cores honest. (Canonical *output* names — `i16` not `smallint` — are
a separate determinism rule owned by the type system, see [types.md](types.md) §2.)

## 4. Lexical edges: the minus operator and two-character operators

Two lexer facts are easy to get subtly wrong across cores, so the grammar pins them:

- **`-` is a unary/binary operator, not part of the literal.** An `integer` token is an
  *unsigned* magnitude of digits; `-5` is the unary-minus operator applied to `5`, and
  `- 5` with a space is now legal (it was a lex error when the sign was lexed into the
  literal). The parser folds unary-minus-of-a-literal into a single negative `Literal`
  value, so the negative-literal range checks (types.md §6) are unchanged.
  - **Magnitude range.** A magnitude must be `<= 2^63` (`9223372036854775808`); a larger
    one is a syntax error (`42601`), not a silent wrap. So that `i64`'s minimum is
    reachable, the lexer carries the magnitude *unsigned* (Rust `u64`, Go `uint64`, TS
    `bigint`) — `i64`/`i64` cannot hold `2^63`. The value `2^63` is in range **only** as
    the operand of unary minus, where it folds to `-9223372036854775808` (`i64::MIN`); a
    bare `2^63` fits no signed integer type and traps `22003` at resolve time (deterministic,
    before any row is scanned).
- **`<=`, `>=`, and `<>` are single tokens**, lexed greedily. The comparison operators are
  `=`, `<>` (not-equal), `<`, `>`, `<=`, `>=`. **`!=` is an alias for `<>`** (PostgreSQL's
  alternate spelling): the lexer scans `!=` and folds it to the same not-equal token, so the two
  spellings are indistinguishable past the lexer (a lone `!` is `42601`). The
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
  list** (`INSERT INTO t (a, c) VALUES ...`) landed alongside `DEFAULT` (§12, §16).
  `INSERT ... SELECT` — inserting the rows a query produces — now also lands (§24). What stays
  deferred is **general expressions in a `VALUES` value slot**.
- **`ORDER BY` keys are bare columns** — a sort key is a table column, never a general
  expression (`ORDER BY a + 1`), an output alias, or an ordinal position (`ORDER BY 1`);
  those stay deferred. The richer surface that *did* land — multiple keys, per-key
  `ASC` / `DESC`, and per-key `NULLS FIRST | LAST` — is described in §10.
- **`LIMIT` / `OFFSET` take a non-negative integer literal**, not a general expression
  (the same literal-only narrowing `INSERT` makes). The two clauses may appear in either
  order, each at most once (§9). There is **no `LIMIT ALL`**, **no `OFFSET … ROWS` /
  `FETCH NEXT … ROWS ONLY`**, and **no SQLite `LIMIT off, cnt` comma form**.
- **ASCII-only identifiers**; the one double-quoted token is a collation name after `COLLATE`,
  not a general quoted-identifier escape (§3, §47).
- **Literal forms.** Integer, **decimal** (`1.50`, `.5`, and scientific `e`-notation
  `1.5e3` / `5e2` / `1e-3` — §14), **single-quoted string** (the `text` type, `'alice'`, with
  `''` for an embedded quote), `TRUE`/`FALSE`, and `NULL`. `boolean` is now a storable column
  type as well as an expression type (boolean literals, comparison/logical results, and stored
  boolean columns — see [types.md](types.md) §9).
- **Function calls.** The expression grammar has a `function_call` production with
  PostgreSQL named-notation (`name(arg, name => expr)`) and `DEFAULT` arguments (§17). It
  resolves the five aggregates (`COUNT`/`SUM`/`MIN`/`MAX`/`AVG`; [aggregates.md](aggregates.md))
  **and** scalar functions (`abs`, `round`, `make_interval`, the uuid/clock functions;
  [functions.md](functions.md) §9–§12). Still deferred: `COUNT(DISTINCT x)` and further scalar
  functions (`length`, `lower`, …); an unknown function name is `42883`, and `DISTINCT` inside a
  call is `42601`.
- **No `;` statement terminator** — one statement per `execute` (SQL comment syntax, by
  contrast, *has* landed — `--` line and nesting `/* */` block comments, §33).
- **Parameter placeholders (`$N`) are parsed, but bound by the host API, not the corpus.**
  The lexer accepts `$` followed by ≥1 ASCII digits as a 1-based bind parameter (`$1`,
  `$2`, …); `$0`, a leading zero (`$01`), and `$` not followed by a digit are `42601`. A
  `$N` is a primary expression usable anywhere an expression is (WHERE / HAVING / ON /
  select list / UPDATE SET RHS / arithmetic / `CAST` inner / `IN` / `BETWEEN` / `LIKE` /
  `CASE`) and as an `INSERT` value slot — but **not** in LIMIT/OFFSET, GROUP BY, or a type
  modifier this slice. A parameter's type is **inferred from context** (its sibling operand,
  target column, or `CAST` target); a parameter with no derivable type is `42P18`. Binding a
  value to `$N` is each implementation's own host-API surface ([api.md](api.md)) — the
  conformance corpus still uses **literal SQL only** (see [conformance.md](conformance.md));
  `?`-style placeholders remain unsupported.

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
none — `i32(5)` — is rejected at resolve. Empty parens (`numeric()`) and a non-integer
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
   SELECT. So with `c i32` declared, `SELECT C FROM t` names the column `c`. (Identifiers
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
positive magnitude of `2^63` (over `i64`'s max) traps `22003` (§4). `LIMIT 0` is valid and
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
list** and the **`DEFAULT` keyword** are covered in §16; inserting the rows a query produces
(`INSERT ... SELECT`) is §24. General expressions in a `VALUES` value slot stay deferred
(§5, [../../TODO.md](../../TODO.md)).

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

**Cost (for the `VALUES` source).** When a row's values are literals and **pre-evaluated
constant defaults** (folded to a value at CREATE TABLE — §16), an `INSERT ... VALUES` reads no
storage and evaluates no expression tree, so it accrues **zero** cost ([cost.md](cost.md)). An
**expression default** (`DEFAULT now()` / `1 + 1`, §16) is the exception: it evaluates an
expression tree per row at INSERT, accruing that evaluation's cost. An `INSERT ... SELECT` is
different again: it accrues exactly the embedded `SELECT`'s cost (§24).

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
[../types/casts.toml](../types/casts.toml)). A coefficient beyond `max_int_digits` integer-part
digits, or a scale beyond `max_scale` ([../types/scalars.toml](../types/scalars.toml)), traps
`22003` at resolve.

**Scientific `e`-notation** (PostgreSQL, `numeric.c` `set_var_from_str`). A significand — with
a `.` (`1.5e3`, `.5e2`) **or without** (`5e2`, `1e3`) — may carry an exponent
`[eE][+-]?digit+`. An exponent makes the literal a **decimal** even with no `.` (so `5e2` is the
decimal `500`, *not* an integer — matching PG, where any exponent forces type `numeric`); a bare
digit run with no `.` and no exponent stays an integer. The display **scale** is
`max(0, frac_digits − exponent)` and the value shifts by `10^exponent`: `1.5e3 → 1500` (scale 0),
`1.50e1 → 15.0` (scale 1), `1.5e-3 → 0.0015` (scale 4), `5e2 → 500` (scale 0), `1.50e-3 → 0.00150`
(scale 5). When the exponent drives the scale below zero the coefficient absorbs the shift as
trailing zeros at scale 0. The result is cap-checked at resolve exactly like a written-out literal
(`1e131072` traps `22003`; `5e2` into an integer column is the same `42804` as any decimal). A
malformed exponent (`1e`, `1e+`, `1ex` — the `e` not followed by `[+-]?digit`) is **not** consumed
as part of the number; it lexes as the trailing token and is rejected at parse, like any other
junk after a literal. The lexer clamps an absurd exponent magnitude (a determinism/resource guard,
CLAUDE.md §13) so `1e9999999999` cannot materialize a gigabyte of coefficient zeros: a clamped
exponent always still traps `22003` for a non-zero coefficient, the one documented divergence being
that a *zero* coefficient with such an exponent (`0e9999999999`) reads as `0` here rather than
PG's overflow error — an extreme corner the single-field `coefficient × 10^(−scale)`
representation makes unavoidable.

**The `numeric(p,s)` type modifier** (§6). `numeric` (unconstrained), `numeric(p)`
(= `numeric(p,0)`), and `numeric(p,s)` are the three forms, in both a column type and a
`CAST` target. `p` is the total significant digits (`1 ≤ p ≤ 1000`) and `s` the digits after
the point (`0 ≤ s ≤ p`); an out-of-range or malformed typmod — `numeric(0)`, `numeric(1001)`,
`numeric(5,7)` — traps **`22023`** (`invalid_parameter_value`,
[../errors/registry.toml](../errors/registry.toml)), PostgreSQL's SQLSTATE. The grammar
accepts the typmod shape on *any* type name (one production, §6); a typmod on a type that
takes none (`i32(5)`, `text(10)`) is a resolve-time error this slice (`0A000` — `varchar(n)`
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
  a word that is **not** in the clause/join stop-keyword set: `as`, the trailing-clause keywords
  (`where`/`group`/`having`/`order`/`limit`/`offset`), the join machinery (`on`/`join`/`inner`/
  `cross`/`left`/`right`/`full`/`outer`), the set operators (`union`/`intersect`/`except` — §25),
  and `returning` (the DML trailing clause — §32, so an `INSERT ... SELECT ... RETURNING` never
  swallows the clause as the source's alias). An explicit `AS` takes the next identifier
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
  `t.*`** qualified-star, **no parenthesized-join FROM** (`FROM (a JOIN b ON …)`). A **derived table**
  (`FROM (SELECT …) AS t`) *is* now supported — see §42.
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

**Constant defaults** are pre-evaluated at CREATE TABLE, so applying one at INSERT substitutes
a constant — no expression is evaluated and cost stays zero (§12). **Expression defaults**
(`DEFAULT now()`, `DEFAULT 1 + 1`) have since landed: a non-constant default is evaluated per
row at INSERT through the per-statement seam ([constraints.md](constraints.md) §2), accruing
that evaluation's cost. The column list and `DEFAULT` keyword apply unchanged when the source is
a `SELECT` (§24).

## 17. Function-call syntax, aggregate and scalar functions

The `primary` rule gains a `function_call` production — `function_call ::= identifier "("
( "*" | function_arg ( "," function_arg )* )? ")"` with `function_arg ::= ( identifier "=>"
)? expr` — the engine's call syntax, shared by aggregate and scalar functions, now including
PostgreSQL **named notation** and an **empty** argument list (see "Named notation" below). The *semantics* (what each aggregate computes, the SUM/AVG widening, the
NULL / empty-set rules, the grouping rules) live in [aggregates.md](aggregates.md), and the
scalar-function semantics in [functions.md](functions.md) §9; this section is the **syntax**
and the disambiguation, the established grammar.md/semantics split (§15 does the same for
joins).

**Aggregate and scalar names resolve.** A function name resolves to one of the five
aggregates — `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`
([../functions/catalog.toml](../functions/catalog.toml), `kind = "aggregate"`) — or to a
scalar function — `abs`, `round` (`kind = "function"`, [functions.md](functions.md) §9).
Any other name is **`42883`** (`undefined_function`,
[../errors/registry.toml](../errors/registry.toml)), resolved like an unknown type name
(§6). The two kinds are syntactically identical and disambiguated at **resolve** time: an
aggregate folds a set of rows and is constrained to projection/`HAVING` contexts (an
aggregate in `WHERE` / a `JOIN ON` / a `GROUP BY` key, or nested in another aggregate, is
`42803`); a scalar function maps values **per row** and is legal **anywhere an expression
is**.

**The argument(s).** `COUNT(*)` is the row counter — the `*` argument is accepted **only**
by `COUNT`; `*` to any other function is a resolve error. Otherwise a call takes a
**comma-separated list of general expressions**: each aggregate takes exactly **one**
(`SUM(a + 1)`, `MIN(t.c)`, `COUNT(expr)`) — a second argument matches no aggregate overload
and is `42883`; `abs` takes **one** (`abs(a - b)`); `round` takes **one or two**
(`round(x)`, `round(x, 2)`). **`DISTINCT` inside the parens** (`COUNT(DISTINCT x)`) is
**deferred** — the parsers reject the `DISTINCT` token in an argument position as `42601`
(it is added as a follow-on).

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

**Named notation + DEFAULT arguments (PostgreSQL named args).** A `function_arg` may be
written `name => expr` (named notation), and the whole argument list may be **empty** — both
landed with `make_interval`, the engine's first named + defaulted function
([functions.md](functions.md) §11). Three rules, parser-enforced and **byte-identical across
the three cores** (another §8 determinism surface, like the one-token call lookahead above):

1. **Two-token lookahead.** A named argument is distinguished from a bare `expr` that happens
   to start with an identifier by peeking **two** tokens: a `word` immediately followed by the
   `=>` arrow is `name => …`; anything else is positional. The `=>` arrow is a single lexer
   token (greedy after `=`, like `::` / `<=`). The legacy `:=` spelling PostgreSQL also accepts
   is **not** part of jed's surface — jed has no `:` token, and `=>` alone is the modern,
   unambiguous form — so `f(a := 1)` is a `42601` syntax error (a deliberate, documented
   narrowing; the conformance override ledger records it).
2. **No positional after named.** Once a named argument appears, a later positional one is
   `42601` (`positional argument cannot follow named argument`) — PostgreSQL's rule.
3. **Resolve-time mapping.** Whether named notation is *allowed*, how names map to parameter
   slots, how omitted trailing arguments are filled from DEFAULTs, and the errors for an
   unknown name (`42883`), a name on a function with no parameter names (`42883`), or a
   duplicate name (`42601`) are **resolve-time** concerns driven by the catalog's
   `arg_names` / `arg_defaults` data — see [functions.md](functions.md) §11. The grammar only
   carries the per-argument optional name to resolve; an all-positional call carries none (so
   it is byte-identical to a pre-named-notation parse).

**The `VARIADIC` keyword (passing an array to a variadic parameter).** The **last** argument of
a call may be prefixed with the `VARIADIC` keyword — `num_nulls(VARIADIC arr)`, which passes the
array `arr` directly to a variadic parameter instead of spreading individual arguments
([array-functions.md](array-functions.md) §12, the AF6 slice). Parser rules, again byte-identical
across the three cores:

1. **Last-argument only.** `VARIADIC` may mark only the final argument; a `VARIADIC`-marked
   argument followed by another (`f(VARIADIC a, b)`) is `42601` (PostgreSQL's rule). It does
   **not** combine with named notation (the keyword is consumed before any `name =>` peek, so
   `f(VARIADIC n => v)` parses `n` as the array expression and then errs on `=>`). `VARIADIC` is
   recognized as a keyword **only at the start of a call argument** — it is not a globally reserved
   word (a column or table may be named `variadic` elsewhere), but it cannot be the bare leading
   identifier of a call argument (PostgreSQL, where `VARIADIC` is fully reserved, agrees).
2. **A flag on the call node.** The parser records a single `variadic` boolean on the `FuncCall`
   node (whether the last argument was `VARIADIC`-marked); an ordinary call carries `false` and
   parses byte-identically to before. The flag travels to resolve.
3. **Resolve-time meaning.** Whether the called function is *actually* variadic, the spread vs.
   array-operand semantics, and the errors for a `VARIADIC` non-array operand (`42804`) or a
   too-short spread (`42883`) are resolve-time concerns driven by the catalog's `variadic` flag —
   see [array-functions.md](array-functions.md) §12.

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
  `ESCAPE 'c'` clause and `SIMILAR TO` are deferred (relaxable later).
- **`ILIKE` — case-insensitive `LIKE`** (collation Slice 3e). `x ILIKE p` / `x NOT ILIKE p` is `LIKE`
  with both operands **simple-lowercased** first — ASCII-only when no Unicode property bundle is
  loaded, full simple (1:1) Unicode mappings when one is ([collation.md §16](collation.md)). Simple
  1:1 folding (never the expanding `ß`→`SS` form) keeps `_`/length semantics intact. Same precedence,
  NULL propagation, `22025` trailing-escape, and non-text `42804` as `LIKE`.
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
  matched"). Bare integer-literal arms keep their natural width (defaulting to i64), so width
  differences from PostgreSQL are unobservable (every integer renders under the `I` tag).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the CASE node, plus the
  `operator_eval`s of the conditions tested up to the match and of the selected result only
  (the lazy-eval exception). Output name for a bare `SELECT CASE … END` is `?column?` (§8) —
  any non-column expression.

## 24. `INSERT ... SELECT`

`INSERT` may take its rows from a **query** instead of a `VALUES` list: the `insert`
production's source is now `( "VALUES" row ("," row)* | select )`. `INSERT INTO dst SELECT a, b
FROM src WHERE …` inserts whatever the embedded `SELECT` produces. The whole `SELECT` surface
is reachable as a source — `WHERE`, `JOIN`, `GROUP BY`/`HAVING`, `DISTINCT`, `ORDER BY`,
`LIMIT`/`OFFSET`, aggregates, `CASE` — because the source *is* a `select`, parsed and executed
by the same path as a top-level query. The optional **column list** and **`DEFAULT`-for-omitted
columns** (§16) apply unchanged; a `DEFAULT` *keyword* value slot is a `VALUES`-only thing and
does not exist in the SELECT source.

**Arity — the `SELECT`'s output column count must match the target**, exactly as a `VALUES`
row's arity must (§12): the number of projected columns must equal the column-list length, or
the table's column count with no list, else `42601`. This is checked **once, before any row is
produced** — so it fires even when the `SELECT` returns **zero rows**.

**Type-assignability — checked up front, PostgreSQL-faithful.** Beyond the per-value checks the
`VALUES` path already does, `INSERT ... SELECT` validates each projected column's **type** is
assignable to its target column **before** producing rows, mirroring PostgreSQL's plan-time type
analysis. So a type-incompatible projection is rejected with `42804` **even over an empty
source** (`INSERT INTO t(int_col) SELECT text_col FROM src WHERE 1=0` errors; it does not
silently insert nothing). The assignability test is the **family-level subset of the per-value
store coercion** ([constraints.md](constraints.md) §2) and must agree with it: an integer
projection is assignable to an integer **or** decimal column (int→decimal widens), a decimal
only to a decimal column (decimal→int is explicit-`CAST` only), a text projection to text/uuid/
bytea (the documented text-adaptation, [types.md](types.md) §6), boolean→boolean, uuid→uuid,
bytea→bytea, and a **NULL-typed** projection to **any** column (a `NOT NULL` target then traps
`23502` per row, if any). A column the list omits is not type-checked — it takes its default
else NULL.

**Same two-phase / all-or-nothing pass as §12.** Once arity and assignability pass, every
produced row runs through the identical validation the `VALUES` path uses: each value is
type-coerced and range-checked in declaration order (`22003` overflow, `23502` NOT NULL,
`22P02` malformed text→uuid/bytea), each storage key is computed and checked for a duplicate
(`23505`, both against stored rows and earlier rows of this statement), and **only if every row
passes** are any inserted. Synthetic rowids (a no-PK target) are allocated in phase two in the
`SELECT`'s output-row order.

**The source is fully materialized before any write — self-insert is well-defined.** The
embedded `SELECT` runs to completion (its rows owned) before phase two mutates the store, so
`INSERT INTO t SELECT … FROM t` reads the **pre-insert snapshot** of `t` and never feeds its own
new rows back (no Halloween problem). A self-insert whose keys collide with the existing rows
traps `23505` and stores nothing; a key-shifting self-insert (`INSERT INTO t SELECT id + 100, a
FROM t`) doubles the table.

**Cost = the embedded `SELECT`'s accrued cost** ([cost.md](cost.md)). The `SELECT` already
charges `storage_row_read` per scanned row and `row_produced` per emitted row (plus expression
`operator_eval`s); storing the rows is unmetered, like every `INSERT`. So the statement's
deterministic, cross-core cost is exactly what the source query accrues — unlike the
`VALUES` source's zero (§12). The output order the `SELECT` produces is itself deterministic and
identical across cores (key-ordered scans, insertion-ordered grouping/distinct, left-deep
joins — [encoding.md](encoding.md), CLAUDE.md §8), so the rowids assigned to
a no-PK target are byte-identical across the three cores.

## 25. Set operations (`UNION` / `INTERSECT` / `EXCEPT`)

A **query expression** is the top-level query form: one or more `SELECT` cores combined by the
set operators `UNION`, `INTERSECT`, `EXCEPT` — the first construct where a query is built from
two sub-queries rather than a single `SELECT`. Each operator has a bare (distinct) form and an
`ALL` (multiset) form: `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`. PostgreSQL is the
behavioral default (CLAUDE.md §1); the semantics below are pinned against `postgres:18`.

The grammar ([../grammar/grammar.ebnf](../grammar/grammar.ebnf)) is a two-level precedence tree
over `select_core` (a `SELECT` with no trailing `ORDER BY`/`LIMIT`/`OFFSET`), with the trailing
clauses lifted to the whole expression:

```
query_expr     ::= set_expr order_by? limit_offset?
set_expr       ::= intersect_expr (("UNION" | "EXCEPT") ("ALL" | "DISTINCT")? intersect_expr)*
intersect_expr ::= select_core ("INTERSECT" ("ALL" | "DISTINCT")? select_core)*
```

A lone query (no set operator) is a single `select_core` whose trailing clauses fold back onto
it — byte-for-byte the pre-set-operations `SELECT`. `UNION`/`INTERSECT`/`EXCEPT`/`ALL`/`DISTINCT`
are **not reserved** (§3), disambiguated positionally.

**Precedence — PostgreSQL.** `INTERSECT` **binds tighter** than `UNION` and `EXCEPT`, so it is
its own inner level; `UNION` and `EXCEPT` share one outer level and are **left-associative**.
Thus `a UNION b INTERSECT c` parses as `a UNION (b INTERSECT c)` and `a UNION b EXCEPT c` as
`(a UNION b) EXCEPT c`. (Oracle: `(VALUES 1) UNION (VALUES 2,3) INTERSECT (VALUES 3,4)` →
`{1, 3}`, confirming `INTERSECT` first.) `DISTINCT` after an operator is the explicit spelling of
the bare (deduplicating) default.

**Result columns — count and names from the LEFT operand.** Both operands must produce the
**same number of columns**, else `42601` (PostgreSQL: "each UNION query must have the same number
of columns"); the check fires **before any row is produced**, so it errors even over empty
operands. The output column **names** are the left operand's (the right operand's names and
aliases are discarded). For a chain, "left" is the leftmost `SELECT` — names propagate up the
left spine.

**Column types — unified per position, full PG fidelity.** Each output column's type is the
fold of the operands' types at that position ([cost.md](cost.md) §3 records the same lattice as a
cross-core contract):

- integer widths **promote** to the widest (`i16` < `i32` < `i64`);
- integer and `decimal` unify to **`decimal`** (oracle: `int2 ∪ int4` → `integer`, `int4 ∪ int8`
  → `bigint`, `int ∪ numeric` → `numeric`);
- a column that is **`NULL`-typed in every operand** unifies to **`text`** (PostgreSQL's
  unknown-literal resolution — oracle: `(SELECT NULL) UNION (SELECT NULL)` → `text`); a `NULL`
  type alongside any concrete type takes the concrete type;
- otherwise the operands must share a base type (`text`/`boolean`/`bytea`/`uuid`/`timestamp`/
  `timestamptz`), giving that type;
- any other pairing is **`42804`** (PostgreSQL: "UNION types `X` and `Y` cannot be matched").

When the unified type is `decimal`, an integer operand's **values are converted** to `decimal`
(scale 0) *before* rows are matched — this is load-bearing for correctness, not just for the
output type tag: the engine's row identity keys an `int` and a `decimal` value distinctly, so
without the conversion `SELECT 1 … INTERSECT SELECT 1.0 …` would wrongly find no match. Each
value keeps its **own** display scale (unconstrained `numeric`, the per-value model of
[decimal.md](decimal.md) §6) — a converted integer renders at scale 0 (`1`), a decimal keeps its
scale (`2.50`); the engine does **not** normalize the column to a uniform scale (oracle:
`SELECT 1 UNION ALL SELECT 2.50` → `1`, `2.50`). Integer width promotion needs no value
conversion (every integer is one internal 64-bit value).

**Row identity — NULL-safe, value-canonical, exactly as `DISTINCT`.** Two rows are "the same row"
under the engine's NULL-safe equality (`IS NOT DISTINCT FROM` — §11): `NULL` matches `NULL`
(oracle: `(VALUES NULL) INTERSECT (VALUES NULL)` → one `NULL` row) and decimals match by
value-canonical form (`1.5` ≡ `1.50`). The **representative** emitted for a matched/deduplicated
key is the **first occurrence scanning the left operand then the right** — so its display scale is
the left's where they tie (oracle: `SELECT 1.0 INTERSECT SELECT 1` → `1.0`; `SELECT 1 UNION
SELECT 1.0` → `1`). This first-occurrence rule is deterministic and identical across cores.

**Multiset semantics** (let *m*, *n* be a row key's multiplicity in the left and right operand):

| form | result multiplicity per key | bare form (no `ALL`) |
|---|---|---|
| `UNION ALL` | all left rows then all right rows | `UNION` — one per key present in either |
| `INTERSECT ALL` | `min(m, n)` | `INTERSECT` — one per key with `m>0 ∧ n>0` |
| `EXCEPT ALL` | `max(0, m − n)` | `EXCEPT` — one per key with `m>0 ∧ n=0` |

(Oracle: `{1,1} INTERSECT ALL {1,1,1}` → `{1,1}`; `{1,1,2} EXCEPT ALL {1}` → `{1,2}`;
`{1,1,2} EXCEPT {1}` → `{2}`.)

**Trailing `ORDER BY` / `LIMIT` / `OFFSET` apply to the whole result**, after the combine. Keys
resolve against the **output columns by name** (the left operand's names) — there is no relation
scope after a set operation, so a **qualified** key (`ORDER BY t.x`) is an error (PostgreSQL:
"missing FROM-clause entry"; the engine reports `42P01`/undefined). **Ordinals** (`ORDER BY 1`)
stay deferred, consistent with the engine deferring ordinals everywhere (§5, §10). Direction and
`NULLS FIRST|LAST` work exactly as §10 (the same `key_cmp` over the output-row value). `LIMIT`/
`OFFSET` then window the ordered result (§9). Output order **without** a trailing `ORDER BY` is
unspecified (CLAUDE.md §8/§10; the corpus compares such queries `rowsort`); the result *multiset*
is exact and identical across cores regardless.

**Deferred narrowings (each relaxable later).**

- **No parenthesized operands** — `(SELECT …) UNION …` is not accepted.
- **No `ORDER BY`/`LIMIT`/`OFFSET` inside an operand** — only on the whole result. Because a
  `select_core` does not consume those clauses, an operand `ORDER BY` is left dangling and the
  statement fails to consume all input → `42601` (the leftover-token rule). To order *then*
  combine, the parenthesized-operand relaxation above is the eventual path.
- **No ordinals** in the trailing `ORDER BY` (above).
- **No set operation in an `INSERT … SELECT` source** (§24) — the source stays a single `select`.

**Cost** is `lhs + rhs`, the combine itself unmetered — see [cost.md](cost.md) §3.

## 26. Subqueries (scalar / `IN` / `EXISTS`)

A **subquery** is a parenthesized `query_expr` (a `SELECT`, or a set operation — §25) used
inside an expression. The three forms below work both **uncorrelated** (the subquery's result
is independent of the enclosing query) and **correlated** (the subquery references a column of
an enclosing query — see [Correlated subqueries](#correlated-subqueries) below). They are
available wherever an expression resolves against a relation: a `SELECT`, and a **`DELETE`
`WHERE` / `UPDATE` `WHERE` / `UPDATE` assignment RHS** (a correlated reference there names the
**target row**, which the per-row evaluator supplies, so `DELETE FROM t WHERE id IN (SELECT …)`
and `UPDATE t SET c = (SELECT … WHERE … = t.k)` both work). The mutation stays two-phase /
all-or-nothing (§6): every subquery reads the **pre-statement snapshot** (`DELETE` collects its
keys before removing; `UPDATE` validates all rows before writing). A subquery in an
`INSERT ... VALUES` slot is not reachable — a `VALUES` slot is not yet a general expression
(§12) — while `INSERT ... SELECT` (§24) already admits subqueries inside its source query.

- **Scalar subquery** — `( query_expr )` in expression position, anywhere a `primary` is
  allowed: `WHERE x = (SELECT max(id) FROM t)`, in the select list, or nested in a larger
  expression `(SELECT …) + 1`. It yields the value of the subquery's single row and single
  column.
- **`x [NOT] IN ( query_expr )`** — membership of `x` in the subquery's single output column.
- **`[NOT] EXISTS ( query_expr )`** — whether the subquery produces at least one row.

**Disambiguation.** A `(` that begins a `primary` starts a scalar subquery when the next token
is `SELECT`, otherwise a parenthesized expression. `IN (` likewise: a leading `SELECT` is the
IN-subquery, otherwise the §20 value list. `EXISTS` is a keyword prefix taking `( query_expr )`.
Because the operand is a full `query_expr`, `IN (SELECT … UNION …)` and `EXISTS (… INTERSECT …)`
parse.

### Evaluation model — plan once, then fold or re-execute

Every subquery is **planned exactly once** — resolved to a plan against a *scope chain* (its
own FROM, then each enclosing query's FROM outward), so structural and type errors are raised
at plan time **whether or not the outer query produces any rows** (matching PostgreSQL, which
resolves a subquery during planning). What happens next depends on correlation:

- An **uncorrelated** subquery's result does **not** depend on any outer row, so the engine
  **executes it once** and folds it into a constant the ordinary evaluator already handles
  (PostgreSQL's "initplan"). This keeps the per-row evaluator unchanged for the common case
  and the cost (below) trivially once-only.
- A **correlated** subquery is **re-executed per outer row** that reaches its node, reading the
  enclosing-row values its plan references. The two agree with PostgreSQL because a single
  uncorrelated execution and per-row correlated execution are exactly what PG does.

The fold/validation rules (column count, cardinality, the typed empty NULL, IN three-valued
membership, EXISTS ignoring the select list) are identical for both — they constrain the
subquery's *result*, which is independent of how many times it ran.

- **Scalar** — the subquery must produce **exactly one column** (else `42601`, "subquery must
  return only one column") and **at most one row** (more than one → **`21000`**,
  cardinality_violation). **Zero** rows → a **typed NULL** (the value is NULL but the type is the
  subquery's output-column type, so `1 = (SELECT 'x' WHERE false)` is still a type error, not
  NULL). The folded constant carries the subquery's resolved output type, so it participates in
  cross-type comparison/promotion exactly as a column of that type would (`int = (SELECT bigint)`
  → `bigint`).
- **`IN`** — the subquery must produce **exactly one column** (else `42601`, "subquery has too
  many columns"). A **non-empty** result folds to the same OR-chain `x IN (v1, v2, …)` desugars
  to (§20), so the three-valued NULL semantics are inherited verbatim: a NULL in the result with
  no positive match yields NULL (unknown), not FALSE. An **empty** result folds directly to
  `FALSE` (`IN`) / `TRUE` (`NOT IN`), **regardless of whether `x` is NULL** — there is no list,
  so the OR-chain is not used.
- **`EXISTS`** — folds to the boolean `(rows > 0)` XOR the `NOT`. The select list is **ignored
  entirely** (`EXISTS (SELECT 1, 2, 3)` and `EXISTS (SELECT *)` are both legal — column count and
  types are irrelevant), and the result is **never NULL**.

**Cost** for an **uncorrelated** subquery is the enclosing query's own cost **plus** the
subquery's cost, counted **once** (it ran once); the folded constant is a leaf and charges no
`operator_eval`. For a **correlated** subquery it is the enclosing query's cost **plus** the
subquery's cost **per outer row** the node evaluates, plus one `operator_eval` for the
subquery node itself each time — see [cost.md](cost.md) §3.

### Correlated subqueries

A **correlated** subquery references a column of an enclosing query. Resolution uses a **scope
chain**: a column name is resolved against the subquery's own FROM first (the ordinary
42703/42702 rules), and only on a clean miss is it sought in the **enclosing** query's scope,
then its enclosing scope, and so on outward. A name found in an ancestor is an **outer
reference**; PostgreSQL's nearest-scope rule holds, so a name present in the inner FROM shadows
the same name outside. Correlation may reach **any** depth — a subquery may reference its
immediate parent, a grandparent, or higher — and nesting composes (each level resolves against
the full chain visible at that level).

- An outer reference is a **constant within the subquery's own evaluation** for a given outer
  row. It is therefore allowed inside an aggregate subquery's select list, `HAVING`, or an
  aggregate **argument** without being subject to the grouping-key rule (§18) — that rule
  constrains the subquery's *own* columns, not values borrowed from outside.
  - **Documented divergence:** an aggregate is always associated with the query in whose select
    list it textually appears, so `(SELECT sum(outer.col) FROM inner)` sums the outer constant
    over the *inner* rows. PostgreSQL instead binds an aggregate whose arguments are **purely**
    outer references to the *outer* query level (so the bare-`sum(outer.col)` form raises an
    outer grouping error there). The common, unambiguous case — an argument that mixes an inner
    column with an outer reference, e.g. `sum(inner.v + outer.v)` — agrees with PostgreSQL.
- The subquery is **planned once** (so a type/structure error in it is raised even when the
  outer produces zero rows) and **re-executed per outer row** that reaches its expression node,
  reading the enclosing-row values its plan references.

**Bind parameters inside a subquery** are **allowed**. Type inference is statement-wide — one
parameter-type table threads through the entire plan tree — so a `$N` typed by an **inner** context
(`WHERE inner.col = $1`, `inner.id IN (SELECT … WHERE x = $1)`) infers correctly, and the **same**
`$N` may appear both inside and outside the subquery (the uses unify). A correlated subquery may
also compare a `$N` against the outer row. The one gap is a `$N` whose **only** type context is the
*enclosing* query — e.g. `k = (SELECT $1 FROM t LIMIT 1)`, where `$1`'s type would have to flow
*into* the subquery from the outer `k = …`. jed does not do that **bidirectional** inference, so the
parameter stays uninferred and `finalize` raises **`42P18`** (indeterminate parameter type). This is
a **documented divergence**: PostgreSQL instead defaults such a `$N` to `text`, then fails the outer
comparison (`operator does not exist: integer = text`, `42883`). Both error; jed's `42P18` names the
real cause (the type can't be determined) and is consistent with its strict, no-guessing type system
(CLAUDE.md §4) — the overriding reason not to mimic PG's `text` default here.

**Deferred narrowings (each relaxable later)**

- **A correlated reference as a `GROUP BY` / `ORDER BY` key** — grouping or ordering a subquery
  *by an enclosing-query column* (a per-outer-row constant — degenerate) is **`0A000`**; the
  key machinery is flat local indices. WHERE / HAVING / `ON` / select-list correlation is fully
  supported.
- **Derived tables** — `FROM ( query_expr ) AS t` (a subquery as a relation) landed as its own
  slice; see §42.
- **`ANY` / `ALL` over a subquery** — `x op ANY/ALL(SELECT …)` landed (the subquery spelling of `IN`);
  see §41 / [array-functions.md §11.6](array-functions.md). **Row-valued** subqueries are not implemented.
- **Subqueries in `GROUP BY`** are not reachable — a `GROUP BY` key is grammatically a
  `column_ref` only (§18), so `(SELECT …)` there is a `42601` syntax error by the existing rule.

## 27. Transaction control (`BEGIN` / `COMMIT` / `ROLLBACK`)

The SQL surface for explicit transactions. This section fixes the **grammar**; the **model**
(autocommit, the immutable-snapshot + working-root staging buffer, the access modes, the abort
semantics, the `synchronous` durability setting) is [transactions.md](transactions.md) §4–§9,
and the equivalent host-API forms (`db.begin`/`view`/`update`) are [api.md](api.md) §2.2/§6. The
SQL statements and the API are **two surfaces over one mechanism** — both drive the handle's
single current transaction.

```
begin    ::= ( "BEGIN" ("TRANSACTION" | "WORK")? | "START" "TRANSACTION" ) access_mode?
access_mode ::= "READ" "ONLY" | "READ" "WRITE"
commit   ::= ("COMMIT" | "END") ("TRANSACTION" | "WORK")?
rollback ::= "ROLLBACK" ("TRANSACTION" | "WORK")?
```

**Autocommit is the default** (transactions.md §4.1): outside an explicit block every statement
is its own transaction — it commits on success / rolls back on error — so the existing one-shot
behavior, and the whole conformance corpus, are unchanged. `BEGIN` / `START TRANSACTION` opens an
explicit block; the statements that follow run within it until `COMMIT` / `END` or `ROLLBACK`.

- **Access mode.** `READ WRITE` (the default) may read and write. `READ ONLY` may only read; a
  write statement (`INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP`) inside it is **`25006`**
  (read_only_sql_transaction). The mode is **fixed when the block opens** (it governs the write
  lock — transactions.md §4.3/§10), declared here and inferred from the statement kind under
  autocommit. On a **read-only handle** (api.md §2.1) an unspecified mode defaults to READ ONLY
  instead, and an explicit `READ WRITE` is `25006` (PostgreSQL hot-standby behavior).
- **`COMMIT` / `END`** publishes the block's changes atomically (the snapshot swap) and makes
  them durable per the `synchronous` setting (transactions.md §9), returning to autocommit.
  Committing a **failed** block (below) performs a `ROLLBACK` instead (PostgreSQL).
- **`ROLLBACK`** discards the block's working set — every `INSERT`/`UPDATE`/`DELETE` **and DDL**
  `CREATE`/`DROP` since `BEGIN`, plus any synthetic-rowid allocations (transactions.md §4.5/§7) —
  and returns to autocommit. DDL is transactional: a table created in a rolled-back block does
  not exist afterward; a table dropped in one is still there.
- **Failed-block poisoning.** A statement error inside an explicit block **aborts the
  transaction**: it enters the *failed* state, and every subsequent statement except
  `ROLLBACK` (and `COMMIT`, treated as `ROLLBACK`) is rejected with **`25P02`**
  (in_failed_sql_transaction) until the block ends. The statement that errored wrote nothing
  partial (two-phase, §6); `ROLLBACK`/`COMMIT` then discards the whole working set. This matches
  PostgreSQL's "current transaction is aborted, commands ignored until end of transaction block."

**Two deliberate edges (transactions.md §4.2), each principled:**

- A **nested `BEGIN`** — issued while a block is already open — is **`25001`**
  (active_sql_transaction). There is no `SAVEPOINT`/nesting this slice; a nested `BEGIN` has no
  defined action, so it errors.
- A **`COMMIT`/`ROLLBACK` with no open block** is a **lenient no-op success** — it always has a
  well-defined action (publish/discard the current work, of which there is none), so it succeeds
  rather than erroring. PostgreSQL emits a *warning* here; jed has no warning channel
  (CLAUDE.md §4), so `25P01` is deliberately **not** raised — a documented divergence.

The asymmetry is the rule "error where the action is undefined, succeed where it is defined."

**Keywords stay non-reserved (§3):** `BEGIN`, `START`, `COMMIT`, `END`, `ROLLBACK`, `WORK`,
`TRANSACTION`, `READ`, `ONLY`, `WRITE` are recognized positionally — a column may still be named
`begin` or `work`.

**Deferred (transactions.md §11):** `SAVEPOINT` / `ROLLBACK TO` / nested transactions;
`SET TRANSACTION ISOLATION LEVEL` (snapshot isolation is the single level); the
`[NOT] DEFERRABLE` and `ISOLATION LEVEL` modifiers on `BEGIN`; `synchronous=off` batching. Only
the access-mode modifier (`READ ONLY` / `READ WRITE`) is accepted on the opener.

## 28. Table-level `PRIMARY KEY` (the composite-key constraint)

A `CREATE TABLE` element is now a `table_element` — a `column_def` **or** the one table
constraint, `PRIMARY KEY ( ident [, ident]* )`, which may appear anywhere among the column
definitions (PostgreSQL's shape). Semantics — member resolution, the implied `NOT NULL`,
the concatenated key bytes, uniqueness over the tuple, and the key-order narrowing — live in
[constraints.md §3](constraints.md); this section is the parser surface.

**Disambiguation.** The constraint keywords stay non-reserved (§3): an element beginning with
the two keywords `PRIMARY` `KEY` parses as the table constraint, anything else as a
`column_def`. Nothing is lost — a column named `primary` would need a *type* named `key`,
which does not exist (42704 at resolve), so no valid column definition starts that way. The
member list must be non-empty and parenthesized; `PRIMARY KEY ()` is a `42601` syntax error
(the first `expect_identifier` rejects `)`).

**Where the errors fire.** The parser accepts any well-formed list — including several
table constraints — and CREATE TABLE's execution resolves it: unknown member `42703`,
repeated member `42701`, more than one primary key across both forms `42P16`, a
non-keyable member type or an out-of-declaration-order list `0A000` (constraints.md §3).
Keeping resolution in the executor matches every other name-resolution error in the
surface (the parser knows no catalog).

## 29. `CHECK` constraints (`[CONSTRAINT name] CHECK ( expr )`)

Both constraint positions gain the same form: a `column_constraint` and a `table_constraint`
may each be `["CONSTRAINT" identifier] "CHECK" "(" expr ")"`. The two positions are
semantically identical (either may reference any column of the table); semantics —
validation, naming, name-order evaluation, `23514`, persistence — live in
[constraints.md §4](constraints.md). This section is the parser surface.

**Disambiguation.** `CHECK` and `CONSTRAINT` stay non-reserved (§3). A table element
beginning with the keyword `CHECK` **followed by `(`** parses as an unnamed check
constraint; one beginning with `CONSTRAINT` followed by an identifier and `CHECK` parses as
a named one. Nothing is lost: a column named `check` is followed by a *type name* (an
identifier, never `(`), and a column named `constraint` would need its second-next token to
be a type, never the keyword `check` followed by `(` — no valid `column_def` collides. In
column-constraint position the same lookahead applies after the column's type.

**The expression is captured for persistence.** The parser records the token span between
the constraint's parentheses; the persisted text is that token sequence re-rendered
([format.md](../fileformat/format.md) "Check-expression text"). The parse itself is the
ordinary `expr` production — `CHECK ()` is `42601`, and the parentheses are required.

**Where the errors fire.** The parser accepts any well-formed expression — including ones
the constraint must reject — and CREATE TABLE's execution validates: subquery `0A000`,
aggregate `42803`, bind parameter `42P02`, unknown column `42703`, non-boolean `42804`,
duplicate name `42710` (constraints.md §4.1–§4.3). The parser knows no catalog.

## 30. `CREATE INDEX` / `DROP INDEX`

Two new top-level statements ([indexes.md](indexes.md)):

```
create_index ::= "CREATE" "UNIQUE"? "INDEX" identifier? "ON" identifier
                 "(" identifier ("," identifier)* ")"
drop_index   ::= "DROP" "INDEX" identifier
```

**`UNIQUE` needs no lookahead of its own**: after `CREATE`, the next word being `UNIQUE`
can only be this form (`CREATE TABLE`/`CREATE INDEX` are the only `CREATE` statements, and
a table cannot be named by position two). `UNIQUE` stays non-reserved everywhere else. The
flag's semantics — the build-time duplicate check (`23505`) and write-time enforcement —
live in [indexes.md §8](indexes.md).

**Disambiguating the optional name.** No word is reserved (§3), so `ON` may itself name
an index or table. The rule, byte-identical across the three parsers: after
`CREATE INDEX`, the next word is the **index name UNLESS** it is `ON` followed by a word
followed by `(` — that exact three-token shape can only be the unnamed form's
`ON table (`. Both readings stay reachable: `CREATE INDEX ON t (a)` is unnamed (the
lookahead sees `ON t (`), while `CREATE INDEX on ON t (a)` names the index `on` (the
lookahead sees `ON ON t`, not `ON word (`), and `CREATE INDEX ON on (a)` is the unnamed
form over a table named `on`. PostgreSQL reserves `ON` so the ambiguity cannot arise
there; the lookahead is jed's standing no-reserved-words mechanism (the same move as
`DISTINCT`'s and `CHECK`'s).

**Key columns are bare identifiers.** A `(` in key position is a `42601` syntax error
(no expression keys — a documented narrowing, indexes.md §1), as are `ASC` / `DESC` /
`NULLS` after a key (they parse as an unexpected token; PostgreSQL accepts them).

**Where the errors fire.** The parser knows no catalog; CREATE INDEX's execution
validates in PostgreSQL's order — table `42P01`, then each key column in list order
(`42703` unknown / `0A000` unindexable type), then the explicit name against the shared
relation namespace (`42P07`) — and DROP INDEX raises `42704` (missing) / `42809` (names
a table). Semantics: [indexes.md §2](indexes.md).

## 31. `UNIQUE` constraints (`[CONSTRAINT name] UNIQUE [( cols )]`)

Both constraint positions gain the `UNIQUE` form ([constraints.md §5](constraints.md)): a
`column_constraint` may be `["CONSTRAINT" identifier] "UNIQUE"` (the one-member form over
its own column), and a `table_constraint` may be
`["CONSTRAINT" identifier] "UNIQUE" "(" identifier ("," identifier)* ")"` (the member list
reuses the `PRIMARY KEY` list shape — bare column names, non-empty).

**Disambiguation.** `UNIQUE` stays non-reserved (§3). A table element beginning with the
keyword `UNIQUE` **followed by `(`** parses as an unnamed unique constraint — a column
named `unique` is followed by a *type name* (an identifier, never `(`), so nothing is
lost. One beginning with `CONSTRAINT` dispatches on the keyword after the name: `CHECK`
(§29) or `UNIQUE`; at table level the named unique requires the `(` list, at column level
the bare keyword ends the form. In column-constraint position `UNIQUE` is one keyword in
the order-free constraint loop, like `NOT NULL` (a repeat is harmless — the identical
constraint folds, constraints.md §5).

**Where the errors fire.** The parser knows no catalog; CREATE TABLE's execution resolves
members (`42703`/`42701`/`0A000`) and names the backing index (`42P07`/`42710`)
(constraints.md §5). PostgreSQL *reserves* `UNIQUE`, so a column named `unique` is
jed-only surface (the standing no-reserved-words stance; such corpus records carry oracle
overrides like `on`'s — conformance.md §5).

## 32. `RETURNING` — DML that produces rows

`INSERT`, `UPDATE`, and `DELETE` take an optional **terminal** `RETURNING` clause —
`returning_clause ::= "RETURNING" select_items` — turning the statement into one that
produces a **query result** (column names + rows, the same `Outcome` shape a `SELECT`
returns; [api.md](api.md) §3). The item list is the ordinary `select_items` production:
general expressions with optional `AS` output labels, or the standalone `*` glob expanding
to every column in declaration order. Output names follow §8 unchanged. All semantics
below are PostgreSQL's, probed against the live oracle (CLAUDE.md §1).

**Which row each statement returns.**

- **`INSERT`** returns each **stored row** — after the column list / `DEFAULT` fill-in and
  after type coercion, i.e. exactly the values written. Both sources (`VALUES` and
  `SELECT`) take the clause; in `INSERT ... SELECT ... RETURNING` the clause belongs to
  the INSERT (it projects the *inserted* rows, after the source's own optional
  `ORDER BY`/`LIMIT` ran).
- **`UPDATE`** returns each matched row's **new** (post-assignment) values.
- **`DELETE`** returns each deleted row's **old** values.

A statement that affects **zero rows** succeeds with an **empty result** (not an error).
Without `RETURNING` the statement produces no result set, exactly as before.

**Resolution — a one-relation scope over the target table.** Items resolve against the
target table only (bare or table-qualified references): an unknown column is `42703`, an
unknown qualifier is `42P01` (*"missing FROM-clause entry for table rel"* — §15), and
resolution precedes execution, so a `42703` beats a would-be `23505` (probed). An
aggregate call is **`42803`** (*aggregate functions are not allowed in RETURNING* — jed
reports its standing generic message under the same code). Subqueries are **allowed**,
correlated ones included — an outer reference reads the row being returned (the new row
under UPDATE, the old row under DELETE, the candidate row under INSERT). `$N` binds as
anywhere else (api.md §5); a parameter typed by nothing is `42P18`.

**The pre-statement snapshot.** A subquery in the list observes the database **as of the
start of the statement** — `INSERT INTO t ... RETURNING (SELECT count(*) FROM t)` counts
**0** over an empty table, an UPDATE's subquery sees pre-update values, a DELETE's sees
the rows still present (all probed). Operationally: projections evaluate after the
statement's validation completes and **before any write** (the two-phase model's phase
boundary, [constraints.md](constraints.md) §4.4), which also keeps a ceiling abort
(`54P01`) all-or-nothing — a statement aborted mid-RETURNING has written nothing.

**Row order is unspecified** (CLAUDE.md §8): `RETURNING` takes no `ORDER BY` (one after
the clause is `42601`, as in PostgreSQL), so the corpus compares its results `rowsort`.
PostgreSQL happens to emit processing order; jed pins only the multiset.

**Keyword + disambiguation.** `RETURNING` stays non-reserved (§3) and is recognized
positionally as the trailing clause of the three DML statements. The single collision —
an `INSERT ... SELECT` source whose final `table_ref` could swallow `returning` as an
implicit alias — is settled by the §15 stop-keyword set, which `returning` joins: a bare
word `returning` after a `table_ref` is never an implicit alias (PostgreSQL fully
reserves `RETURNING` and rejects even `AS returning`; jed's explicit-`AS` form stays
legal — the standing no-reserved-words divergence, oracle-overridden like `on`'s).
`RETURNING` with an empty list is `42601` (the item parser requires an expression).

**The `old.`/`new.` row-version qualifiers** (PostgreSQL 18 semantics, probed). Inside a
`RETURNING` list — and only there — the qualifiers `old` and `new` name the affected row's
two versions: `old.col` is the pre-statement value, `new.col` the post-statement value.
The side a statement does not produce is the **all-NULL row**: an `INSERT`'s `old.col` is
NULL (every column, the key included), a `DELETE`'s `new.col` is NULL. Bare and
table-qualified references keep their §32 meaning unchanged (the new row under
INSERT/UPDATE, the old row under DELETE) — so `new.v - old.v` is an UPDATE's delta, and
for an unassigned column `old.v = new.v`. The qualifiers work anywhere in an item
expression, **including inside subqueries** (they resolve like any outer reference —
probed: `RETURNING (SELECT old.v + s.a FROM s ...)`). Resolution rules:

- **Only in `RETURNING`.** In any other clause (`SET`, `WHERE`, a `SELECT`), `old`/`new`
  are ordinary identifiers — an unknown qualifier is the usual `42P01`
  (*missing FROM-clause entry*), exactly PostgreSQL's behavior (probed).
- **The table name shadows the qualifier** (probed): if the target table is itself named
  `old` (or `new`), that qualifier keeps its ordinary table-qualified meaning — the
  row-version pseudo-relation is suppressed. PostgreSQL recovers the hidden version via
  `RETURNING WITH (OLD AS o, NEW AS n)` aliasing; jed defers that form (a deliberate,
  relaxable narrowing — the unaliased qualifiers are the whole feature surface this
  slice; [../../TODO.md](../../TODO.md)).
- An unknown column under a qualifier is `42703`; **bare** `old`/`new` are ordinary column
  references (a column may be named either — they resolve normally, never to a row
  version); `old.*`/`new.*` follow the standing no-qualified-star narrowing (§15).
- **Output names** follow §8 unchanged: `RETURNING old.v` names the column `v` (the
  qualifier never leaks — matches PostgreSQL).

Operationally the projection row is the **concatenation** `[base | other]` of the two
versions (base = what bare references read; other = the opposite version, the all-NULL
row when the statement has no such version), and `old`/`new` are **qualifier-only**
pseudo-relations over it — invisible to bare-column resolution (no new ambiguity) and to
every other statement's scope. Cost: the qualifiers are leaves like any column, and the
**touched set** distinguishes the sides — [cost.md](cost.md) §3 "`RETURNING`".

**Cost.** Each returned row charges `row_produced` plus its items' metered evaluation,
and the items' column references join an UPDATE/DELETE's touched set —
[cost.md](cost.md) §3 "`RETURNING`".

## 33. Comments are whitespace

The lexer treats SQL comments as whitespace, exactly as PostgreSQL does. Two forms
([grammar.ebnf](../grammar/grammar.ebnf) conventions block):

- **Line comments.** Two hyphens (`--`) start a comment running to the end of the line
  (the next LF or CR, or end of input). The two hyphens **always** start a comment
  outside a string literal, even when abutting a token: `1--2` lexes as the single
  integer `1` (PostgreSQL behavior). `1- -2` (with the operators separated) remains
  `1 - (-2)`.
- **Block comments.** A slash-star opens a block comment and a star-slash closes it;
  blocks **nest**, per PostgreSQL and the SQL standard, so the lexer tracks a depth
  counter and the comment ends only when the depth returns to zero. A block comment is
  not scanned for string quotes (an apostrophe inside one is ordinary comment text).
  An **unterminated** block comment — end of input at depth ≥ 1 — is a `42601` syntax
  error (`unterminated /* comment`).

Comment openers inside a `'...'` string literal are ordinary text (`'--x /*y*/'` is just
characters). A stray star-slash with **no** opener is *not* comment syntax — it lexes as
the two operator tokens `*` `/` and fails at parse (`42601`), matching the spirit of
PostgreSQL's rejection.

Because a comment is whitespace, it is also a **token separator**: `SELECT/*c*/v` lexes
as `SELECT v`. Comments carry no semantic content anywhere in the engine — they do not
survive into the AST, and constructs that persist re-rendered statement text (the CHECK
catalog, §29) never see them (rendering works from tokens).

An input that is *only* comments (or empty after comment stripping) is still "no
statement" and parses as `42601` — the one-statement-per-input rule (§1) is unchanged.

## 34. FROM-less `SELECT` (the virtual row)

The `FROM` clause is **optional** ([grammar.ebnf](../grammar/grammar.ebnf)
`select_core`). A `SELECT` with no `FROM` evaluates its select list over **one virtual
zero-column row**, exactly PostgreSQL's model: `SELECT 1` returns one row, no table is
touched, and no scan cost accrues — the only charges are the items' metered evaluation
plus `row_produced` per emitted row ([cost.md](cost.md) §3). `SELECT 1` costs exactly 1.

Every clause composes over the virtual row with its ordinary semantics:

- **`WHERE`** filters it: `SELECT 1 WHERE false` returns zero rows (cost 0 — the
  constant filter is a leaf, and no row is produced).
- **Aggregates** fold it through the single-group rule ([aggregates.md](aggregates.md)
  §4): `SELECT count(*)` is `1`; with a false `WHERE` the group still emits
  (`count` → `0`, other aggregates → `NULL`). `HAVING` filters that single group
  (`SELECT 1 HAVING false` → zero rows).
- **`DISTINCT`**, and a lone query's **`ORDER BY` / `LIMIT` / `OFFSET`**, apply to the
  0-or-1-row result unchanged.
- It is a **full citizen of composition**: a set-operation operand
  (`SELECT 1 UNION SELECT 2`), a subquery in any position — including a **correlated**
  one whose zero-relation scope resolves purely outward (`SELECT (SELECT o.id) FROM t o`)
  — and an `INSERT ... SELECT` source (`INSERT INTO t SELECT 1`).

**Errors.** With zero relations in scope there is nothing for a star or a column to
bind to:

- `SELECT *` (no FROM) — `42601`, message `SELECT * with no tables specified is not
  valid` (PostgreSQL's exact message; raised at projection resolution, so the RETURNING
  `old`/`new` qualifier-only scope of §32 is unaffected).
- A bare or qualified column reference — the existing `42703` (top level) or an outer
  resolution in a subquery (§26). `GROUP BY` / `ORDER BY` keys are table columns only
  (§5/§10), so on a lone FROM-less SELECT they are always `42703`; a set operation's
  trailing `ORDER BY` still resolves by **output name** (§25), so
  `SELECT 2 AS x UNION SELECT 1 ORDER BY x` is legal.
- A parameter typed by nothing (`SELECT $1`) is `42P18`, the ordinary rule (§5).

**Documented divergences from PostgreSQL** — consequences of jed's non-reserved
keywords (§3) and the ≥ 1 select-item rule, not separate decisions:

- **`SELECT` with an empty target list** (legal in PG, returning zero-column rows)
  stays `42601`: a zero-column result is unrepresentable in jed's result surface and
  conformance format, and buys nothing ("we own our surface", CLAUDE.md §1).
- **`SELECT from`** reads `from` as a column reference → `42703` (PG parses an empty
  target list). `SELECT from t` is a leftover-token `42601`.
- **`SELECT distinct`** at end of input — the §11 two-token DISTINCT lookahead is
  unchanged, so the word is a column reference → `42703`. Likewise
  `SELECT distinct WHERE ...` keeps treating `distinct` as the modifier and fails as a
  leftover-token `42601` where PG would run it.
- **`SELECT 1 x`** (implicit item alias) remains `42601` — aliasing requires explicit
  `AS` (§5); only the message changes (leftover tokens rather than a failed `FROM`
  expectation).

None of these forms appear in the conformance corpus (they would pin oracle overrides
for zero value); the corpus tests the PG-agreeing surface.

## 35. Set-returning functions in `FROM` (`generate_series`)

A `table_ref` may be a **set-returning function (SRF)** call instead of a base table name
([grammar.ebnf](../grammar/grammar.ebnf) `table_function`): `SELECT * FROM
generate_series(1, 5)`, `SELECT * FROM unnest(ARRAY[10,20,30])`. An SRF is a **computed row
source** — it *expands* its arguments into a row set rather than scanning stored rows. The
semantics, the synthetic-relation model, and the cost rule live in [functions.md](functions.md)
§10; the two SRFs — `generate_series` (integer series) and the polymorphic `unnest(anyarray)`
([array-functions.md §9](array-functions.md), a row per array element) — are registered as shared
catalog data ([catalog.toml](../functions/catalog.toml) `[[set_returning]]`). The grammar is
identical for both: a single `table_function` production keyed on a `(` after the leading
identifier — the resolver dispatches by name.

**Syntax.** `table_function ::= identifier "(" expr ("," expr)* ")"` — a `(` immediately
after the leading identifier in `table_ref` position marks the function form (a **one-token
lookahead**, a §8 cross-core determinism surface; a bare table name has no `(` there). The
arguments are general expressions parsed exactly like a `function_call`'s, minus the `*` /
`DISTINCT` forms (those are the aggregate/star spellings, not an SRF argument list). The
optional alias is parsed identically to a base table's.

**Labeling and the single-column-alias rule.** The relation's **label** is the alias, or the
function name when there is none. The produced relation has **one column**; its **name**
follows PostgreSQL's single-column function-alias rule: the alias when one is given
(`generate_series(1, 5) AS g` ⇒ column `g`, so `g.g` resolves and `g.generate_series` is
`42703`), else the function name (`generate_series(1, 5)` ⇒ column `generate_series`, so
`generate_series.generate_series` resolves). Oracle-verified against PostgreSQL.

**Composition.** An SRF relation is a first-class FROM item: it joins/cross-joins other
relations (`t CROSS JOIN generate_series(1, 3)` is the product), and `WHERE` / `ORDER BY` /
`LIMIT` / subqueries compose over it. Its arguments are **implicitly `LATERAL`** (§44, matching
PG): a `$N` parameter, a **correlated outer-query column** (`(SELECT count(*) FROM
generate_series(1, o.n)) FROM t o`), **and** a **column of an earlier sibling FROM relation**
(`FROM t CROSS JOIN generate_series(1, t.n) g`) are all legal arguments — a sibling reference
re-evaluates the SRF once per left-hand row (§44). (For the first/only FROM item there is no
sibling, so an arg sees only `$N`/outer, as before.)

**Deferred narrowings** (each a `0A000` or the relevant error, relaxable later, and shared by both
SRFs): the **SELECT-list** SRF position (`SELECT generate_series(1, 5)` / `SELECT unnest(…)` — an
SRF is not a scalar function, `42883`), the **column-alias-list** form `AS g(c1, …)` (`0A000` — a
`(` after the alias), **`WITH ORDINALITY`**, and `generate_series`'s non-integer variants
(numeric/timestamp). The `generate_series` integer forms and their PostgreSQL edge cases (NULL arg →
zero rows, step zero → `22023`, overflow → clean stop), and `unnest`'s element-expansion semantics,
are spec'd in [functions.md](functions.md) §10 and [array-functions.md §9](array-functions.md).

## 36. Typed string literals (`type '…'`) and string-literal casts

A `primary` may be a **typed string literal** — *any* type name immediately followed by a
single-quoted string ([grammar.ebnf](../grammar/grammar.ebnf) `typed_literal`):

```
typed_literal ::= identifier string
```

This is PostgreSQL's `type 'string'` form, which is exactly `CAST('string' AS type)` restricted
to a string-literal operand ([types.md](types.md) §5). The type name **names the type**, so the
literal carries it in **any** expression position — independent of surrounding context:
`SELECT TIMESTAMP '2024-01-01 12:00:00'`, `SELECT INTEGER '42'`, `SELECT NUMERIC '1.5'`,
`SELECT BOOLEAN 'true'`, `SELECT BYTEA '\xDE'`, `SELECT UUID '…'`, `SELECT TEXT 'hi'`, and
`TIMESTAMP '2024-01-31' + INTERVAL '1 month'` (arithmetic spelled entirely with literals). The
string is **coerced to the named type at resolve time**, before any scan — so an unknown type
name is `42704` (undefined_object), and a malformed/out-of-range string traps the type's parse
code (below).

**One coercion, three syntaxes.** The literal's string is run through the **same** resolve-time
coercion as `CAST('string' AS type)` and as a bare string adapting to a column — `type 'x'` is
just "coerce the string `x` to that type, with the type named explicitly." The per-type rules:

| target | rule | error codes |
|---|---|---|
| `timestamp` / `timestamptz` | the §3 datetime parse ([timestamp.md](timestamp.md)); tz normalizes the offset to UTC | `22007` / `22008` |
| `date` | the ISO `YYYY-MM-DD` parse ([date.md](date.md) §2); a trailing time/offset is validated then dropped | `22007` / `22008` |
| `interval` | the "unit + time" subset ([interval.md](interval.md) §3) | `22007` / `22008` |
| `bytea` | `\x`-hex input ([types.md](types.md) §13) | `22P02` |
| `uuid` | PG-flexible uuid input ([types.md](types.md) §14) | `22P02` |
| `text` | identity (the string itself) | — |
| `i16` / `i32` / `i64` | optional sign + decimal digits, surrounding whitespace trimmed | `22P02` malformed / `22003` out of range |
| `decimal` / `numeric` | jed's decimal-literal grammar (sign, digits, one `.`), whitespace trimmed; capped | `22P02` malformed / `22003` over cap |
| `boolean` | PG's `boolin`: `t`/`tr`/`tru`/`true`, `f`/`fa`/`fal`/`fals`/`false`, `y`/`ye`/`yes`, `n`/`no`, `on`/`off`, `1`, `0` (case-insensitive, trimmed) | `22P02` malformed |

The native-syntax types (`integer`/`decimal`/`boolean`) are where `type 'string'` is a genuine
**cast from text** — coercing a *string* to a number/bool. jed allows it **only when the operand
is a string literal** (the `type 'string'` form and `CAST(<string literal> AS T)`), folded at
resolve. A **runtime** text→`T` cast on a non-literal text expression (`CAST(text_col AS int)`)
stays deferred (`0A000`, [types.md](types.md) §5). And a **bare** string still does **not**
silently become a number/bool in a numeric context (`WHERE int_col = '42'` is `42804`, the strict
rule — [types.md](types.md) §4): the type must be *named* for the string→number coercion to
happen. So strictness is preserved; only the *explicit* spelling is admitted.

**Disambiguation — the type names stay non-reserved (§3).** A word introduces a typed literal
**only** when the *next* token is a string; otherwise it is an ordinary identifier, so a column
named `timestamp` / `interval` / `integer` still parses (`SELECT timestamp FROM t`,
`SELECT interval + 1 FROM t`). This is the one-token lookahead used for `CAST` / `EXISTS` /
function names — a §8 cross-core determinism surface, byte-identical across the three hand-written
parsers. (`true` / `false` / `null` are excluded from the type-name position — they are their own
value literals, so `true 'x'` is not `CAST('x' AS true)`.)

**Documented divergences from PostgreSQL** (we own our surface, CLAUDE.md §1):

- For `integer`/`decimal`, jed coerces by its **own literal grammar**, not PG's input-function
  extras: **hex/octal/binary** (`integer '0x10'`), **digit underscores** (`integer '1_000'`),
  **scientific notation** (`numeric '1.5e3'`), and **`NaN` / `±Infinity`** (`numeric 'NaN'` — jed's
  decimal is always finite, [decimal.md](decimal.md) §2) all trap `22P02` where PG accepts them.
  This keeps `type 'string'` coercion identical to writing the value as a native jed literal.
- jed uses the **canonical single-word** type names: PG's multi-word `TIMESTAMP WITH TIME ZONE
  '…'` and the **precision typmod** form `TIMESTAMP(p) '…'` / `NUMERIC(p,s) '…'` are not typed
  literals here (a `(` after the name breaks the lookahead; typmod rides only on `CAST`).
- **ISO-8601 / SQL-standard combined interval forms** inside `INTERVAL '…'` remain deferred
  ([interval.md](interval.md) §3) — a parse-subset gap, not a literal-syntax one.

`DATE '…'` / `TIME '…'` are absent only because those types are not in the scalar set.

## 37. The `::` cast operator (`expr :: type`)

`expr :: type_name` is PostgreSQL's postfix typecast operator, and it is **exactly**
`CAST(expr AS type_name)` ([grammar.ebnf](../grammar/grammar.ebnf) `postfix`,
[types.md](types.md) §5). It is pure surface sugar: the parsers **desugar `::` to the existing
`Cast` AST node at parse time**, so there is one resolver path, one evaluator path, one cost, and
one cross-core contract for both spellings. Everything the cast machinery already does carries
over unchanged:

- the **cast matrix** ([../types/casts.toml](../types/casts.toml)) — `'42' :: int` (string-literal
  coercion, the same primitive as the `integer '42'` typed literal of §36), `x :: int8` (widen),
  `x :: int2` (narrow, traps `22003`), `d :: int` (decimal→int, round half-away), `n :: numeric(10,2)`
  (re-scale to the typmod);
- the **deferred narrowings** — casting **to or from** `text` / `boolean` / `bytea` / `uuid` /
  `timestamp` / `timestamptz` / `interval` is `0A000` (except a *string-literal* operand, which
  coerces). `5 :: text` is `0A000`, identical to `CAST(5 AS text)`; `'5' :: text` is the string
  identity `'5'`;
- the resolve codes `42704` (unknown type) / `22003` (out of range) / `22P02` (malformed string) /
  `22023` (bad typmod), and a **typmod on the type name** (`x :: numeric(10,2)`), exactly as CAST.

**Chaining is left-associative.** `x :: int8 :: int2` is `(x :: int8) :: int2` — the parser loops,
wrapping each `Cast` around the previous. So `42 :: int8 :: int2` is `42` widened to i64 then
narrowed to i16 (= 42), and `9999999999 :: int8 :: int2` traps `22003` at the **inner** narrow's
eval.

**Precedence — `::` binds tighter than unary minus** (PostgreSQL's operator table: `::` sits just
below the `.` qualifier, above unary `+`/`-`). So:

```
-5 :: int        ==  -(5 :: int)        -- NOT (-5) :: int
-32768 :: i16  ->  22003              -- inner 32768 overflows i16, THEN negate
(-32768) :: i16 ->  -32768            -- parenthesized: the in-range value
1 + 2 :: int8    ==  1 + (2 :: int8)    -- tighter than additive, too
```

This matters because of §4's **leading-`-`-of-a-literal fold** (which makes `i64`'s minimum
representable as `-9223372036854775808`). That fold is **suppressed when a `::` immediately follows
the numeric literal**, so `-N :: T` parses as `-(N :: T)` (the cast applies to the unsigned
magnitude first), matching PG. A bare `-N` with no trailing `::` still folds as before — no
regression. The suppression is a one-token lookahead on the token *after* the literal, a §8
cross-core determinism surface, byte-identical across the three hand-written parsers.

**Bind parameters — `$1 :: int` types the parameter.** A bind-parameter operand of a cast takes
the cast **target** as its inferred type ([api.md](../design/api.md) §5): `$1 :: int` declares `$1`
as `int`, `$1 :: numeric(10,2)` declares it `decimal` and re-scales the bound value to `(10,2)`.
This is the same parameter-typing rule the `CAST($1 AS int)` spelling already documents; both now
infer the type rather than reporting `42P18`. (A *bare* `SELECT $1` with no cast and no other
context is still `42P18` indeterminate.) Casting a parameter to a deferred target (`$1 :: text`) is
`0A000`, like any non-literal cast to text.

**Lexing.** `::` is two colons scanned greedily as one token; a **lone** `:` is a `42601` syntax
error (jed has no `:name` host parameters, array slices, or `psql` meta-syntax — nothing else uses
a colon). See [grammar.ebnf](../grammar/grammar.ebnf) `double_colon`.

**Divergence note.** Because casting to non-string-literal text/boolean/etc. is still the deferred
`0A000` narrowing (§36, [types.md](types.md) §5), `5 :: text`, `x :: boolean`, etc. are `0A000`
where PostgreSQL succeeds — the *same* documented divergence the `CAST(... AS ...)` spelling
already carries, not a new one. `::` adds no behavior of its own; it only adds the spelling.

## 38. Composite field selection (`(expr).field` / `(expr).*`)

Field selection reads one named field of a composite value, and `(expr).*` expands a composite into
all its fields (spec/design/composite.md §1, §S4). Both are **postfix operators at the `::` cast
level** ([grammar.ebnf](../grammar/grammar.ebnf) `postfix`), so they chain — with `::` and with each
other, in token order: `(s).p.x`, `(c).a :: int8`. The parser builds an `Expr::FieldAccess { base,
field }` for `.field` and `Expr::FieldStar { base }` for `.*`.

- **Field access is parens-required** (PostgreSQL): `.field` / `.*` applies only to a
  **parenthesized** base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1`, `('(…)'::addr).zip` — and
  chains on a prior field access (`(c).a.b`). The parser tracks field-accessibility: a primary is
  field-accessible iff it started with `(`, and a `.field` keeps the chain accessible (a `::` cast
  does not). So `.field` fires only on a parenthesized / chained-field base.
- **The unparenthesized `a.b` / `a.b.c` form is a (multi-part) column reference, never field
  access.** `home.zip` is consumed by `column_ref` as a qualified column whose qualifier `home` must
  name a relation — else `42P01` (missing FROM-clause entry), exactly as PG. There is **no** bare
  `col.field` fallback (the original plan assumed one; the differential oracle showed PG rejects
  every unparenthesized field reference, so jed matches PG). To select a field of a composite
  column you must parenthesize: `(home).zip`.
- **Errors.** Field lookup is case-insensitive (PG folds the identifier). An unknown field is
  `42703` (undefined_column); a non-composite base is `42809` (wrong_object_type, PG's "column
  notation … applied to type …, which is not a composite type"); `.*` outside a projection list is
  `0A000`.
- **Output name.** An un-aliased `(expr).field` is named after the **field** (PG); `(expr).*`
  contributes one output column per field, each named after that field, in declaration order.

Field selection adds no on-disk format change and no new cost unit — `(expr).field` is one interior
expression node (one `operator_eval`), and `(expr).*` is N independent field-selection nodes.

## 39. The `||` array concatenation operator (`a || b`)

`||` is the array **concatenation operator** ([array-functions.md §8](array-functions.md)) — the
operator spelling of the AF1 builders, with three polymorphic overloads (`array || array` →
`array_cat`, `array || element` → `array_append`, `element || array` → `array_prepend`). It is the
one grammar change AF2 makes:

- **Token.** Two `|` are scanned greedily into a single `||` token (like `::` / `=>`). A lone `|`
  is a `42601` syntax error — jed has no bitwise-or operator.
- **Precedence.** A new `concat` rung sits between the comparison level and the additive level
  ([grammar.ebnf](../grammar/grammar.ebnf) `concat`; `precedence = 37` in
  [../functions/catalog.toml](../functions/catalog.toml)). This is PostgreSQL's "any other
  operator" rung: `||` binds **tighter than the comparisons** (`a || b = c` is `(a || b) = c`) and
  **looser than `+`/`-`**, and is **left-associative** (`a || b || c` is `(a || b) || c`). Every
  comparison/`IN`/`BETWEEN`/`LIKE` operand parses at the `concat` level, so `||` is usable inside
  them.
- **AST + resolution.** One `BinaryOp::Concat` node; the resolver (`resolve_concat`) does overload
  resolution over the three `concat` catalog rows in order (cat, append, prepend) and reuses the
  AF1 array kernels. A bare untyped `NULL` operand resolves to `array_cat` (the NULL array is the
  identity), matching PostgreSQL; a typed null element resolves to `array_append`
  (array-functions.md §8.1). No matching overload — including text `||` and `int || int`, both
  deferred — is `42883`.

## 40. The array containment / overlap operators (`@>` / `<@` / `&&`)

`@>` (contains), `<@` (contained by), and `&&` (overlaps) are the array **containment/overlap
operators** ([array-functions.md §10](array-functions.md)) — three polymorphic
`anyarray <op> anyarray → boolean` operators, the one grammar change AF4 makes:

- **Tokens.** `@>` is `@` then `>`; `<@` is `<` then `@`; `&&` is two `&`. Each is scanned greedily
  into a single token. A lone `@` and a lone `&` are `42601` syntax errors (jed has no
  unary-`@`/bitwise-and); a lone `<` stays the comparison `<` (only `<@` and `<=` extend it).
- **Precedence.** They join the existing `concat` rung (the "any other operator" level, `precedence
  = 37`) — sharing `||`'s precedence, **left-associative**, binding **tighter than the comparisons**
  and **looser than `+`/`-`**. The `concat` production gains them as alternatives
  ([grammar.ebnf](../grammar/grammar.ebnf) `concat`), so `a || b @> c` is `(a || b) @> c` (matching
  PostgreSQL). They are usable inside every comparison/`IN`/`BETWEEN`/`LIKE` operand.
- **AST + resolution.** Three `BinaryOp` nodes (`Contains` / `ContainedBy` / `Overlaps`); the
  resolver (`resolve_containment`) unifies the two operands' element types over the single
  `(anyarray, anyarray)` overload (a non-array operand or an element-type mismatch is `42883`),
  with the same literal adaptation `resolve_concat` uses (so `xs @> ARRAY[20]` adapts the
  constructor to `xs`'s element type). The operators are **strict** (a NULL whole-array operand →
  NULL); their element matching is **strict equality** — a NULL element matches nothing — over the
  flattened element multiset (any dimensionality), the one place they differ from the search
  functions' NOT DISTINCT FROM (array-functions.md §10).

## 41. The `ANY` / `ALL` quantified array comparisons (`x op ANY(arr)`, `x op ALL(arr)`)

A comparison operator may be followed by a **quantifier** — `ANY`/`SOME` (existential) or `ALL`
(universal) — over a parenthesized **array** expression ([array-functions.md §11](array-functions.md)).
`x = ANY(arr)` is the array spelling of `IN`; `x op ALL(arr)` is its universal dual. It is the one
grammar change AF5 makes — **no new tokens** (`ANY`/`SOME`/`ALL` are plain keywords):

- **Grammar.** A `compare_op` (`= < > <= >=`) is followed by *either* a quantifier `(`expr`)` *or*
  the ordinary `concat` right operand ([grammar.ebnf](../grammar/grammar.ebnf) `comparison`). The
  parser, after taking the operator, peeks `ANY`/`SOME`/`ALL`; if present it consumes `( expr )` and
  builds an `Expr::Quantified { op, all, lhs, array }` (`all = true` for `ALL`, `false` for
  `ANY`/`SOME`). The operand is a **full `expr`** (so `ARRAY[…]`, `'{…}'::T[]`, a column, or a `||`
  expression all fit). It is **non-associative**, like the comparisons it extends.
- **`SOME` is `ANY`** (SQL-standard synonym), folded at parse time.
- **Semantics** ([array-functions.md §11](array-functions.md)): three-valued over the array's
  flattened elements — `ANY` is the OR-fold (TRUE if any `x op e` is TRUE; else NULL if any is NULL;
  else FALSE; **empty → FALSE**), `ALL` is the AND-fold (FALSE if any is FALSE; else NULL if any is
  NULL; else TRUE; **empty → TRUE**). A NULL array operand → NULL. This reuses the `IN`-list 3VL
  membership machinery (`x = ANY(arr)` ≡ `x IN (the elements)`), generalized to all five comparison
  operators and both quantifiers.
- **Resolution.** `x` and the array operand are resolved with the same literal adaptation the
  comparison operators use (a bare-literal `x` adapts to the element type; a bare `ARRAY[…]` adapts
  to `x`'s type). The right operand must be an **array** — a non-array right side is **`42809`**
  (`op ANY/ALL (array) requires array on right side`, PG) — whose element type must be **comparable**
  with `x` (else `42883`, PG's `operator does not exist`). The result is always `boolean`. A bare
  untyped `NULL` array operand is **`42P18`** (jed's polymorphic indeterminate posture, §11).
- **The subquery form** `x op ANY/ALL(SELECT …)` (the subquery spelling of `IN`) also resolves: a
  leading `SELECT` after the quantifier's `(` selects it (the §26 lookahead), folding over the
  subquery's single column (`42601` if >1) exactly as `IN` does — three-valued, no `21000` cardinality
  limit, uncorrelated-folded / correlated-re-executed per §26, incomparable types `42883`
  ([array-functions.md §11.6](array-functions.md)).

## 42. Derived tables (`FROM ( query_expr ) AS t`)

A **derived table** is a parenthesized subquery used as a relation in the `FROM` clause:
`SELECT d.id FROM (SELECT id, n FROM t WHERE n > 0) AS d`. It is the FROM-position sibling of the
expression-position subqueries (§26): the same `( query_expr )` body, now a **computed row source**
rather than a scalar/`IN`/`EXISTS` operand. The body is any query expression — a `SELECT`, a JOIN,
an aggregate, or a set operation (§25). This lands the *parser surface* over machinery the CTE
slice already built ([cte.md](cte.md) §3): a derived table is, mechanically, an **anonymous,
always-inlined single-reference CTE** — the inline evaluation path, the synthetic-relation seam,
and column resolution are all reused unchanged.

```
SELECT d.label, d.total
FROM   (SELECT k AS label, sum(v) AS total FROM t GROUP BY k) AS d
WHERE  d.total > 100
```

**The body is planned, then run in place — INLINE.** Unlike a CTE, a derived table has no name to
reference twice, so there is no materialize path and no `cte_scan_row`: its body is planned once
(so structural/type errors surface at plan time regardless of outer cardinality, matching §26) and
**executed in place**, charging its **intrinsic cost** — exactly a single-reference / `NOT
MATERIALIZED` CTE ([cost.md](cost.md) §3). No new cost unit; no on-disk format change (a derived
table is purely a query-plan construct).

**The alias is optional (PostgreSQL 18).** The classic rule required a subquery in `FROM` to be
aliased (*"subquery in FROM must have an alias"*), but **PostgreSQL 18 relaxed it** — `SELECT id FROM
(SELECT id FROM t)` is valid — and jed matches the oracle ([../../CLAUDE.md](../../CLAUDE.md) §1).
When present, the alias is the relation's **label** (`d.col` qualifies it) and may carry a
parenthesized **column-rename list** (`AS d (a, b)`) renaming the body's output columns left-to-right,
with the **same** rules as a CTE's ([cte.md](cte.md) §1): **fewer** aliases is a partial rename (the
rest keep their body names), **more** is **`42P10`** (`invalid_column_reference`). A column list with
no preceding alias name (`(SELECT …) (a)`) is a syntax error (`42601`), matching PG. When the alias is
**absent**, the relation has **no qualifier** — its columns are reachable only by bare name — so two
unaliased derived tables coexist without collision (no `42712`), again matching PG 18. Either way, a
body producing two same-named columns is allowed, `SELECT *` emits both, and a later **bare** reference
to that name is **`42702`** (ambiguous) — the rule cte.md §2 already anticipated for "a future inline
derived table" (and the same rule a self-join of same-labelled relations takes). An explicit label
must be **distinct** from the other FROM relations' labels: a collision is **`42712`**
(`duplicate_alias`).

**Disambiguation (a §8 cross-core determinism surface).** In `table_ref` position, a leading `(`
followed by `SELECT` is a derived table — the *same* leading-`SELECT` lookahead scalar subqueries
and IN-subqueries use (§26), so the three hand-written parsers stay byte-identical. A leading `(`
followed by `VALUES` is the **VALUES-body derived table** (below). A leading `(` followed by
**neither** is a **`42601`** this slice (a parenthesized join expression `FROM (a JOIN b ON …)` is a
deferred narrowing, below). The base-table / SRF forms are unchanged: a `(` *after* a leading
identifier is still the function form (§35).

**The VALUES body — `FROM (VALUES (1),(2)) AS v(x)`.** The derived-table body may be a `VALUES` list
instead of a `query_expr`: a **computed relation of literal rows**, the FROM-position sibling of the
`INSERT … VALUES` source (§12). It reuses the *same* derived-table seam — an anonymous,
always-inlined single-reference CTE — so the alias rules, the column-rename list (`AS v(x)`), the
`42712`/`42702`/`42P10` rules, the non-correlation rule, and the intrinsic-cost charging are all the
**unchanged** derived-table behavior above; only the *body* differs.

- **Values are general constant expressions (matching PostgreSQL).** Unlike `INSERT … VALUES` (whose
  slot is a literal/`$N`/`DEFAULT`, §12/§16), a VALUES-body value is any expression — `(1+1)`,
  `(upper('a'))`, `(now())`, a cast, `CASE`, `ROW(…)`, `ARRAY[…]`, an uncorrelated scalar subquery.
  Each is resolved as a **constant**: the body is **non-`LATERAL`** (`parent = None`, like every
  derived table), so it has no FROM row and no outer row — a **column reference** inside is therefore
  unresolved (`42703`/`42P01`), an **aggregate** is `42803`, and a bind parameter with no inferable
  local type is `42P18` (jed does not infer a bare `$N` from sibling rows — the documented `$N`
  posture of §26; write `$1::int`). This is the one place a VALUES body is *richer* than
  `INSERT … VALUES`: lifting general expressions into the INSERT slot too is a separate future slice.
- **Shape and column typing.** Every row must have the **same arity** (`42601`,
  `VALUES lists must all be the same length` — the §12 rule). The relation has one column per value;
  its **default names are `column1`, `column2`, …** (PostgreSQL), overridable by the alias's
  column-rename list. Each **column's type unifies across the rows** exactly like a set operation
  (§25): `int`-widths widen, `int`+`decimal` → `decimal`, anything + `NULL` keeps the other, an
  all-`NULL` column is `text` (unknown→text); an incompatible pair is **`42804`**
  (`datatype_mismatch`). The unified column type then coerces each row's value (the only runtime
  change is `int`→`decimal`, as for a set operation).
- **No trailing `ORDER BY` / `LIMIT` on the body (a deferred narrowing).** `(VALUES … ORDER BY 1)` is
  `42601` (a leftover token). Order/limit the *outer* query instead (`FROM (VALUES …) v ORDER BY x`).
  A documented PG divergence (PG accepts a VALUES query's trailing clauses), recorded in the override
  ledger alongside the no-parenthesized-join and no-`WITH`-body narrowings.

**The body is NOT correlated (no `LATERAL`).** A derived-table body is planned as an **independent
query** (`parent = None`), exactly like a non-recursive CTE body ([cte.md](cte.md) §2): it sees its
own FROM, the catalog, and the statement's **CTE bindings** (so `WITH c AS (…) SELECT * FROM (SELECT
* FROM c) AS d` works), but **never** the enclosing query's FROM or a sibling FROM relation. A
reference to an outer or sibling column inside the body is therefore unresolved (`42703`/`42P01`),
*not* a correlated reference. This is the central behavioral rule, and it matches PostgreSQL:
without the `LATERAL` keyword a FROM-subquery cannot see the other FROM items. (`LATERAL` — the body
seeing earlier FROM relations — is a deferred follow-on.) A derived table itself nests freely: its
body may contain further derived tables, joins, and the expression-position subqueries of §26, each
counting toward the parser's `MAX_EXPR_DEPTH` nesting limit (`54001`, [cost.md](cost.md) §7) so a
pathologically nested `FROM (SELECT * FROM (SELECT …))` aborts before overflowing the native stack.

**Composition and reach.** A derived table is a first-class FROM relation: it joins and cross-joins
other relations, and `WHERE` / `GROUP BY` / `HAVING` / `ORDER BY` / `LIMIT` and the §26 subqueries
all compose over it. Because the planner path is shared, a derived table also appears inside any
**subquery**, including those in an `UPDATE`/`DELETE` `WHERE` / `SET` / `RETURNING` (which observe the
pre-statement snapshot, §26) — no extra work, since `UPDATE`/`DELETE` reach it only *through* a
subquery (they remain single-table at top level, §15). **Bind parameters** inside the body are typed
by the body's own context (§26); a `$N` with no type context errors `42P18` as everywhere.

**Errors.**

| Condition | Code | Notes |
|---|---|---|
| MORE column-rename aliases than body columns | `42P10` | `invalid_column_reference`; fewer is a legal partial rename. |
| A column-rename list with no preceding alias name (`(SELECT …) (a)`) | `42601` | The bare `(` is a leftover token (matches PG). |
| Duplicate explicit FROM label (alias collides) | `42712` | `duplicate_alias`; the general FROM-label rule (§15). Unaliased derived tables have no label and never collide. |
| Ambiguous bare reference to a duplicated body output column | `42702` | Within one relation or across two (cte.md §2). |
| Outer / sibling column referenced inside the body | `42703` / `42P01` | The body is not correlated / not LATERAL. |
| Leading `(` in FROM not followed by `SELECT`, `VALUES`, or `WITH` | `42601` | No parenthesized-join FROM this slice — a **documented divergence** (PG parses `(a JOIN b …)`; the override ledger records it). |
| Nested `WITH` inside the body | ✅ | **Landed** (cte.md §7): a derived-table body may be a `WITH`-prefixed query, establishing its own CTE scope. |
| VALUES-body rows of differing arity | `42601` | `VALUES lists must all be the same length` (the §12 rule). |
| VALUES-body columns whose row types do not unify | `42804` | `datatype_mismatch`, the set-operation unification rule (§25). |
| Column reference / aggregate / no-context `$N` in a VALUES-body value | `42703` / `42803` / `42P18` | The body is a non-`LATERAL` constant relation (no FROM/outer row). |
| Trailing `ORDER BY` / `LIMIT` on a VALUES body | `42601` | Leftover token — a deferred narrowing (PG accepts it on a VALUES query). |
| Body nesting exceeds `MAX_EXPR_DEPTH` | `54001` | The parser depth gate (cost.md §7). |

**Deliberate narrowings (each relaxable later, [../../TODO.md](../../TODO.md)).**

- **`LATERAL`** — ✅ **landed** (§44): a derived table (or SRF, §35) preceded by `LATERAL` (SRFs
  implicitly) sees earlier FROM relations. A *non-`LATERAL`* derived-table body stays independent
  (`parent = None`), the rule above.
- **Parenthesized join / table-reference FROM** — `FROM (a JOIN b ON …) c`. A leading `(` not
  starting a `SELECT` is `42601`.
- **A `WITH` *inside* the body** (nested `WITH`) — ✅ **landed** (cte.md §7): a derived-table body,
  like any parenthesized subquery, may be a `WITH`-prefixed query (`subquery_expr`), establishing its
  own CTE scope (the enclosing statement's CTEs are not inherited — a documented divergence). *(The
  `VALUES` body itself has **landed** — see "The VALUES body" above; its own residual narrowings are a
  trailing `ORDER BY`/`LIMIT` on the body and general expressions in the `INSERT … VALUES` slot.)*
- **Top-level `UPDATE`/`DELETE` FROM** stays single-table (§15) — a derived table reaches them only
  inside a subquery.

## 43. `FOREIGN KEY` constraints (`REFERENCES` / `FOREIGN KEY ( cols ) REFERENCES …`)

Both constraint positions gain the referential form ([constraints.md §6](constraints.md)). A
`column_constraint` may be a bare `references_clause` (the one-member form — the local column is the
column being defined):

```
references_clause  ::= "REFERENCES" identifier ("(" identifier ("," identifier)* ")")?
                       ("ON" ("DELETE" | "UPDATE") referential_action)*
referential_action ::= "NO" "ACTION" | "RESTRICT" | "CASCADE" | "SET" "NULL" | "SET" "DEFAULT"
```

and a `table_constraint` may be
`["CONSTRAINT" identifier] "FOREIGN" "KEY" "(" identifier ("," identifier)* ")" references_clause`
(the local-column list reuses the `PRIMARY KEY` list shape — bare names, non-empty). The
referenced-column list is optional and defaults to the parent's primary key.

**Disambiguation.** `FOREIGN`, `KEY`, `REFERENCES`, `ON`, `DELETE`, `CASCADE`, `RESTRICT`, `ACTION`,
`NO`, `SET` stay non-reserved (§3). A table element beginning with the two keywords `FOREIGN KEY`
parses as a foreign-key table constraint — a column named `foreign` would need a type named `key`
(none exists), so nothing is lost (the `PRIMARY KEY` precedent, §28). One beginning with `CONSTRAINT`
dispatches on the keyword after the name: `CHECK` (§29), `UNIQUE` (§31), or `FOREIGN`. In
column-constraint position the clause begins with `REFERENCES`; a column named `references` is
followed by a *type name* (an identifier, never another identifier-then-clause), so the order-free
constraint loop recognizes it positionally like `CHECK`. The `ON DELETE` / `ON UPDATE` actions may
appear in either order and at most once each; a repeat is `42601`.

**Where the errors fire.** The parser knows no catalog; CREATE TABLE's execution resolves the local
columns (`42703`/`42701`), looks up the parent table (`42P01`), matches the referenced columns to the
parent's PRIMARY KEY or a UNIQUE constraint (`42830` — none matching, or a referencing/referenced
count mismatch), type-checks corresponding columns (`42804`), names the constraint (`42710` against
the per-table constraint namespace), and rejects an unsupported `referential_action` (`0A000` —
CASCADE / SET NULL / SET DEFAULT) ([constraints.md §6](constraints.md)). PostgreSQL *reserves*
`FOREIGN` / `REFERENCES`, so the jed-only non-reserved surface carries oracle overrides like `on`'s
(conformance.md §5). `MATCH FULL` / `MATCH PARTIAL` are not in the grammar (MATCH SIMPLE only).

## 44. `LATERAL` joins (`… JOIN LATERAL ( query_expr ) ON …`, implicitly-lateral table functions)

A **`LATERAL`** FROM item may reference columns of the FROM relations that appear **before** it in the
same FROM clause — a *dependent* (correlated) join: the lateral item is re-evaluated **once per
combined left-hand row**, with that row bound as its immediate outer scope. It is the FROM-clause
form of the correlated subquery (§26), and it reuses the *same* machinery: the scope `parent` chain
(an earlier relation referenced from a lateral body resolves to an `Outer{level, index}`), the
per-outer-row evaluation stack, and the deterministic per-row cost charging. This is the standard
**top-N-per-group** / explode-a-row-into-many shape, e.g.

```
-- the 2 highest-value orders of each customer
SELECT c.id, o.amount
FROM   customers c
JOIN   LATERAL (SELECT amount FROM orders WHERE cust = c.id ORDER BY amount DESC LIMIT 2) o ON true
```

**What may be lateral.** `LATERAL` precedes a **derived table** (a `(SELECT …)` sub-SELECT or a
`(VALUES …)` body, §42) or a **table function** (an SRF — `generate_series`, `unnest`, §35). It may
**not** precede a bare base-table name (`FROM a CROSS JOIN LATERAL b` is `42601`, matching PG — a plain
table has nothing to correlate, so the keyword is meaningless there). Two visibility rules, both
matching PostgreSQL:

- A **sub-SELECT / VALUES derived table is lateral only with the keyword.** Without it the body is the
  independent, non-correlated relation of §42 (a sibling reference is `42P01`/`42703`). *With* it, the
  body's `parent` is the scope of the **earlier** FROM relations, so `t.col` / a bare `col` of an
  earlier relation resolves as a correlated reference.
- A **table function is *implicitly* lateral.** Its arguments may reference earlier FROM relations
  **with or without** the keyword (`FROM t CROSS JOIN generate_series(1, t.n) g` works bare — PG); the
  explicit `LATERAL` is accepted but redundant before an SRF. *(This lifts the §35 "SRF args are
  non-LATERAL" narrowing.)*

**Left-to-right, earlier-only.** A lateral body sees the relations to its **left** in FROM order, never
itself and never a **later** sibling — a forward reference is `42P01` (`FROM t CROSS JOIN LATERAL (SELECT
u.id) s, u` cannot see `u`). Lateral items **chain**: each one's correlation scope is the *cumulative*
prefix, so `t CROSS JOIN LATERAL (…) a CROSS JOIN LATERAL (SELECT a.x + 1) b` lets `b` reference the
earlier lateral relation `a`. The **first** FROM item is never lateral (nothing precedes it); a
`LATERAL` keyword on it is accepted as a no-op (`FROM LATERAL (SELECT 1) s` — PG).

**Spelled through `JOIN` (no comma-FROM).** jed reaches `LATERAL` via explicit join syntax —
`CROSS JOIN LATERAL`, `[INNER] JOIN LATERAL … ON …`, `LEFT [OUTER] JOIN LATERAL … ON …`. The
comma-FROM spelling (`FROM t, LATERAL (…)`) needs comma-`FROM`, still a deferred §15 narrowing; until
it lands, write `CROSS JOIN LATERAL` for the implicit-cross form.

**Join kinds.** `INNER` / `CROSS` / `LEFT` work as expected: `LEFT JOIN LATERAL … ON <pred>` keeps every
left row, NULL-extending it across the lateral columns when the lateral side produces no matching row
(so `LEFT JOIN LATERAL (…) ON true` keeps a left row even when the lateral body returns **zero** rows —
the common "optional explode"). A **`RIGHT`/`FULL` JOIN to a lateral item that actually references the
left side is `42P10`** (`invalid_column_reference` — *"the combining JOIN type must be INNER or LEFT for
a LATERAL reference"*, matching PG): the right side cannot both be kept whole *and* be evaluated per
left row. A lateral-eligible item with **no** actual correlation (`RIGHT JOIN generate_series(1, 2) g`,
`RIGHT JOIN LATERAL (SELECT 1) s`) is *not* dependent, so it is materialized once and **any** join kind
is allowed — again matching PG.

**Execution + cost.** A non-correlated FROM item is materialized **once** (unchanged). A
**correlated** lateral item is materialized **per combined left-hand row**: the left row is pushed onto
the outer-row stack and the body / SRF args are re-evaluated, exactly as a correlated subquery is
re-executed per outer row (§26). Cost is the sum of those per-row evaluations — fully deterministic
(it depends only on the data, not timing), so the `max_cost` ceiling bounds a runaway lateral explode
(`54P01`) and the corpus pins the accrued cost cross-core ([cost.md](cost.md) §3). No new cost unit and
**no on-disk format change** — `LATERAL` is purely a query-plan construct (like a derived table).

**Errors.**

| Condition | Code | Notes |
|---|---|---|
| `LATERAL` before a bare base-table name | `42601` | The keyword is valid only before a derived table or table function (PG). |
| Earlier-relation reference inside a **non-`LATERAL`** sub-SELECT / VALUES body | `42P01` / `42703` | The §42 non-correlation rule — mark the body `LATERAL` to correlate it. |
| **Forward** / self reference (a later sibling) inside a lateral body | `42P01` | Only relations to the **left** are in scope. |
| `RIGHT` / `FULL JOIN` to a lateral item that references the left side | `42P10` | `invalid_column_reference`; the combining join type must be INNER or LEFT for a LATERAL reference (PG). |
| A lateral SRF / body whose own argument / clause errors (arity, type, `54001` depth, …) | *(as §35/§42)* | Lateral changes only the *scope* the body resolves against, not its other rules. |

**Deliberate narrowings (relaxable later, [../../TODO.md](../../TODO.md)).**

- **`RIGHT`/`FULL JOIN LATERAL` rejection is slightly broader than PG.** jed raises `42P10` when the
  lateral item references **any** outer relation (a left sibling *or* an enclosing query); PG rejects
  only a reference to the **left side of that join**. The practical case — `RIGHT JOIN LATERAL` that
  correlates to the left — matches PG exactly; the divergence is the exotic `RIGHT JOIN LATERAL (body
  that references only an *enclosing* query)`, recorded in the override ledger. A `RIGHT`/`FULL JOIN` to
  a fully **un**correlated lateral item is allowed (matches PG).
- **comma-`FROM` `LATERAL`** (`FROM t, LATERAL (…)`) waits on comma-`FROM` (§15).

## 45. `CREATE SEQUENCE` / `DROP SEQUENCE` ([sequences.md](sequences.md))

```
create_sequence ::= "CREATE" "SEQUENCE" ("IF" "NOT" "EXISTS")? identifier sequence_option*
sequence_option ::= "INCREMENT" "BY"? signed_integer
                  | "MINVALUE" signed_integer | "NO" "MINVALUE"
                  | "MAXVALUE" signed_integer | "NO" "MAXVALUE"
                  | "START" "WITH"? signed_integer
                  | "CACHE" signed_integer
                  | "CYCLE" | "NO" "CYCLE"
drop_sequence   ::= "DROP" "SEQUENCE" ("IF" "EXISTS")? identifier ("," identifier)* "RESTRICT"?
alter_sequence  ::= "ALTER" "SEQUENCE" ("IF" "EXISTS")? identifier
                    ( "RENAME" "TO" identifier
                    | alter_sequence_option+ )
alter_sequence_option ::= sequence_option | "RESTART" ("WITH"? signed_integer)?
```

(`sequence_option` above also includes `AS type` for `CREATE`; on `ALTER` the same loop accepts it
syntactically but execution rejects `AS` as `0A000` — §15.)

The options are **order-free** and each appears at most once (a repeat is `42601`), like the FK
actions (§43) — the parser loops, dispatching on the leading keyword. `INCREMENT`/`START` accept an
optional `BY`/`WITH`. An option value is a `signed_integer` (an optional leading `-` then an integer
literal — `START WITH -1`, `INCREMENT BY -2`); it spans the full i64 range, so a value out of range
is `22003` at parse, and `INCREMENT 0` / `CACHE < 1` / an inconsistent `START`/`MIN`/`MAX` is `22023`
at execution. `SEQUENCE`, `INCREMENT`, `MINVALUE`, `MAXVALUE`, `START`, `CACHE`, `CYCLE`, `BY`, `WITH`
stay **non-reserved** (§3): `CREATE SEQUENCE` / `DROP SEQUENCE` is recognized by the two leading
keywords, and the option keywords are matched positionally inside the loop, so an unrelated identifier
use is unaffected. The parser knows no catalog; execution resolves the name against the shared relation
namespace (`42P07` duplicate unless `IF NOT EXISTS`) and (`DROP`) raises `42P01` on a missing sequence
unless `IF EXISTS`. PostgreSQL *reserves* none of these either, so no oracle-override surface beyond the
sequence behavior itself. `nextval('s')` / `currval('s')` / `setval('s', n[, b])` / `lastval()` are
ordinary `function_call`s (§ no new production) — the first three resolve their `text` argument to a
sequence at evaluation (`42P01` if missing), `lastval()` reads per-session state.

`ALTER` is **not otherwise a statement keyword**, so the dispatcher recognizes it solely as
`ALTER SEQUENCE` (a two-keyword lookahead, the `CREATE SEQUENCE` precedent; `ALTER`/`RESTART`/`RENAME`
stay non-reserved). After the name the parser dispatches on the next keyword: `RENAME` → the rename
form; `OWNED` / `OWNER` / `SET` → `0A000`; otherwise the **option loop** — the same order-free
`sequence_option` loop `CREATE` uses, extended with an interleavable `RESTART [[WITH] n]`, requiring
**≥ 1** option (a bare `ALTER SEQUENCE s` is `42601`, matching PG). `RESTART` resets the counter so the
next `nextval` returns `n` (`RESTART WITH n`) or the stored `START` (bare `RESTART`), clearing
`is_called`; the stored `START` is unchanged. On `ALTER` only the **written** options change and
`last_value`/`is_called` are preserved unless `RESTART` is given (PG `init_params`, `isInit = false`);
the post-edit cross-checks are `START ∈ [MIN, MAX]` then the preserved `last_value ∈ [MIN, MAX]` (each
`22023`; NB — `setval`'s out-of-bounds error is `22003`, the two PG paths differ), and `MINVALUE <
MAXVALUE` is strict. `RENAME TO new_name` moves the catalog key (`42P07` if `new_name` already names a
relation, the same name included) and rewrites an **owned** sequence's owning-column `nextval` default
(§15.3). A missing sequence is `42P01` unless `IF EXISTS` (then a no-op). `ALTER … AS type` (the type is
not persisted — sequences.md §14.4), `OWNED BY`, `OWNER TO`, and `SET { SCHEMA | LOGGED | UNLOGGED }`
stay `0A000`.

## 46. `INSERT ... ON CONFLICT` (UPSERT — [upsert.md](upsert.md))

`INSERT` takes an optional **`ON CONFLICT`** clause **between the source and `RETURNING`**
(`on_conflict` in the grammar): a candidate row that would violate a `UNIQUE`/`PRIMARY KEY`
constraint takes a **conflict action** instead of trapping `23505`.

```
ON CONFLICT [ ( col [, …] ) | ON CONSTRAINT name ] { DO NOTHING | DO UPDATE SET … [WHERE …] }
```

- **`DO NOTHING`** skips the offending row; **`DO UPDATE SET … [WHERE …]`** updates the
  existing conflicting row. In `DO UPDATE`, the **`excluded`** pseudo-relation names the row
  *proposed for insertion* (`excluded.col`), while a **bare or table-qualified** column names
  the **existing** row — the `SET`/`WHERE` reuse the `UPDATE` `assignment`/`where_clause`
  productions, and `excluded` is a **qualifier-only** pseudo-relation exactly like `old`/`new`
  in `RETURNING` (§32).
- The optional **conflict target** names the **arbiter** constraint. A **column list** is
  matched as an order-independent **set** against a unique index / the primary key
  (`ON CONFLICT (b, a)` matches `UNIQUE (a, b)`); no match is **`42P10`**. **`ON CONSTRAINT
  name`** names a unique index, or the synthesized **`<table>_pkey`** for the primary key; a
  miss is **`42704`**. An unknown column in the list is **`42703`**.
- **`DO UPDATE` requires a target** (**`42601`** without one); **`DO NOTHING`** may omit it,
  in which case **any** uniqueness conflict is skipped. The arbiter **only** arbitrates its
  own constraint — a conflict on a **different** unique/PK constraint still traps **`23505`**.
- Two **proposed** rows sharing the arbiter key are **`21000`** under `DO UPDATE` (*cannot
  affect row a second time*) and **skipped** under `DO NOTHING`.

The whole feature is two-phase / all-or-nothing with **sequential planning** so a later
proposed row observes earlier ones (PostgreSQL's row-at-a-time visibility, reproduced over
the two-phase model). `RETURNING` projects the **affected** rows (inserted + updated);
`DO NOTHING`-skipped and `WHERE`-false rows contribute nothing. `ON`/`CONFLICT`/`DO`/
`NOTHING`/`EXCLUDED` stay non-reserved (§3), recognized positionally; the clause is
disambiguated from an `INSERT ... SELECT` source by appearing after the complete source.
Full behavior, the arbiter model, the `21000` rule, cost, and the PG divergences are in
[upsert.md](upsert.md).

## 47. `COLLATE` and `ORDER BY … COLLATE` ([collation.md](collation.md))

`expr COLLATE "name"` is PostgreSQL's postfix collation operator: it yields the same value with an
**explicit** collation for the surrounding comparison / sort ([collation.md](collation.md) §1). The
named collation must be **vendored** into the binary ([collation.md](collation.md) §2/§9; the corpus
`# load-collation:` directive declares the dependency); `"C"` is the built-in byte / code-point order
and is always available.

**Precedence — the postfix / typecast rung.** COLLATE binds at the **same level as `::` / `[]` /
`.field`** ([grammar.ebnf](../grammar/grammar.ebnf) `postfix`), so it is **tighter than `||` and the
comparison operators** (PostgreSQL): `a || b COLLATE "x"` is `a || (b COLLATE "x")`, and
`'a' < 'b' COLLATE "x"` is `'a' < ('b' COLLATE "x")`. It chains left-to-right with the other
postfix operators (`a :: text COLLATE "x"` is `(a :: text) COLLATE "x"`). The parsers parse it in
the same `parse_postfix` loop as `::`.

**The name is a double-quoted identifier.** Collation names contain hyphens and are case-sensitive
(`"en-US"`, `"C"`), so the lexer gained a `quoted_identifier` token (`Token::QuotedIdent`,
case-sensitive, `""` an embedded quote — [grammar.ebnf](../grammar/grammar.ebnf) `quoted_identifier`).
Today it is consumed **only** after `COLLATE` (in an expression or an `ORDER BY` key); a quoted
identifier anywhere else is a `42601` syntax error.

**`ORDER BY col COLLATE "name"`.** A sort key may carry an explicit COLLATE between the column and
the `ASC`/`DESC` direction ([grammar.ebnf](../grammar/grammar.ebnf) `sort_key`). The key is then
ordered by that collation's UCA sort key; **absent the clause, the key inherits the column's frozen
collation** (slice 1d — so `ORDER BY name` over an `en-US` column sorts by en-US).

**`COLLATE "name"` as a column constraint (slice 1d).** `CREATE TABLE` accepts a `COLLATE "name"`
column modifier (`name text COLLATE "en-US"`, [grammar.ebnf](../grammar/grammar.ebnf)
`column_constraint`) — text-only (else `42804`), the name loaded or `"C"` (else `42704`). The
**effective** collation is frozen into the column at create (an explicit clause, else the per-database
default as of creation, [collation.md](collation.md) §1/§5) and becomes the column's **implicit**
collation in the derivation below.

**Resolution / errors** ([collation.md](collation.md) §1/§7): applying COLLATE to a non-text
expression/column is `42804` (`datatype_mismatch`); naming an unloaded collation is `42704`
(`undefined_object`); combining two **different explicit** collations in one comparison is `42P21`
(`collation_mismatch`); combining two **different implicit** collations (two columns with different
collations — `C` counts as a distinct one) with no explicit COLLATE is `42P22`
(`indeterminate_collation`, slice 1d), resolved by adding an explicit COLLATE. The conflict is
derived for **all** comparison ops including `=`/`<>` (PG raises it regardless). A non-`C` collation
orders only the **ordering** comparisons (`< <= > >=`) and `ORDER BY` by its sort key; `=`/`<>` stay
byte-equality (deterministic-collation equality is byte-identity — [collation.md](collation.md) §7).

## 48. Data-modifying (writable) CTEs ([writable-cte.md](writable-cte.md))

A `WITH` item's body, and the `WITH`-prefixed primary statement, may be an `INSERT` / `UPDATE` /
`DELETE` ([grammar.ebnf](../grammar/grammar.ebnf) `cte_body`) — PostgreSQL's *writable CTE*. A
data-modifying CTE feeds its `RETURNING` rows forward; the move-rows idiom is the canonical shape:

```sql
WITH moved AS (DELETE FROM inbox WHERE ready RETURNING *)
INSERT INTO archive SELECT * FROM moved;
```

**Parsing.** `parse_cte`, after `AS [ [NOT] MATERIALIZED ] (`, peeks the body's leading keyword:
`insert`/`update`/`delete` parse the data-modifying statement, anything else a `subquery_expr` — a
`query_expr` that may itself be `WITH`-prefixed (a nested `WITH`, cte.md §7). `parse_with_statement`'s
top-level primary peeks the same way after the CTE list, but takes the plain `query_expr` (a
re-prefixed top-level `WITH` is a `42601`). A `cte_body` is one parser nesting level (the `deepen`/`undeepen` guard, like a subquery), so
deeply-nested writable CTEs still hit `54001`. Both the body's and the primary's `RETURNING` are the
ordinary `returning_clause` (§32).

**Semantics** (the full record is [writable-cte.md](writable-cte.md)): every sub-statement reads
**one pre-statement snapshot** (a read pin — they cannot see each other's table writes; data crosses
only via a CTE's `RETURNING` buffer), the parts run in **lexical order** (data-modifying CTEs first,
each always to completion and materialized, then the primary), and the whole statement is **one
all-or-nothing transaction**. The statement result is the **primary's** (a query result, an
affected-row count, or the primary's `RETURNING` rows). A data-modifying CTE without `RETURNING`
runs for its effect but a `FROM` reference to it is `0A000`; a data-modifying target resolves against
the **catalog** (a CTE name as target with no base table is `42P01`); an insert/insert key clash
between two parts is `23505`, while an update/update or update/delete of the same row is jed's
deterministic last-write-wins (a documented divergence on a case PostgreSQL leaves unspecified —
[writable-cte.md](writable-cte.md) §7). `WITH RECURSIVE` with a (non-self-referencing) data-modifying
CTE is allowed (a data-modifying body is never the recursive `UNION` shape). No on-disk format change.
