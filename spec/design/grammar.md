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

## 4. Lexical edges: the leading-minus literal and two-character operators

Two lexer facts are easy to get subtly wrong across cores, so the grammar pins them:

- **A negative integer is one lexical token, not a unary operator.** The leading `-` is
  bound to the digits at lex time *only when a digit immediately follows*; `- 5` with a
  space is a lex error, and there is no general unary-minus expression yet. Out-of-range
  magnitudes (beyond signed 64-bit) are a syntax error (`42601`), not a silent wrap.
- **`<=` and `>=` are single tokens**, lexed greedily. The only comparison operators are
  `=`, `<`, `>`, `<=`, `>=`; **`<>` and `!=` do not exist** in this surface.

## 5. Deliberate narrowings (each relaxable later)

The current surface is intentionally minimal. Every omission below is a future feature,
tracked in [../../TODO.md](../../TODO.md), not an oversight:

- **Single WHERE predicate** — exactly one `IS [NOT] NULL` test or one comparison. No
  `AND` / `OR` / `NOT`, and no parentheses around predicates.
- **No aliases** — neither column (`expr AS name`) nor table aliases. The only `AS` in
  the surface is inside `CAST(expr AS type)`.
- **Single table** — one table per `SELECT`/`UPDATE`/`DELETE`; no `JOIN`, no subqueries.
- **Positional `INSERT`** — no column list, no multi-row `VALUES`, no `DEFAULT`.
- **One `ORDER BY` key**, optional `ASC` / `DESC`. There is no `NULLS FIRST` / `NULLS
  LAST` *syntax*; NULL ordering is fixed semantics (NULLs first ascending), not a knob.
- **No `LIMIT` / `OFFSET`.**
- **ASCII-only identifiers**, no quoted identifiers (§3).
- **Two literal forms only** — integer and `NULL`. No string, decimal, or boolean
  literals (the scalar set is integers-only this slice — see [types.md](types.md) §1).
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
(CLAUDE.md §10/§11). [../../TODO.md](../../TODO.md) is the roadmap of what comes next —
compound predicates (`AND`/`OR`/`NOT`), integer arithmetic operators, a general
expression evaluator, and onward. Because the parser is hand-written rather than
generated, "conform to the grammar" is verified by cross-reading each production against
the three parsers and confirming every corpus statement is derivable from the grammar,
not by a generator step.
