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

## 5. Deliberate narrowings (each relaxable later)

The current surface is intentionally minimal. Every omission below is a future feature,
tracked in [../../TODO.md](../../TODO.md), not an oversight:

- **Column aliases via explicit `AS` only** (`expr AS name`); see §8 for the output-name
  rule. **Table aliases** and **implicit** aliases (`expr name`, no `AS`) remain deferred,
  and `AS` aliasing in `ORDER BY` is not yet visible (ORDER BY still resolves a bare table
  column). Before this slice the only `AS` in the surface was inside `CAST(expr AS type)`.
- **Single table** — one table per `SELECT`/`UPDATE`/`DELETE`; no `JOIN`, no subqueries.
- **Positional `INSERT`** — no column list, no multi-row `VALUES`, no `DEFAULT`, and the
  values are *literals only* (not general expressions; see the `literal` production).
- **One `ORDER BY` key**, optional `ASC` / `DESC`, over a bare column (not a general
  expression). There is no `NULLS FIRST` / `NULLS LAST` *syntax*; NULL ordering is fixed
  semantics (NULLs first ascending), not a knob.
- **No `LIMIT` / `OFFSET`.**
- **ASCII-only identifiers**, no quoted identifiers (§3).
- **No string or decimal literals.** Integer, `TRUE`/`FALSE`, and `NULL` are the literal
  forms. `boolean` exists only as an *expression* type this slice — there are boolean
  literals and comparison/logical results, but no boolean *column* (see
  [types.md](types.md) §1).
- **No function calls.** The expression grammar has operators and parentheses but no
  `f(args)` call syntax — no scalar functions are defined yet.
- **No `;` statement terminator** and **no SQL comment syntax** in the input.
- **No parameter placeholders** (`$1`, `?`). The conformance corpus uses literal SQL by
  design — see [conformance.md](conformance.md); bound parameters are an
  implementation-API concern, not part of the parsed grammar.

## 6. Type names stay as `identifier`

The grammar parses a column's and a `CAST`'s type as a bare `identifier` rather than
enumerating `int16 | smallint | …` as a production. The catalog owns the type lattice
and resolves the name case-insensitively, rejecting unknowns at execution time
(`42704`). Keeping resolution out of the grammar means the scalar set can grow
([types.md](types.md)) without touching the grammar, and a misspelled type yields a
clean structured error instead of a parse failure. The accepted names are listed as an
informative comment beside the `type_name` rule.

## 7. Growth rule

The grammar grows **one production at a time, spec-first**. When a feature lands it
edits this grammar and [grammar.ebnf](../grammar/grammar.ebnf) in the *same change* that
adds the parser code in all cores and the conformance entries that exercise it
(CLAUDE.md §10/§11). The general expression substrate — operator precedence,
parenthesization, integer arithmetic, the `boolean` type, and the `AND`/`OR`/`NOT`
connectives — landed together as the `expr` tower above; [../../TODO.md](../../TODO.md)
is the roadmap of what comes next (`LIMIT`/`OFFSET`, richer `ORDER BY`, more predicate
forms, and onward). Because the parser is hand-written rather than
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
4. **Any other un-aliased expression** (arithmetic, comparison, `CAST`, a literal, `IS NULL`,
   a unary/logical expression, …) → the fixed literal **`?column?`**.

Case 4 is deliberately a constant placeholder rather than a re-rendering of the expression
text. Echoing normalized SQL text (the SQLite behaviour) would require a canonical
expression printer that is byte-identical across Rust, Go, and TS — a new §8 divergence
hotspot for no present benefit. A column whose name matters can be given one with `AS`. A
normalized-name printer remains a possible later refinement.
