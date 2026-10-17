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
grouping, `"..."` terminals, `[a-z]` character classes, `A - B` exclusion, slash-star
comments) rather than ISO/IEC 14977. The W3C form reads closer to the railroad-style grammars common to SQL
references: optional/repeat postfix operators and juxtaposition are quieter on the page
than 14977's comma-concatenation, `{ ... }` repetition, and `;` rule terminators. The
notation legend is duplicated at the top of
[grammar.ebnf](../grammar/grammar.ebnf) so that file is self-contained — a reader never
needs this doc to *read* the grammar, only to understand *why* it is shaped as it is.

## 3. Identifiers: unquoted names fold to lowercase, quoted names are exact; keywords are not reserved

**Two identifier spellings, one name.** An identifier is written either **bare** or
**double-quoted**, and both produce a *name* — a byte string — which is all the rest of the
engine ever sees. This is PostgreSQL's rule (CLAUDE.md §1), adopted exactly:

- A **bare identifier** (`[A-Za-z_][A-Za-z0-9_]*`, ASCII only) **folds to lowercase** in the
  lexer: `Orders`, `ORDERS`, and `orders` are all the name `orders`. Folding is ASCII-only
  (there are no non-ASCII bare identifiers to fold).
- A **quoted identifier** (`"…"`, [grammar.ebnf](../grammar/grammar.ebnf) `quoted_identifier`) is
  the name **verbatim** — case kept, any UTF-8 character allowed (spaces, hyphens, `.`, non-ASCII),
  an embedded `"` written by doubling it (`"a""b"` is the name `a"b`). `"userId"`, `"order"`, and
  `"my table"` are names. A **zero-length** quoted identifier `""` is **`42601`** (PostgreSQL's
  *"zero-length delimited identifier"*).
- A **Unicode-escape identifier** (`U&"…"`) is a quoted identifier whose body is decoded after
  scanning — see *Unicode escapes* below.

**Unicode escapes (`U&'…'`, `U&"…"`).** PostgreSQL's Unicode-escape string constants and
identifiers, adopted exactly (`scan.l` + `parser.c` `base_yylex` / `str_udeescape`). `U&` (either
case, immediately followed by the quote) opens a body scanned exactly like the plain form (`''` /
`""` doubled), then decoded:

- `\XXXX` (4 hex digits) and `\+XXXXXX` (6 hex digits) are the code point's UTF-8; `\\` is a literal
  backslash. Hex digits are case-insensitive; any other character is copied verbatim.
- A UTF-16 **surrogate pair** written as two escapes (in either form, `\D83D\DE00`) combines into one
  code point. A lone low surrogate, a high surrogate not immediately followed by a low-surrogate
  escape (an ordinary character, a doubled escape, a non-surrogate escape, or the end of the body),
  are `42601` *"invalid Unicode surrogate pair"*.
- Any other use of the escape character is `42601` *"invalid Unicode escape"* with PostgreSQL's
  Hint (*"Unicode escapes must be \XXXX or \+XXXXXX."*, [error-fields.md](error-fields.md) §11.5);
  the code point 0 or one past U+10FFFF is `42601` *"invalid Unicode escape value"*.
- **`UESCAPE '<c>'`** immediately after the token replaces the backslash with `<c>`, consumed with
  the token. `<c>` must be a plain string literal of exactly one byte that is not a hex digit, `+`,
  `'`, `"`, or whitespace (`42601` *"invalid Unicode escape character"*); anything else after
  `UESCAPE` — a word, a `U&'…'` string, end of input — is `42601` *"UESCAPE must be followed by a
  simple string literal"*. `UESCAPE` is recognized only there; anywhere else it is an ordinary word
  (`SELECT 'x' AS uescape`).
- The decoded **string** is an ordinary text literal wherever one is accepted (`DATE U&'…'`, a
  `LIKE` pattern, a function argument). The decoded **identifier** is exact — never folded, never a
  keyword — like any quoted identifier; `U&""` is the zero-length error (checked before decoding),
  and the 63-byte limit is measured on the decoded name.
- jed has no `standard_conforming_strings = off` mode, so PostgreSQL's `0A000` *"unsafe use of
  string constant with Unicode escapes"* cannot arise.
- The lexer decodes the token, so nothing downstream sees the escape spelling: stored canonical text
  (a view definition, a CHECK or DEFAULT expression) renders the value as a plain `'…'` literal or
  `"…"` identifier, which re-lexes to the same token.
- **Error positions** ([error-fields.md](error-fields.md) §10) are PostgreSQL's. A `UESCAPE` error
  points at the token after `UESCAPE` (end of input when there is none). A decoding error points at
  the token start + 3 + the **byte** offset of the escape in the body *after* `''` / `""`
  collapsing, converted to characters by PostgreSQL's count — so each doubled quote before the
  escape moves the cursor one character left, and a dangling high surrogate's error points just
  past it. When that byte offset falls inside a multibyte character, PostgreSQL fails to render its
  cursor and reports `22021`; jed reports the `42601` at the next character (the one recorded
  divergence, unreachable without a doubled quote before a non-ASCII character).

**Names compare by exact bytes, everywhere.** After the fold, every name comparison and every
catalog key is **byte-exact** — tables (persistent, `TEMP`, attached), columns, indexes,
sequences, composite types and their fields, constraints, databases (attachment names), CTE and
relation aliases, window names, select-list output names (the `ORDER BY` output-name match,
§10), function names, and type names. So `"Orders"` and `orders` are **different** names (both
may exist in one table namespace), `CREATE TABLE Orders` then `SELECT * FROM "Orders"` is `42P01`
(the table is `orders`), and `"userId"` must be written quoted at every reference. Built-in
functions and types are named in lowercase, so `LOWER(x)`, `I32`, and `"lower"(x)` resolve while
`"LOWER"(x)` is `42883` and `CAST(x AS "I32")` is `42704`, exactly as PostgreSQL. Collation names
were already quoted and exact (§47) and are unchanged. Every canonical **name order** the spec
defines (catalog emission order, constraint evaluation order, access-path tie order, FK visitation
order, …) is ascending **byte order of the name**; earlier spec text that says "lowercased name"
or "case-insensitively" about a catalog name means the name itself — for every name written bare
the two are the same thing, because the bare spelling is already lowercase.

**Names supplied through the host API are exact.** A name passed to the host/embedding API as a
string — an attachment name, `compact(name)`, a privilege grant's table name, a host-registered
function or type name — is taken **verbatim** as a catalog name: it is not folded and not parsed
for quotes (the host API speaks names, not SQL text). `attach("Reports", …)` is therefore reached
from SQL as `"Reports".t`, and `attach("reports", …)` as `reports.t` or `Reports.t`.

**Name-valued text arguments parse as an identifier.** The sequence functions' name argument
(`nextval('s')`, `currval`, `setval` — the PostgreSQL `regclass` text form, [sequences.md](sequences.md)
§4) is parsed the way PostgreSQL parses a `regclass` string: surrounding whitespace is ignored; a
bare word folds to lowercase; a double-quoted name is exact (with `""` an embedded quote). So
`nextval('S')` advances `s`, and `nextval('"S"')` advances `S`. Any other text — empty or
all-whitespace, an unterminated quote, internal whitespace in a bare word (`'my seq'`), trailing
characters after the name — is **`42602`** (`invalid_name`, *"invalid name syntax"*), as in
PostgreSQL. `'""'` parses (to the empty name, which never exists: `42P01`, as PostgreSQL). A dotted
name (`'a.b'`) is also `42602`: jed has no schemas and does not resolve a database-qualified
sequence name from text (PostgreSQL would look up schema `a` — a documented divergence). An engine-generated reference to a name
that would not survive this parse bare — a `serial` column's `DEFAULT nextval('…')` naming a
sequence derived from a quoted table — is written **quoted** (`nextval('"Foo_id_seq"')`, as
PostgreSQL does). The quoting rule for a name inside such a text literal: a name matching
`[a-z_][a-z0-9_]*` is written bare, any other name double-quoted with `"` doubled. A name the
engine substitutes into persisted **expression** text (a `RENAME` rewrite of CHECK / DEFAULT /
index-expression / partial-predicate text, [alter.md](alter.md) §2.2) is **always** written
double-quoted: jed has no reserved-keyword table, so a bare `null` or `true` there would re-lex as
the keyword, not the renamed column.

**Keywords are not reserved.** The lexer has **no reserved-keyword table**: it emits one bare
identifier token for every bare word, and the parser recognises keywords purely by grammatical
position (since the word is already folded, `SELECT` = `select` = `SeLeCt`). Two consequences
the grammar encodes:

- A keyword spelling is a **legal identifier** wherever the grammar expects one — e.g. a
  column may be named `select`, bare or quoted.
- A keyword terminal in the grammar (`"SELECT"`, `"FROM"`, …) matches **only a bare word**,
  never a quoted identifier: `SELECT "from" FROM t` selects the column `from`, and `"null"` is a
  column reference, not the null literal. Keyword-like words in fixed vocabularies — `EXPLAIN`
  options, frame bounds, multi-word type names (`double precision`,
  `timestamp with time zone`) — are keywords in this sense and accept only bare words. An
  `EXTRACT` field is not: as in PostgreSQL it is an identifier, bare or quoted, or a string (§50).
  Punctuation terminals (`"("`, `"="`) match literally.

**Output names fold too.** A select item's `AS alias` is an identifier like any other, so
`SELECT 1 AS Total` names its column `total` and `SELECT 1 AS "Total"` names it `Total` (§8),
matching PostgreSQL.

**The `jed_` reservation** is checked on the **name**, byte-exact: a name beginning with the four
lowercase bytes `jed_` is reserved ([introspection.md](introspection.md) §4). `CREATE TABLE
JED_x` folds to `jed_x` and is rejected; `CREATE TABLE "JED_x"` is the name `JED_x`, which does
not begin with `jed_`, and is allowed — PostgreSQL's exact rule for its `pg_` prefix.

A qualified reference's structural `identifier "." identifier` shape takes precedence over
every expression-keyword interpretation of its first word. Thus relation aliases such as
`null`, `cast`, `case`, `not`, and `current_date` remain usable as `null.v`, `cast.v`, and so
on; the leading word is not parsed as a literal, conditional, unary operator, or clock form.
This priority follows directly from accepting every bare word wherever `identifier` appears.

This is a CLAUDE.md §8 divergence hotspot: if one core folded case differently, or
reserved a word another did not, the corpus would diverge. Recording the rule in the
grammar keeps all cores honest. (Canonical *output* names — `i16` not `smallint` — are
a separate determinism rule owned by the type system, see [types.md](types.md) §2.)

### 3.1 Engine auto-names fit the identifier limit

A name the engine derives for an unnamed object — a constraint (`<table>_pkey`,
`<table>_<cols>_key`, `…_fkey`, `…_check`, `…_excl`), an index (`<table>_<parts>_idx`), or a
`serial` / identity column's owned sequence (`<table>_<column>_seq`) — always fits the 63-byte
identifier limit ([cost.md](cost.md) §7a), so it can be written in SQL (`DROP INDEX`, `ALTER TABLE
… DROP CONSTRAINT`, `nextval('…')`, `ON CONFLICT ON CONSTRAINT`). The algorithm is PostgreSQL's,
byte for byte (`makeObjectName` / `ChooseRelationName` / `ChooseConstraintName` /
`ChooseIndexNameAddition` / `ChooseIndexColumnNames`, `indexcmds.c` + `pg_constraint.c`):

- **Parts.** `name1` is the exact table name; `name2` is the column part — one column name (a
  single-column CHECK, a `serial`), the column names joined by `_` (UNIQUE / FK / EXCLUDE members,
  index parts; the join stops once it reaches 64 bytes), or absent (`pkey`, a CHECK naming zero or
  several columns). The `label` is `pkey` / `key` / `fkey` / `check` / `excl` / `idx` / `seq`.
- **Cut.** With `overhead` = the label's bytes + one `_` per separator, the parts may use
  `63 − overhead` bytes. While they exceed it, the **longer** of the two loses one byte (`name2`
  when they are equal). Each part is then clipped back to a UTF-8 character boundary (PostgreSQL's
  `pg_mbcliplen`), so a cut never splits a character, and the name is
  `name1[_name2]_label`. The label is never cut.
- **Collision.** If the name is taken in its namespace (the object kind's existing rules —
  constraints.md §4.3/§5.3/§6.2, indexes.md §2, sequences.md §12; a constraint's namespace is every
  constraint name of its schema, constraints.md §4.3), the label becomes `label1`,
  `label2`, … and the name is rebuilt from the same parts, so a longer label shortens the parts
  (`…_abcde_check`, then `…_abcd_check1`). The derived PK name is not persisted and takes no suffix
  (constraints.md §5.4).
- **Index parts are de-duplicated first** (`ChooseIndexColumnNames`): a key part equal to an
  earlier one becomes `part1`, `part2`, … (the part clipped so the suffixed form fits 63 bytes),
  so `CREATE INDEX ON t (a, a)` is `t_a_a1_idx` and `(lower(x), lower(x))` is
  `t_lower_lower1_idx`.

A name that fits is unchanged — `t_a_key` stays `t_a_key`. Every auto-name is persisted as chosen,
so this changes only what newly created objects are called; existing databases keep their stored
names (no format change).

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
- **PostgreSQL 16+'s number forms, with PostgreSQL's scanner rules.** An integer literal may be
  written with a **radix prefix** — `0x`/`0X` hexadecimal, `0o`/`0O` octal, `0b`/`0B` binary
  (`0x1F` = `31`) — and every digit run (a decimal integer, a fraction, an exponent, a radix
  integer's digits) may carry single **`_` separators** between digits (`1_000`, `1_000.000_1`,
  `1e1_0`, `0xFF_FF`; one may follow a radix prefix, `0x_ff`). A radix literal is the same untyped
  integer constant as the equal decimal literal, with the same `2^63` magnitude bound; it never
  takes a fraction or exponent (`0x1.5` is `0x1` then `.5`, a parse error; `0x1e` is `30`). The
  lexer scans a number the way PostgreSQL's `scan.l` does: the longest of the number patterns, and
  a number **glued to an identifier character** — a letter, `_`, or any non-ASCII character
  starting a word that would continue it (`123abc`, `1_`, `1__0`, `0x1g`, `1e`, `1.5x`, `1.5e1_`)
  — is `42601` "trailing junk after numeric literal", as is an exponent sign with no digit
  (`1e+`). A radix prefix with no digit (`0x`, `0x_`, `0o`, `0b`) is `42601` "invalid hexadecimal
  / octal / binary integer". `1..2` lexes `1` then `..` (a parse error). So a number is always
  separated from a following word by whitespace or punctuation: `SELECT 1x` is an error, never
  `1 AS x`.
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

- **Column aliases** are `expr AS name` or a bare label `expr name` (PostgreSQL's
  bare-label rule, with a fixed no-bare-label word set); see §8 for both and the output-name
  rule. A qualified star `t.*` takes neither (§15).
- **Single-table `UPDATE` / `DELETE`** — those two still take one table (no `JOIN`, no `USING`).
  `SELECT` is now **multi-table** via `JOIN` (§15): `INNER JOIN ... ON`, `CROSS JOIN`, the
  `LEFT`/`RIGHT`/`FULL [OUTER] JOIN` family, comma-`FROM`, `JOIN ... USING (cols)`, `NATURAL JOIN`,
  and parenthesized joins `FROM (a JOIN b ON …)` all execute (subqueries / derived tables / `IN` /
  `EXISTS` / correlated landed too). Only **`FULL JOIN USING`** / `NATURAL FULL JOIN` (the
  `COALESCE` merge) remains a deferred join form.
- **`INSERT` values** were originally *literals only*; the `DEFAULT` keyword, an explicit
  **column list** (`INSERT INTO t (a, c) VALUES ...`, §12, §16), `INSERT ... SELECT` (§24), and
  finally **general expressions in a `VALUES` value slot** (arithmetic, function calls, `CASE`,
  casts, scalar subqueries — §16) have all landed.
- **`ORDER BY` keys are a column or an output-column ordinal** — a sort key is a table column
  or an **ordinal** (`ORDER BY 1`, the 1-based position of a select-list item), never a general
  expression (`ORDER BY a + 1`) or an output alias; those stay deferred. The richer surface that
  *did* land — multiple keys, per-key `ASC` / `DESC`, per-key `NULLS FIRST | LAST`, and the
  ordinal form — is described in §10.
- **`LIMIT` / `OFFSET` take a non-negative integer literal**, not a general expression. The two clauses may appear in either
  order, each at most once (§9). There is **no `LIMIT ALL`**, **no `OFFSET … ROWS` /
  `FETCH NEXT … ROWS ONLY`**, and **no SQLite `LIMIT off, cnt` comma form**.
- **ASCII-only bare identifiers**; a non-ASCII name is written quoted (`"café"`, or with Unicode
  escapes `U&"caf\00e9"`, §3). A bare or quoted identifier longer than **63 bytes** (a quoted
  one measured on its decoded UTF-8 name) is **`42622`** `name_too_long`, where PostgreSQL silently
  truncates to `NAMEDATALEN − 1` ([cost.md](cost.md) §7, the identifier-length limit).
- **Literal forms.** Integer (decimal or `0x`/`0o`/`0b` radix, with `_` separators — §4),
  **decimal** (`1.50`, `.5`, and scientific `e`-notation `1.5e3` / `5e2` / `1e-3` — §14), **single-quoted string** (the `text` type, `'alice'`, with
  `''` for an embedded quote, or the Unicode-escape form `U&'…'`, §3), `TRUE`/`FALSE`, and `NULL`. `boolean` is now a storable column
  type as well as an expression type (boolean literals, comparison/logical results, and stored
  boolean columns — see [types.md](types.md) §9).
- **Function calls.** The expression grammar has a `function_call` production with
  PostgreSQL named-notation (`name(arg, name => expr)`) and `DEFAULT` arguments (§17). It
  resolves the five aggregates (`COUNT`/`SUM`/`MIN`/`MAX`/`AVG`; [aggregates.md](aggregates.md))
  **and** scalar functions (`abs`, `round`, `make_interval`, the uuid/clock functions;
  [functions.md](functions.md) §9–§12). An aggregate may carry a leading `DISTINCT`
  (`COUNT(DISTINCT x)`, [aggregates.md](aggregates.md) §5) and/or a trailing `FILTER (WHERE cond)`
  ([aggregates.md](aggregates.md) §11); both are aggregate-only (`42809` on a scalar function), an
  empty/`*` `DISTINCT` argument is `42601`, and a non-boolean / aggregate-bearing `FILTER` cond is
  `42804`/`42803`. An unknown function name is `42883`.
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

The grammar parses a column's and a `CAST`'s type as an `identifier` — the catalog
owns the type lattice and resolves the (folded, §3) name exactly, rejecting unknowns at
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
connectives — landed together as the `expr` tower above (`AND`/`OR` are Kleene three-valued and
evaluate **left to right with short-circuit**: a FALSE left operand decides `AND` and a TRUE one
decides `OR` without evaluating the right operand, so `d <> 0 AND 10 / d > 1` never divides by
zero; in a `WHERE`/`ON`/`HAVING` the top-level `AND` chain also stops at a NULL conjunct, as
PostgreSQL's qual list does — [cost.md](cost.md) §3, a stronger promise than PostgreSQL's
unspecified operand order); [../../TODO.md](../../TODO.md)
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

1. **`expr AS alias`** or **`expr alias`** (a bare label, below) → the `alias` **name** —
   folded like any identifier (§3), so `AS Total` names the column `total` and `AS "Total"`
   names it `Total` (PostgreSQL). The alias is a pure
   output label, *not* entered into the **input** resolution namespace —
   WHERE, HAVING, and sibling select items never see it (they resolve against the FROM
   columns). The one place an alias *is* visible is the trailing **`ORDER BY`** of the same
   query (and a set-operation `ORDER BY`), which resolves a bare sort-key name against the
   **output** columns first — the SQL-standard rule (§10). Matching there is byte-exact on
   names, so `… AS Total … ORDER BY TOTAL` binds (both fold to `total`) while
   `… AS "Total" … ORDER BY total` does not.
   Aliases may collide with a real column name or with each other (no uniqueness check); a
   colliding bare-name `ORDER BY` key is the one place that is observable — two output
   columns of the same name with **different** expressions make such a key `42702`
   (*ambiguous*, §10).
2. **A bare column reference** (no alias) → the **catalog's canonical column name** at the
   resolved index, i.e. the name from `CREATE TABLE`. With `c i32` declared, `SELECT C FROM t`
   names the column `c` (`C` folds to `c`, §3); with `"C" i32` declared, the column is `C` and
   is referenced as `"C"`.
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

**Bare labels (`AS` omitted).** A select item (and a `RETURNING` item, §32) may name its
column without `AS` — `SELECT count(*) total`, `SELECT id "X"` — PostgreSQL's bare-label rule.
After the item's **complete** expression the parser takes the next token as the label when it
is a **quoted identifier** (never a keyword, §3) or a **bare word outside the no-bare-label
set**; anything else ends the item. Because jed reserves no keyword, this set is the one place a
bare word is refused, and it is fixed as **exactly PostgreSQL's `AS_LABEL` keywords** (PG 18
`kwlist.h`), 39 words:

> `array as char character create day except fetch filter for from grant group having hour
> intersect into isnull limit minute month notnull offset on order over overlaps precision
> returning second to union varying where window with within without year`

They are the words that may legally follow a select item (the trailing clauses `from`, `where`,
`group`, `having`, `window`, `order`, `limit`, `offset`, `fetch`, `for`, `into`, `returning`,
`on` — an `INSERT … SELECT`'s `ON CONFLICT` — and the set operators), or that continue an
expression in PostgreSQL's grammar (a type name's `precision`/`varying`/`with`/`without`/`array`,
an interval field `day`…`second`/`to`, the `over`/`filter`/`within` postfixes, `overlaps`,
`isnull`/`notnull`). Each needs `AS` (`SELECT 1 AS year`) or quoting (`SELECT 1 "year"`); a
bare one after an expression is `42601`. Every other word — including keyword spellings like
`select`, `case`, `null`, `end`, `desc`, `join`, `not`, `at` — is a label. A bare label is
**one** identifier (`SELECT 1 x y` is `42601`), folds like any bare identifier, and binds a bare
`ORDER BY` key exactly like an `AS` alias. The set is a CLAUDE.md §8 cross-core surface,
hand-written in each parser and pinned word-by-word by `query/bare_alias.test`.

Two documented divergences, both narrowings of pathological spellings (oracle-overridden):

- **Operator words stay operators.** jed's expression parser is recursive descent: after an
  operand it reads `and`, `or`, `is`, `in`, `between`, `like`, `ilike`, and `collate` as the
  operator they begin, so a *dangling* one (`SELECT 1 and`) is `42601`. PostgreSQL's LR grammar
  defers the choice and labels the column `and`. The conditional continuations are PG-identical:
  `not` continues only before `in`/`between`/`like`/`ilike`, `at` only before `time zone`, and
  `escape` only directly after a `LIKE`/`ILIKE` pattern (§22), so `SELECT 1 not, 2 at, 3 escape`
  labels all three, and `SELECT 'a' LIKE 'b' escape` is `42601`, as in PostgreSQL.
- **No postfix `ISNULL`/`NOTNULL`.** They are not a jed surface (`IS [NOT] NULL` is), so `SELECT
  1 isnull` is `42601` where PostgreSQL evaluates the postfix operator.

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

`ORDER BY` is **one or more sort keys** (`order_by` / `order_term` in the grammar), each a
**general expression** (`ORDER BY a + 1` — see below; a bare table column is its simplest form)
or an **output-column ordinal** (`ORDER BY 1` — see below) with an optional
direction (`ASC` / `DESC`, default `ASC`) and an
optional explicit NULL placement (`NULLS FIRST | LAST`). Keys apply **left to right**: the
first is primary, the next breaks its ties, and **a full tie across all keys is broken by the
primary key** — so `ORDER BY` fixes the order *completely*, ties included. (That last tie-break
is a deliberate, documented determinism choice beyond the SQL standard — CLAUDE.md §8/§10:
unlike row order *without* `ORDER BY` (now unspecified), order *under* `ORDER BY` is fully
deterministic. Today it is realized by a **stable** sort over the primary-key scan; under
future parallel execution it is the same observable result via an implicit primary-key
tie-break, so it stays parallelism-compatible.) A **bare-name** key resolves an **output
column first**, then an input column (the alias form, below) — a name need not appear in the
projection (an unmatched name falls through to the *table's* columns); an **ordinal** key
resolves by *position* into the select list (below).

**The general-expression form (`ORDER BY a + 1`).** A sort key may be **any expression** — `ORDER
BY a + b`, `ORDER BY b % 2`, `ORDER BY abs(b - 25)`, `ORDER BY (b > 15)` — evaluated **per row**
and sorted by the computed value (a bare column is just its degenerate case, kept on the fast
path below). In a **plain** query the expression resolves against the FROM columns; in a **grouped /
aggregate** query it resolves against the group context exactly as a select-list item does — it may
combine grouping expressions and aggregates and may introduce a **new aggregate not in the select
list** (`SELECT a FROM t GROUP BY a ORDER BY sum(b)`), while a non-grouped, non-aggregated column is
the usual `42803`. The value is **materialized** (evaluated once per pre-sort row and appended,
the same mechanism a window key uses — window.md §5.1) so the slot-based sort/`LIMIT`/projection
pipeline below is unchanged and the cost stays *sort-before-project*; the per-row expression
evaluation is **metered** like a projection (an `operator_eval` per node — the only new cost, and
none for a bare-column key). A **non-orderable** result type — `json` (no btree opclass, json.md
§5) — is `42883`; `jsonb` is orderable. A text-typed expression takes its **derived collation**
(its expression's derivation — an inner `COLLATE`, a column's frozen collation carried through
functions, casts, `CASE`/`COALESCE`, aggregates and subqueries, else `C`/byte order — collation.md
§1; a key combining two different implicit collations is `42P21`), overridable by a trailing
`COLLATE` on the key. Because the key is a full expression, `ORDER BY
1 + 1` is a **constant expression** (a no-op sort — PostgreSQL's reading), **not** an ordinal.
Materializing a key disables the PK-scan elision below, so an expression key always takes the
blocking sort.

**The ordinal form (`ORDER BY 1`).** A sort key may also be an **output-column ordinal** — an
integer literal naming the **1-based position** of a select-list item, the SQL92 form PostgreSQL
accepts. The ordinal resolves to the item's value: `SELECT a, b FROM t ORDER BY 1`
orders by `a`, `SELECT a AS x, b FROM t ORDER BY 1` orders by the real column `a` (the alias is
just a label — §8), and with `SELECT *` the ordinal indexes the expanded scope columns left to
right. An ordinal pointing at a **computed** select-list item (`SELECT a + 1 ... ORDER BY 1`)
sorts by that item's value — the same materialization as a general-expression key. A position
**< 1 or greater than the number of output columns** is `42P10`
(`invalid_column_reference`, *"ORDER BY position N is not in select list"*) — matching PostgreSQL,
which also reports `ORDER BY 0` / `ORDER BY -1` this way. The ordinal/expression split matches
PostgreSQL exactly: a **bare** integer literal (optionally a leading `-`, folded so a negative
position reaches `42P10` not a syntax error) is an ordinal, but anything more — `ORDER BY 1 + 1`,
`ORDER BY -1 + 1`, `ORDER BY (1)` *(which folds to the bare literal, so still an ordinal)* — is a
constant **expression**. The ordinal is parsed only in the **query / set-operation** `ORDER BY`;
in a `WITHIN GROUP (ORDER BY …)` an integer is a constant expression, not an ordinal (PostgreSQL
again — aggregates.md §13), and that clause stays column-only (its general-expression form is a
separate deferred follow-on).

**The alias form (`ORDER BY s`).** A **bare** sort-key name (one unqualified identifier, no
direction/COLLATE before it changes this) resolves against the **output columns first**, then the
input columns — PostgreSQL's SQL92 rule, the *opposite* of `GROUP BY`'s precedence and the reason
the docs call it an inconsistency. So `SELECT a + b AS s FROM t ORDER BY s` sorts by the computed
`a + b` (the alias binds), `SELECT a AS x FROM t ORDER BY x` sorts by `a` even though no column `x`
exists, and a bare name shadows a same-named input column — `SELECT a AS b FROM t ORDER BY b`
sorts by `a` (the output column `b`), not the table column `b`. The match uses each select item's
**output name** — its `AS` alias, else its derived name (a bare column's canonical name, an
un-aliased aggregate's lowercased function name, so `SELECT count(*) FROM t ORDER BY count`
binds), byte-exactly on names (§3, §8); an un-nameable item (`?column?`) never matches. A matched name
behaves exactly like the **ordinal** pointing at that item: a plain-column item stays on the
column fast path (so PK-scan elision still applies — `SELECT id AS k FROM t ORDER BY k` over the
primary key needs no sort), and a **computed** item is materialized like a general-expression key.
The alias binds **only when the whole key is a bare name** — `ORDER BY s + 1` is a general
expression, so `s` there resolves against the *input* columns and is `42703` when no such column
exists (PostgreSQL again: the alias is not in scope inside an expression). When **two** output
columns share the name with **different** expressions the bare key is **`42702`**
(`ambiguous_column`, *"ORDER BY \"s\" is ambiguous"*); two items that are the *same* expression
(`SELECT a, a FROM t ORDER BY a`) are not ambiguous. A **set-operation** `ORDER BY` already
resolves names against the unified output columns (below), so the alias form is the single-`SELECT`
counterpart of that.

**Correlated key (now supported, `query.order_by_correlated`).** An `ORDER BY` key inside a subquery
may reference a column of an **enclosing** query (`SELECT a, (SELECT b FROM u ORDER BY t.a LIMIT 1)
FROM t`). The outer column is a per-evaluation **constant** of the enclosing row, so it is a degenerate
(constant) leading sort key — a deterministic later key decides the order — and PostgreSQL accepts it.
Both a bare/qualified outer-column key and a general expression that *contains* an outer reference
(`ORDER BY t.a + u.b`) are **materialized as an `OuterColumn` expression** evaluated per outer row
against the outer-row environment (never a local sort-row slot, which holds only this query's columns).
A subquery correlated **only** through its `ORDER BY` is still correlated, so it re-executes per outer
row (the correlation detector inspects the materialized order expressions). Skip-level (grandparent)
references work.

**Correlated `GROUP BY` key (now supported, `query.group_by_correlated`).** A `GROUP BY` inside a
subquery may be — or contain — a column of an enclosing query: `SELECT (SELECT count(*) FROM u GROUP
BY t.a) FROM t`, `GROUP BY u.b + t.a`, or an ordinal / bare name that resolves to an outer column. As
in PostgreSQL the outer column is a per-evaluation constant, so grouping by it alone puts every input
row of one execution in a **single group** — and an empty input forms **no** group (a scalar subquery
is NULL, unlike `GROUP BY ()`'s one grand-total row). The key is materialized like a general-expression
key (aggregates.md §15): an `OuterColumn` expression evaluated per input row against the outer-row
environment, so a subquery correlated only through its `GROUP BY` re-executes per outer row (the
correlation detector and the touched-column walk inspect the materialized group keys). A select-list /
`HAVING` / `ORDER BY` reference to a grouped outer column reads the group's key slot whatever its
spelling (`x` or `t.x` — PostgreSQL matches the resolved column), so a `ROLLUP (t.x)` grand-total row
sees it NULL. A local column outside the key is still `42803`. `GROUPING(t.x)` of an outer column
belongs to the outer query level, which PostgreSQL rejects (`42803`); jed does too. Skip-level
(grandparent) outer keys work. No cost change: an outer column is a leaf (no `operator_eval`).

**Window function / `GROUPING()` inside a key (now supported, `query.order_by_window` /
`query.order_by_grouping`).** An `ORDER BY` key may be — or contain — a **window function**
(`ORDER BY row_number() OVER (ORDER BY b)`, `ORDER BY rank() OVER (...) % 2`) or a **`GROUPING(...)`**
call (`GROUP BY ROLLUP (a) ORDER BY GROUPING(a), a`). The key is resolved in the **same** context the
projection uses — windowing for a window query, the group context for a grouped query — so the call
collects into the **same** window-spec / grouping-spec list and references the same placeholder; the
window/`GROUPING()` placeholder rebases therefore run **after** ORDER BY resolution, once the spec
counts are final (an order-introduced window spec or window key shifts every slot consistently). A
query whose **only** `OVER` call sits in the `ORDER BY` still sets up the window machinery, and sorting
by a select-list window value via its **output alias** works the same way (the alias binds the computed
item, which is materialized — it no longer re-resolves to `42P20`). Execution is unchanged: the window
stage appends its results before `ORDER BY` materializes the key over those rows. This composes with a
**grouped** query: an expression key in a query that **both** groups and has window functions
(`SELECT g, sum(v), rank() OVER (ORDER BY sum(v)) FROM t GROUP BY g ORDER BY sum(v) + 1`, or `ORDER BY
rank() OVER (ORDER BY sum(v)) DESC`) resolves in the grouped+window context — collecting aggregates and
window specs at once — and is materialized over the grouped rows **after** the window stage extends
them, so the order values follow the window results (`query.order_by_grouped_window`).

**Still narrowed (§5).** *(The output **alias** key, the **correlated** key, a **window function /
`GROUPING()`** in a key, and the **grouped+window** expression key are now supported — above.)* A
**set-operation** `ORDER BY` (after `UNION`/`INTERSECT`/`EXCEPT`) accepts only an output column name or
ordinal — a general-expression key there is `0A000` (*"invalid UNION/INTERSECT/EXCEPT ORDER BY clause"*,
matching PostgreSQL, which rejects expressions once the inputs are unified). **GROUPING SETS** (more
than one set) combined with **window functions** remains a `0A000` deferral.

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

**The sort is elided when the scan already produces the order.** When the `ORDER BY` keys are an
`ASC` prefix of the single base table's **primary key** (sorting by each column's stored key
order), the table scan already delivers rows in that order, so the engine skips the sort entirely
and streams scan→filter→project — and with a `LIMIT` short-circuits a top-N
([cost.md](cost.md) §3 "ORDER BY satisfied by primary-key order"). The result is identical (the
PK scan order *is* the requested order, tie-break included); only the cost drops for the `LIMIT`
case. A collated PK is stored in collation order, so a collated `ORDER BY` is satisfied for free.
`DESC` (a reverse scan), secondary-index order, `DISTINCT`, and joins keep the blocking sort —
each a relaxable follow-on.

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
(`INSERT ... SELECT`) is §24. General expressions in a `VALUES` value slot are §16.

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
expression tree per row at INSERT, accruing that evaluation's cost, and so does a **general
expression slot** (`VALUES (1 + 1)`, §16). An `INSERT ... SELECT` is different again: it accrues
exactly the embedded `SELECT`'s cost (§24).

## 13. `DROP TABLE`

`DROP TABLE t` removes a table — **its definition and all its rows** — from the catalog
(`drop_table` in the grammar). It is the inverse of `CREATE TABLE`: where CREATE registers
a name in the catalog (and rejects a name already taken, §1, `42P07`), DROP removes one
(and rejects a name not present). Both stores the table touches — the catalog entry and the
per-table row store — are dropped together, keyed by the table's name (§3: `DROP TABLE T`
folds to and drops `t`; `DROP TABLE "T"` drops only a table named `T`). After a drop the name is free again, so
`DROP TABLE t` then `CREATE TABLE t (...)` re-creates it from empty.

**A missing table is `42P01` — unless `IF EXISTS`.** Dropping a table that does not exist
raises `42P01` (`undefined_table`, *"table does not exist: t"*) — the same code a
`SELECT` / `INSERT` / `UPDATE` / `DELETE` against an unknown table already raises. This
mirrors `CREATE TABLE`'s `42P07`-on-duplicate (§1) and matches PostgreSQL's bare
`DROP TABLE`. The idempotent **`DROP TABLE IF EXISTS t`** form turns that missing-table
error into a **no-op success** (PostgreSQL emits a notice; jed has no notice channel, so it
is a silent success). `IF EXISTS` suppresses **only** the missing-table error — a name that
*does* resolve, but to a non-table relation (a secondary index), is still the `42809`
(`wrong_object_type`, *"t is not a table"*) error that the bare form raises (PG keeps it
too). The `IF EXISTS` keyword pair is recognized by a **two-token lookahead** (the shape the
statement dispatch uses), so a lone `if` stays an ordinary non-reserved identifier:
`DROP TABLE if` drops a table named `if` — PG-faithful (§1) — while only the adjacent pair
`IF EXISTS` selects the idempotent form.

**`CREATE TABLE IF NOT EXISTS` — the symmetric idempotent create.** A name already taken in the
target database's relation namespace — a table, index, or sequence (the namespace is shared, §1) —
turns `CREATE TABLE`'s `42P07` into a **no-op success** (PostgreSQL emits a *"relation already
exists, skipping"* notice; jed has no notice channel, so it is silent). It is **not** a "create or
replace": the existing relation keeps its definition and rows whatever the new definition says.
Following PostgreSQL (`transformCreateStmt` checks existence first), the skip is decided **before
any column or constraint work**: the skipped definition is never validated (an unknown type, a
repeated column, a jed narrowing that would be `0A000` on a create — none is reported) and creates
nothing (a `serial` column's owned sequence is not made). What *does* still run first is the
statement's own gating and target resolution — the `allow_ddl` / `allow_temp_ddl` gate (`42501`),
the database qualifier (an unattached one is `42P01`, a read-only attachment `25006`), and the
`jed_` reservation (`42939`; a reservation is not a collision, as for `CREATE SEQUENCE IF NOT
EXISTS`). The collision is checked in the namespace the bare form would check: a host attachment's
own namespace for `db.table`, and the temp-aware session namespace otherwise — so under the
preclude-overlaps rule ([temp-tables.md](temp-tables.md) §3) `CREATE TEMP TABLE IF NOT EXISTS p`
with a persistent `p` is a **no-op**, where PostgreSQL would create a `pg_temp` table shadowing it
(the same documented divergence the bare form's `42P07` carries). Like `IF EXISTS`, the keywords
are recognized by a **two-token lookahead** (`IF NOT`), so `CREATE TABLE if (...)` still creates a
table named `if`.

**`CREATE TEMP TABLE … ON COMMIT`.** An optional `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }`
after the closing parenthesis picks a temporary table's end-of-transaction action
([temp-tables.md](temp-tables.md) §5.1): keep it (the default), empty it at every commit, or drop it
when its creating transaction commits. As in PostgreSQL, any `ON COMMIT` clause on a table that is
not temporary — `PRESERVE ROWS` included, and a `main.`- or attachment-qualified table — is `42P16`
*"ON COMMIT can only be used on temporary tables"*, checked after the `IF NOT EXISTS` skip and
before any column or constraint work. The words are recognized positionally after `)`, so none
becomes reserved; anything else there is `42601`.

**`CREATE TABLE … AS query`** creates a table from a query's output columns and fills it with the
query's rows (§57, [create-table-as.md](create-table-as.md)). Unlike the column-definition form, its
`IF NOT EXISTS` skip comes **after** the query is analyzed (PostgreSQL's order), so the query's own
errors are reported even for a taken name.

**Multi-table drop.** `DROP TABLE a, b, c` removes several tables in one statement. It is
**two-phase / all-or-nothing** (the discipline §12 uses for multi-row work): every name is
resolved and validated *first* — a missing one is `42P01` (unless `IF EXISTS`, which skips
just that name), a non-table relation is `42809` — and only if the whole list checks out is
*anything* removed. So `DROP TABLE a, missing` drops nothing and raises `42P01`; `a` survives.
A name **repeated** in the list is deduplicated (`DROP TABLE a, a` drops `a` once and
succeeds), matching PostgreSQL, which collects the targets into a set.

**`CASCADE` / `RESTRICT`.** Now that foreign keys exist (`format_version` 11,
[constraints.md](constraints.md) §6), a table can have a dependent — another table's `FOREIGN
KEY` that references it — so the trailing keyword is meaningful:

- **`RESTRICT`** (the default, and the behavior of the bare form) refuses the drop if a table
  *outside the statement's drop set* references it, raising `2BP01`
  (`dependent_objects_still_exist`). This is the existing single-table FK-parent check.
- **`CASCADE`** drops those dependent `FOREIGN KEY` constraints along with the table: the
  referencing table and its rows survive, only its now-dangling FK constraint is removed.
  (Secondary indexes and owned `serial` sequences are *internal* dependents — `DROP TABLE`
  always removes them regardless of mode, §6/sequences.md §12 — so the keyword governs only
  cross-table FK dependents.)

A foreign key **between two tables both named in the same statement** never blocks, under
either mode: dropping the referencing table removes the dependency, so `DROP TABLE parent,
child` (or `child, parent`) succeeds even under `RESTRICT`. The drop-set exclusion — a
dependent that is *itself* being dropped does not count — is what the two-phase resolve buys.

**Views.** A **view** that reads a table being dropped blocks the drop under `RESTRICT` (`2BP01`);
`CASCADE` drops it, and every view depending on it, along with the FK dependents
([views.md](views.md) §6.1). A view itself is dropped with `DROP VIEW` (§54); `DROP TABLE v` of a
view is `42809`.

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
malformed exponent (`1e`, `1e+`, `1ex` — the `e` not followed by `[+-]?digit`) is `42601` trailing
junk after numeric literal, as PostgreSQL reports it (§4). `_` separators may sit between the
digits of the integer part, the fraction, and the exponent (`1_000.000_1`, `1e1_0`); they carry no
value and do not count toward the scale (`1.5_0` is scale `2`). The lexer clamps an absurd exponent magnitude (a determinism/resource guard,
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
takes none (`i32(5)`) is a resolve-time error (`0A000`; `text`'s `varchar(n)` length limit is the
second parameterized type, [types.md](types.md) §15). The
limits, the p/s interaction (integer-part digits ≤ `p − s`), and the rounding-on-coercion rule
are the type system's, detailed in [decimal.md](decimal.md) §2–3; the grammar fixes only that
the *syntax* is `type_word "(" integer ("," integer)? ")"`.

**The two-word `character varying`** ([grammar.ebnf](../grammar/grammar.ebnf) `type_word`). A type
name is one identifier, with exactly one exception: PostgreSQL's `character varying`, the alias of
`text` ([types.md](types.md) §15), so `character varying(n)` is `varchar(n)`. Every type position
— a column or composite-field definition, `ALTER COLUMN … TYPE`, `CAST(… AS …)`, `::`, a typed
literal (`character varying 'x'`, recognized by a two-token lookahead to the string), a SQL/JSON
`RETURNING`, a `JSON_TABLE` column, and `CREATE SEQUENCE … AS` — reads it the same way: the bare
word `character` immediately followed by the bare word `varying` (either case) is the one name
`character varying`. A quoted `"character"` is an ordinary identifier, so `"character" varying`
leaves `varying` as stray input (`42601`). A `character` not followed by `varying` stays a one-word
name, which no type has (`42704` at resolve; the blank-padded `char(n)` / `character(n)` is out of
scope). This two-token lookahead is a §8 determinism surface the three parsers match exactly.

## 15. Multi-table `FROM` and `JOIN`

The `SELECT` `FROM` clause grows from a single table name to a **left-deep chain** —
`from_clause ::= table_ref join_clause*` — adding table aliases, qualified column references
(`t.col`), and the first multi-table relational operators. The engine executes **`INNER JOIN
... ON`**, **`CROSS JOIN`**, and the **`LEFT`/`RIGHT`/`FULL [OUTER] JOIN`** family (outer joins
landed as an executor-only follow-on — see "Outer joins" below). The reasoning lives here; the
cost contract is in [cost.md](cost.md) §7.

**Table references and aliases.** `table_ref ::= qualified_table ("AS"? identifier)?` — a table
name with an optional alias, the `AS` optional (`FROM orders o` = `FROM orders AS o`). The alias, or
the table name when there is none, is the relation's **label**. Labels qualify columns and must
be **distinct**: two relations with the same label — a self-join written without aliases
(`FROM t JOIN t ...`) — is **`42712`** (`duplicate_alias`, *"table name t specified more than
once"*, [../errors/registry.toml](../errors/registry.toml)), matching PostgreSQL. A self-join is
therefore written with two distinct aliases (`FROM t AS a JOIN t AS b ON ...`). Labels compare
byte-exactly after folding (§3), like every other name — `FROM t AS A JOIN u AS a` is `42712`, while
`FROM t AS "A" JOIN u AS a` is two distinct labels.

**Base-table column alias lists.** An aliased base table (or view, [views.md](views.md) §4) may carry
a parenthesized **column alias list** after its alias: `FROM t AS x (p, q)`. It renames the table's
columns left to right **as this FROM item exposes them**, PostgreSQL's rule. A shorter list is a
partial rename (the remaining columns keep their names). A longer list is **`42P10`** (*table "x"
has 3 columns available but 4 columns specified*). There is no list without an alias: `FROM t (p)`
is the table-function form (§35), a call of a function `t`, exactly as in PostgreSQL. The renamed names are the only names that FROM item exposes:
`x.p` resolves, `x.a` is `42703`, `x.*` and `*` expand under the new names, and a `USING`/`NATURAL`
join matches on them. The rename is a pure resolution-time relabeling. Storage, access paths, index
eligibility, cost, and plan are those of the same query written with the original names. A
duplicate name in the list follows PostgreSQL: it is accepted, and a later bare reference to the
duplicated name is `42702`. Views use this list to keep seeing a column under its old name after
`RENAME COLUMN` ([views.md](views.md) §7).

**The database qualifier (`qualified_table`).** `qualified_table ::= (identifier ".")? identifier`
— an OPTIONAL leading `db "."` names the **attached database** a table lives in (`reports.sales`),
jed's first multi-part name in **table** position ([attached-databases.md](attached-databases.md)
§3). The qualifier is a *database*, not a schema (jed has no schemas / `search_path`); it applies to
`table_ref` (FROM/JOIN), the DML targets (`INSERT INTO` / `UPDATE` / `DELETE FROM`), and — added in
Slice 1b — the DDL targets `CREATE TABLE` and `CREATE INDEX … ON` (creating *into* an attachment; a
column-level `REFERENCES` parent stays **bare**, a qualified parent there is `42601`). The label is
still the alias or **table** name — the database qualifier **never** becomes a label or an output
name (a column stays the 2-part `rel.col` below; there is no 3-part `db.table.col` this slice). A
bare name resolves in **implicit** scope; the reserved implicit qualifiers `main` (the file database)
and `temp` (the session-local domain) resolve to the implicit scope, and — since Slice 1b — a
host-attached database name routes to that attachment; an unknown qualifier is `42P01` "database … is
not attached". Because jed **precludes overlaps** (a name is temp XOR persistent within a session, §3),
a `main.`/`temp.` qualifier resolves to the *same* relation the bare name would, so for those two the
resolver merely **validates** the relation is in the claimed scope (`42P01` otherwise); a *host
attachment* qualifier is real **N-way routing** into that database's own catalog and stores (Slice 1b). A `db "."` never precedes the derived-table or set-returning-function forms — a
database-qualified name is a base table only, so a qualified function call is `42601`. The `db "."`
slot is free because a bare table name never carried a leading dot (the only dotted name was `rel.col`
in column position, below).

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
directive asserts them positionally).

**Qualified star `t.*`.** A select item may be `t.*` — the columns of the FROM relation labeled
`t` alone, in catalog order — the per-relation counterpart of bare `*`. Unlike bare `*` (a
standalone select list), `t.*` is an ordinary item and **may be mixed** with others
(`SELECT t.*, u.x`, `SELECT b.label, a.*`); two qualified stars expand each relation in turn. The
parser recognizes it by the three-token shape `identifier "." "*"` *before* the general expression
parser, so `t.col` (Dot then a word) and `a * b` (no Dot) are unaffected, and a bare `*` is still
the standalone form. The qualifier resolves against the **local** FROM scope by label exactly as a
qualified column does — an unknown label is `42P01`. Two deliberate narrowings diverge from
PostgreSQL (both `42601`, recorded in `oracle_overrides.toml`): `t.*` takes **no alias**
(`t.* AS x` and the bare-label `t.* x` are rejected; PG accepts both), and it is a
**select-list / RETURNING construct only** — in a scalar position (`WHERE t.*`, `t.* + 1`, `count(t.*)`) the parser rejects it, where PG would
treat `t.*` as a whole-row value (jed has no row-valued `t.*`). It is distinct from the composite
whole-row expansion `(expr).*` (a postfix on a parenthesized base — composite.md §S4): `t.*` names
a *relation*, `(c).*` a *composite value*; the bare-identifier shape selects the relation reading.

**The join operators.** `join_clause ::= "CROSS" "JOIN" table_ref | join_type? "JOIN" table_ref
("ON" expr | "USING" "(" identifier ("," identifier)* ")")`. A bare `JOIN` is `INNER` (the keyword
optional). The `ON` predicate is a general expression that **must resolve to boolean** — a
non-boolean `ON` is `42804`, the same rule WHERE takes — and is evaluated **at the join** over the
combined (left-concatenated-with-right) row; only a **TRUE** result keeps the pair (three-valued, so
a `NULL` join key never matches, matching PostgreSQL inner-join semantics). `CROSS JOIN` is the
Cartesian product (no condition). An `INNER`/bare `JOIN` with **neither `ON` nor `USING`** is
`42601`, and a `CROSS JOIN ... ON/USING ...` is likewise `42601`.

**`USING (col, …)` — the column-merge join condition.** `a JOIN b USING (k)` is an **equi-join on
the named columns** that **merges** each into a single output column (PostgreSQL — grammar.md §15).
Equivalent to `a JOIN b ON a.k = b.k`, *except* the output carries **one** `k`, not two. The merge
shows up in three places: (1) `SELECT *` lists the merged columns **first** (in `USING` order), then
the left input's other columns, then the right's — so `SELECT * FROM a JOIN b USING(k)` is
`k, <a's rest>, <b's rest>`. This is PostgreSQL's rule applied **per join**, recursively: a join's
output columns are (its own merged columns) ++ (its left input's output columns minus those merged
here) ++ (its right input's columns minus its merged copy), so in a left-deep chain the **newest**
join's merges lead — `a(id, x) JOIN d(k, x) USING (x) JOIN e(k, y) USING (k)` is `k, x, id, y`, not
`x, k, id, y` — and each comma-separated FROM item lays out its own merges at that item's position
(`a JOIN b USING (id), d JOIN c USING (k)` is `id, <a's rest>, <b's rest>, k, <d's rest>, <c's rest>`,
the `k` not hoisted to the front); (2) a **bare** `k` resolves to the merged column, never the `42702`
ambiguity the two underlying copies would otherwise raise; (3) the qualified `a.k` / `b.k` still
reach each side directly. The merge value is the **surviving side**: the **left** column for
`INNER`/`LEFT` (always present for a left-preserved row), the **right** column for `RIGHT` — both
exactly `COALESCE(a.k, b.k)` for those kinds, since the surviving side is the non-NULL one. Only
`FULL JOIN USING`, whose merge is a genuine `COALESCE` of two possibly-NULL sides, is **not** a
single column; it is a deferred **`0A000`** narrowing (the single-flat-index merge model carries
one column, not a coalesce). Each `USING` column must exist in **both** sides (else `42703`), and a
type-mismatched pair reaches jed's **strict comparison** (`42804`, a documented divergence from PG's
`42883`). `USING` composes with multi-column lists, the outer kinds (INNER/LEFT/RIGHT), and a
**chain** (`a JOIN b USING(k) JOIN c USING(k)` keeps one `k`). Implementation: the planner
synthesizes the `left.col = right.col` predicate and records each merge as `(name → surviving flat
index)` on the scope, with both underlying copies marked **hidden** so `*` and bare resolution skip
them, and builds the `*` column order join by join alongside; this is computed **before** GROUP BY /
DISTINCT / projection so every clause sees the merge.
`USING` is non-reserved (§3) — it is the join condition only as the keyword immediately following the
right `table_ref`, joining the implicit-alias stop set so `JOIN b USING (…)` never reads `using` as
`b`'s alias.

**`NATURAL` — the derived `USING`.** A `NATURAL` prefix (`a NATURAL JOIN b`, `a NATURAL LEFT JOIN b`)
**derives** the `USING` column list at resolution as the column names **common to both sides**, in
**left order** — the left input's `*` order above, so after a `USING` join its merged column comes
first (each taken once) — then merges them exactly like `USING` — so all the merge behavior
above (merged-first `*`, unambiguous bare reference, surviving-side value, the `FULL` `0A000`) carries
over unchanged. With **no** common column the join degenerates to a **`CROSS` join** (the Cartesian
product — an empty derived list means no predicate and no merge). `NATURAL` is a parser prefix on the
join type, takes **no** `ON`/`USING` (a `NATURAL … ON`/`USING` is `42601`), and **`NATURAL CROSS
JOIN`** is `42601` (NATURAL implies a condition). Like `USING`, `NATURAL` is non-reserved and joins
the implicit-alias stop set (it is the prefix only immediately before a join — `FROM t NATURAL JOIN
…` never reads `natural` as `t`'s alias); a bare `NATURAL` not followed by a join is `42601`.

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

**Comma-`FROM` (the implicit cross join).** `FROM a, b` is an **implicit `CROSS JOIN`** — the
Cartesian product, byte-identical in result and cost to `a CROSS JOIN b`. The `from_clause` grew
from one chain to `from_item ("," from_item)*`, where each `from_item` is the old `table_ref
join_clause*`. A comma synthesizes a `CROSS` `JoinClause` (no `ON`) for the next item, flagged
`comma: true`; because the engine already executes `CROSS` and the flat-row model concatenates
relations, this is **resolver-only** beyond the parser loop. Left-deep flattening (`a, b JOIN c …`
becomes `a CROSS b JOIN c …`) yields the **identical row multiset** as the precedence-correct
parse when the later item's joins are inner or `LEFT`, because a cross product commutes through
their preserved left rows; only jed's *cost* reflects its own left-deep execution (the rows still
match PG — the oracle confirms it). A `RIGHT` or `FULL` join does not commute: `(e × a) RIGHT JOIN b`
keeps an unmatched `b` row once, where `e × (a RIGHT JOIN b)` keeps it once per `e` row (and not at
all when `e` is empty). So the parser makes a **later comma item whose chain holds a `RIGHT` or
`FULL` join** the unaliased parenthesized join it is — `FROM e, a RIGHT JOIN b ON p` is `FROM e, (a
RIGHT JOIN b ON p)` (below) — which keeps the item's own `ON` scopes, `*` layout, and visible labels,
and joins it to the items before it as one relation.

The one place the comma's looser precedence is **observable** is ON-resolution scope: a comma binds
**looser than `JOIN`**, so each `from_item` is its own segment and a join's `ON` may reference only
relations **within its own comma item**, never an earlier one. So `FROM a, b JOIN c ON a.x = c.x` is
`42P01` on `a` (*"missing FROM-clause entry for table a"*) — exactly as PostgreSQL rejects it. The
planner computes each join's **segment start** (the most recent `comma`-flagged relation at or
before it) and resolves its `ON` against `rels[seg_start..=k+1]` rather than `rels[..=k+1]`; with no
commas every segment starts at relation 0, so a pure `JOIN` chain is unchanged. **`LATERAL` is not
segment-restricted** — a lateral item's body resolves against *all* preceding relations, so
`FROM t, LATERAL (… t.col …)` correlates across the comma (PostgreSQL); only a join's `ON` is
narrowed. This makes `FROM t, LATERAL (…)` reachable for the first time (until now LATERAL needed
explicit `JOIN` syntax — [../../TODO.md](../../TODO.md)).

**Parenthesized joins (`FROM (a JOIN b ON …) [AS g [(cols)]]`).** A `(` at the start of a
`table_ref` that is **not** followed by `SELECT`, `WITH`, or `VALUES` (those begin a derived table,
§42) begins a **parenthesized join**: `"(" table_ref join_clause+ ")" derived_alias?`. It must
contain at least one join and no comma (`FROM (a)` and `FROM (a, b)` are `42601`, as in
PostgreSQL); its items may themselves be parenthesized joins, derived tables, or table functions;
`LATERAL` may not precede it (`42601`); and its nesting counts toward `MAX_EXPR_DEPTH` (`54001`).
The parentheses set the **join tree**: `c LEFT JOIN (a JOIN b ON p) ON q` preserves every `c` row
unless the *whole* inner join matches, whereas the left-deep `c LEFT JOIN a ON q JOIN b ON p` drops
the `c` rows its trailing inner join cannot extend. The group's `ON`/`USING` clauses see only the
group's own relations — a reference to an enclosing sibling is `42P01` (PG's "invalid reference to
FROM-clause entry"), as is one from a non-`LATERAL` derived table inside the group (§42).

**`LATERAL` inside a group.** PostgreSQL resolves a parenthesized join in its enclosing FROM's
namespace, where the relations to the join's **left** are lateral-only entries: a `LATERAL` item or
table function (§44) anywhere inside the group — including a nested group, and including an
**aliased** group, whose alias hides only the inner labels from the enclosing query — may reference
them, while the group's `ON` clauses and its non-`LATERAL` items may not. So `FROM a JOIN (b JOIN
LATERAL (SELECT a.x + b.y) s ON true) ON true` works, re-evaluating the group once per `a` row, and a
non-`LATERAL` subquery *inside* the `LATERAL` item sees `a` too (the whole lateral item is in its
scope). §44's join-type rule applies at every level: a group on the right side of a `RIGHT`/`FULL`
join may not reference that join's left side (`42P10`), while a reference to an earlier comma item
— outside that join — is allowed (`FROM c, a RIGHT JOIN (b JOIN LATERAL (SELECT c.z) s ON true) ON
true`), and a `RIGHT JOIN LATERAL` *inside* the group may reference a relation to the group's left
(`FROM a JOIN (b RIGHT JOIN LATERAL (SELECT a.x) s ON true) ON true`). A group whose lateral items
reference only an enclosing query level is not correlated to its left and joins under any join type.
Because the relations to the group's left and those inside it are **one** FROM namespace, a bare
column that one of each carries is ambiguous (`42702`) from a lateral item inside the group — not
shadowed by the inner relation, though jed plans the group's body as its own level — and the outer
relation is checked first, so one refused by a `RIGHT`/`FULL` join is `42P10`.

- **Unaliased**, the group is one join-tree node whose inner relation labels stay visible: `a.x`,
  `b.*`, `WHERE`/`GROUP BY`/`ORDER BY`, a correlated subquery, and a later `LATERAL` item all reach
  them, and they take part in the FROM duplicate-label check (`FROM a JOIN (a JOIN b ON …) ON …` is
  `42712`). Its `*` contribution is the inner join's own `*` (its USING/NATURAL merges first), placed
  at the group's position; an outer `USING`/`NATURAL` sees the group's merged column as one column.
- **Aliased**, `(a JOIN b ON p) AS g` **is** the derived table `(SELECT * FROM a JOIN b ON p) AS g`
  (§42): the inner labels are hidden (`a.x` is `42P01`, so an inner label may repeat an outer one),
  `g.*` is the join's `*`, `AS g(c1, …)` renames left to right, and a name the join carries twice is
  ambiguous through the alias (`g.id` is `42702`, like any duplicated derived-table column).

*Mechanics.* The parser builds the group's body as a `SELECT *` over its chain. A group that
**begins** a chain — the first FROM item, the item after a comma, or the first item of an enclosing
group — is the same left-deep chain without its parentheses (the same tree and the same `ON` scopes),
so an unaliased one is **spliced** into the enclosing chain and the planner sees an ordinary chain —
except that a later comma item whose (spliced) chain holds a `RIGHT` or `FULL` join is made a group
again (comma-`FROM`, above).
Any other unaliased group is an inline derived body (planned against the enclosing query's scope, like
§42 — so its `ON` clauses may correlate to an enclosing query level, never to a sibling) marked as a join
group: after its `*` columns it emits every slot the `*` omitted — the USING/NATURAL copies a merge
superseded — so each inner relation is complete, and it records where each inner label's columns
landed. The enclosing scope carries those labels as **qualifier-only views** over the group's slots
(never physical relations; a partial `ON` scope sees only the views of the relations it spans), marks
the extra copies hidden (so `*` and bare resolution skip them, while `b.id` still reads `b`'s own
column — NULL under a LEFT join where the merged `id` is `a`'s), and keeps each inner column's
collation. Being a derived input, a right-hand group is a join-search fence; its body plans its own
join.

*Lateral fence.* A right-hand group that holds a lateral-capable item (a `LATERAL` derived table, a
table function, or a nested group holding one) is planned against a **lateral fence**: an empty scope
at the correlation level of the relations to its left, so the group's `ON` clauses and non-`LATERAL`
items see through it to the enclosing query only, carrying a view of those relations — the same prefix
scope a top-level `LATERAL` item resolves against — which the group's own `LATERAL` items and table
functions (including a first item, which has no sibling) resolve their outer references through. Only
the group's body reads the view; a derived table planned against the fence strips it, so a `LATERAL`
item nested in a non-`LATERAL` subquery cannot reach past the subquery. The body is first planned as a
shape (planner.md §3.2) to learn whether any reference reached the prefix. If one did, the group is a
**correlated lateral relation** (exactly as a top-level `LATERAL` item: re-materialized per combined
left row, never pushed into, a join-search fence) planned for real against the fence; if none did, it
is planned as an ordinary derived body, so its cost and plan are those of the group without the
fence. *(Fixed with this slice: a `USING` predicate binds the two chosen columns directly through
one-column pseudo relations at their flat slots, so a side that is an unlabeled relation — an
unaliased derived table or join group — no longer resolves to the first unlabeled relation.)*

**Deliberate narrowings (each relaxable later, [../../TODO.md](../../TODO.md)).**

- **No `FULL JOIN ... USING`** / `NATURAL FULL JOIN` (its `COALESCE` merge — above). A **derived
  table** (`FROM (SELECT …) AS t`) and a **parenthesized join** are supported (§42, above); the
  **`t.*`** qualified star, **`USING`**, and **`NATURAL`** have landed (above).
- ~~**`((SELECT …)) AS s`**~~ — ✅ **landed** with parenthesized queries (§25): redundant
  parentheses around a derived-table subquery make its body a parenthesized query.
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

**`DEFAULT VALUES`** is the all-columns-omitted INSERT source: `INSERT INTO t DEFAULT VALUES`
inserts exactly one row, applying every column's default else NULL. It takes the same two-phase
validation path as a one-row VALUES insert and may be followed by `ON CONFLICT` and `RETURNING`.
Matching PostgreSQL, it cannot be combined with an explicit column list or `OVERRIDING` (`42601`).

The UPDATE assignment form likewise recognizes `UPDATE t SET x = DEFAULT`, and so does the
`ON CONFLICT … DO UPDATE SET` action (one shared `update_assignment` production): the RHS is the
target column's declared default else NULL, applied per updated row. `DEFAULT` is recognized
positionally after `=` **only when it is the complete assignment RHS** — the next token is `,`,
`)` (the end of a writable-CTE body), `WHERE`, `RETURNING`, or the end of the statement; it remains
non-reserved elsewhere, so `SET x = default + 1` is an ordinary expression referencing a column
named `default`. The semantics and cost are in [constraints.md](constraints.md) §2 and, for the
conflict action, [upsert.md](upsert.md) §5.

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

### General expressions in a value slot

A `VALUES` slot may hold any expression: `INSERT INTO t VALUES (1 + 1, upper('x'))`,
`VALUES (now())`, `VALUES (CASE WHEN … END)`, `VALUES ((SELECT max(id) FROM t) + 1)`. The grammar's
`insert_value` is `"DEFAULT" | expr`, with one parse rule that keeps the literal forms' meaning:

- **Literal-shaped slots keep contextual adaptation.** A slot that is *exactly* a `literal`
  (including the folded `-5`), a `parameter`, `DEFAULT`, or a `ROW(…)` / `ARRAY[…]` whose fields are
  themselves only those forms — i.e. the token after it closes the slot (`,` or `)`) — is parsed and
  materialized as before: a string literal adapts to the column type (`'2024-01-01'` into a `date`,
  `'{1,2}'` into an array, `'[1,5)'` into a range, `'(Main,1)'` into a composite via `record_in`,
  `'today'` per date.md §6), a bare `NULL` is the column type's NULL (a composite's too), a `ROW(…)` materializes
  field by field against a composite column, and a bare `$N` takes its target column's type. Any
  other slot re-parses from its start as a general `expr`. A constructor with an expression field
  (`ARRAY[1 + 1, 3]`) is therefore one general expression, not a literal constructor. A non-syntax
  error from the literal form (an out-of-range integer, `22003`; the nesting limit, `54001`, which
  also bounds literal-field constructors) is the slot's own error.
- **Resolution.** An expression slot resolves against a scope with **no columns** (a column
  reference is `42703`) that admits **subqueries** and sees the statement's CTEs; an aggregate
  outside a subquery is `42803`, as in PostgreSQL. The target column's type is
  the literal hint, exactly as for `UPDATE … SET` (§12/decimal.md §6), so `100 + 23` into an `i16`
  column types as `i16`. The result must be **assignable** to the column — the scalar gate
  `INSERT ... SELECT` and `UPDATE` use (`42804`); a range/array column follows `UPDATE`'s container
  rule (a NULL, or the same container type — array.md §4 / ranges.md §4); a composite column takes
  NULL or a value of its own composite type (e.g. a scalar subquery over a composite column), as
  `UPDATE` does. An anonymous `ROW(…)` *expression* into a composite column is `0A000`, the same
  deferred anonymous-record → named-composite assignment `UPDATE` carries (composite.md §12) — a documented
  narrowing (PostgreSQL accepts it); a literal-field `ROW(…)` is unaffected. A `$N` inside an
  expression is typed by its operator context (api.md §5), not by the column. Slots are resolved
  row by row, left to right, before `RETURNING` and `ON CONFLICT` (PostgreSQL's analysis order), so
  every resolution error precedes the `428C9` GENERATED ALWAYS gate — an expression is an explicit
  value there, like a literal.
- **Evaluation.** Uncorrelated subqueries fold **once**, after parameters bind and before any row is
  materialized (cost.md §3); they read the pre-statement snapshot, like an `INSERT ... SELECT`
  source. Each expression slot then evaluates **per row** during phase one — rows in `VALUES` order,
  slots in column declaration order, interleaved with `DEFAULT`-keyword expression defaults — so a
  volatile call (`nextval`, `uuidv4()`) advances per row and every evaluation error (`22012`, `21000`)
  aborts before any write. `RETURNING` subqueries fold only after every row materializes, so a
  VALUES error precedes theirs (PostgreSQL's order). The value is then store-coerced like any other (`22003`, `22001`, `23502`,
  CHECK `23514`).
- **Cost and privileges.** An expression slot accrues its evaluated tree (`operator_eval` per node
  plus its functions' own units) per row; literal slots stay free. The statement needs `SELECT` on
  any table a slot's subquery reads and `EXECUTE` on the functions it calls, in addition to
  `INSERT` (session.md §5.3). A prepared `INSERT … VALUES` with expression slots caches its resolved
  plan like a literal-only one unless a slot holds a subquery (which folds per execution).

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
(`round(x)`, `round(x, 2)`). **`DISTINCT` inside the parens** (`COUNT(DISTINCT x)`) folds
only the distinct argument values ([aggregates.md](aggregates.md) §5): the parser accepts a
leading `DISTINCT` (an argument is required, `DISTINCT )` is `42601`; it may not combine with
`*`, `DISTINCT *` is `42601`), and the resolver rejects it on a non-aggregate (`42809`) or a
window function (`0A000`).

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

**The `WITHIN GROUP (ORDER BY …)` clause (ordered-set aggregates).** An **ordered-set aggregate**
— `mode`, `percentile_cont`, `percentile_disc` ([aggregates.md](aggregates.md) §13) — attaches a
`within_group_clause ::= "WITHIN" "GROUP" "(" "ORDER" "BY" sort_key ")"` between the argument list
and any `FILTER` / `OVER`. The parenthesized argument list is the per-group **direct** argument
(the percentile fraction; empty for `mode`); the `WITHIN GROUP` order key is the **aggregated**
argument (the value sorted over). Parser rules, byte-identical across the three cores:

1. **One sort key — column or general expression.** `WITHIN GROUP`'s `sort_key` is a single key
   parsed as a general expression with the ordinary `ASC`/`DESC` / `NULLS FIRST|LAST` suffix — the
   same general-expression key as the query `ORDER BY` (§10, `query.within_group_expr`): a
   bare/qualified column stays a column key (a leaf), any other expression (`a + b`, `abs(x)`) is the
   aggregate's per-row operand. The one PG divergence from the query `ORDER BY` is the bare-integer
   case: there an integer is an **ordinal**, but in `WITHIN GROUP` it is a **constant** expression
   (PostgreSQL), so `WITHIN GROUP (ORDER BY 1)` sorts every row equal rather than by output column 1.
   The clause carries a `Some(sort_key)` onto the `FuncCall` node (an ordinary call carries `None` and
   parses byte-identically to before); the one-key restriction, the per-name validity, and every error
   below are resolve-time concerns driven by [aggregates.md](aggregates.md) §13. `WITHIN`/`GROUP` are
   recognized positionally, right after the call's `)`, so they stay non-reserved.
2. **Resolve-time meaning.** A second sort key, `WITHIN GROUP` on a non-ordered-set function, and
   an ordered-set aggregate **without** `WITHIN GROUP` are all **`42883`**; `OVER` on an
   ordered-set aggregate is **`0A000`**; an out-of-range percentile fraction is **`22003`**; an
   aggregate inside the `WITHIN GROUP` order key is **`42803`** — see [aggregates.md](aggregates.md)
   §13.

**The in-aggregate `ORDER BY`.** The argument list of an aggregate call may end in `ORDER BY key [,
…]` (`agg_order_clause`, `json_agg(x ORDER BY y DESC)`, `jsonb_object_agg(k, v ORDER BY ts)`) — the
order the aggregate folds its input rows in ([aggregates.md](aggregates.md) §22). Parser rules,
byte-identical across the three cores:

1. **After the last regular argument.** `ORDER` (reserved) right after an argument — instead of the
   `,` or `)` — starts the clause; it ends the argument list (the next token must be `)`). It needs a
   regular argument first: `f(ORDER BY x)` and `count(* ORDER BY x)` are `42601`. It may follow a
   `VARIADIC` or named argument and combine with a leading `DISTINCT`.
2. **Keys.** Each key is an `order_term` — a general expression with the `COLLATE` / `ASC`|`DESC` /
   `NULLS FIRST|LAST` suffix — parsed with ordinals OFF: a bare integer is a constant key (PostgreSQL),
   as in `WITHIN GROUP`. The keys ride on the call node (`agg_order`); an ordinary call carries none and
   parses byte-identically to before.
3. **Not with `WITHIN GROUP`.** A call carrying both is `42601` (*"cannot use multiple ORDER BY
   clauses with WITHIN GROUP"*), raised by the parser as PostgreSQL's grammar does.
4. **Resolve-time meaning.** On a non-aggregate `42809`, on a window call `0A000`, a `json` key `42883`,
   an aggregate or window function in a key `42803`, under `DISTINCT` a key that is not an argument
   `42P10` — see [aggregates.md](aggregates.md) §22.

## 18. `GROUP BY`

`group_by ::= "GROUP" "BY" group_item ("," group_item)*` slots between `WHERE` and
`ORDER BY`. It partitions the post-`WHERE` rows into groups sharing a value on every grouping
key and produces **one result row per group**; the select list then projects the grouping
keys and aggregates over each group. Semantics live in [aggregates.md](aggregates.md) §5–§6;
this section is the syntax + the two narrowings.

**Grouping sets.** A `group_item` is an ordinary column group, or `ROLLUP (...)` / `CUBE (...)` /
`GROUPING SETS (...)` — the forms that name **several** grouping sets in one `GROUP BY`
([aggregates.md](aggregates.md) §12). `ROLLUP`/`CUBE` are sugar (subtotal prefixes / every subset);
a plain term cross-products with them. The `GROUPING(col, …)` function (which reports a row's
grouping set as a bitmask) is an ordinary `function_call`, valid only in a grouped query. The
no-grouping-sets case below is the single-set special case.

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
  the `n−1` `or` nodes (+1 `not` for `NOT IN`) always, and the `eq` nodes **up to the first TRUE
  one**: the left-deep OR-chain short-circuits left to right, so later elements are neither
  compared nor charged (and an element expression past the match is never evaluated, so it
  cannot trap). Because the OR-chain re-uses `x` in every comparison, **the LHS is evaluated
  once per element compared** (a deliberate consequence of the desugar model); for a bare-column
  `x` that is a free leaf, so the cost is just the comparison/connective nodes. Output column name for a bare `SELECT x IN (…)` is the fixed
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
  `and` + 2 `compare`), but the short-circuit `AND` skips the `<=` (and its `hi` bound) when
  `x >= lo` is FALSE; **the LHS is evaluated up to twice** (once per bound compared — the
  desugar consequence). Output name for a bare `SELECT x BETWEEN …` is `?column?` (§8).

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
  NULL subject is still NULL, not an error — verified against PostgreSQL 18). A bind parameter
  operand adopts `text`, the only operand type `LIKE` declares, whatever its sibling: `'x%' LIKE
  $1` and `$1 LIKE $2` bind text, as in PostgreSQL ([api.md](api.md) §5).
- **Wildcards and the default `\` escape.** `%` = any run, `_` = one character. The default
  escape character is **backslash** (PostgreSQL's default): `\%`, `\_`, and `\\` match a
  literal `%`, `_`, and `\`; a `\` before any other character matches that character literally
  (`'a' LIKE '\a'` is TRUE). String literals have no backslash escapes
  (`standard_conforming_strings`, §3 / [types.md](types.md) §11), so a `\` written in a
  pattern literal is a literal backslash byte the matcher then interprets. `SIMILAR TO` is
  excluded (regex.md).
- **`ESCAPE e` — choosing the escape character.** `s [NOT] LIKE p ESCAPE e` (and `ILIKE`) matches
  with `e` as the escape character instead of `\`. The parser **desugars** it exactly as
  PostgreSQL does: `s LIKE like_escape(p, e)`, where the built-in `like_escape(text, text) → text`
  ([string-functions.md](string-functions.md) §3) rewrites `p` into the default backslash form. So
  the clause adds no operator node, and every rule below follows from ordinary function evaluation
  (all verified against PostgreSQL 18):
  - `e` is any text expression (a column works); a non-text `e` is **42883** (no `like_escape`
    function for those argument types — PostgreSQL's code). A bare `NULL` or `$N` pattern or escape
    is typed `text`, the type `like_escape` declares, by the general rule for an unknown-typed
    function argument ([functions.md](functions.md) §9.1) — so `s LIKE $1 ESCAPE '#'` and
    `s LIKE 'a#%' ESCAPE $1` bind like `s LIKE $1`, and `'a' LIKE NULL ESCAPE '#'` is NULL like
    `'a' LIKE NULL`.
  - `e = ''` turns escaping **off**: `\` is an ordinary character (`'a\b' LIKE 'a\b' ESCAPE ''` is
    TRUE). Once any other escape is chosen, `\` is ordinary too (`'a\b' LIKE 'a\b' ESCAPE '#'` is
    TRUE). The escape may be a wildcard: `'a%' LIKE 'a%%' ESCAPE '%'` is TRUE.
  - `e` of two or more **code points** is **22025** *invalid escape string*. One code point is one
    character, so `ESCAPE '😀'` is valid (the TS core counts code points, never `.length`).
  - **NULL and error order.** `like_escape` is strict and is evaluated as the pattern operand,
    *before* LIKE's NULL check — so a NULL pattern or a NULL escape yields NULL with no error, but a
    NULL **subject** does not suppress an invalid escape (`NULL LIKE 'a' ESCAPE 'xy'` is 22025).
  - The rewritten pattern keeps a trailing escape as a trailing `\`, so the lazy trailing-escape
    22025 below is unchanged: `'ax' LIKE 'a#' ESCAPE '#'` traps, `'a' LIKE 'a#' ESCAPE '#'` is FALSE.
  - **`ILIKE`:** the escape is recognized on the pattern **before** case-folding (the rewrite
    precedes the fold), so it is case-sensitive: `'ax' ILIKE 'ax' ESCAPE 'X'` is TRUE (`x` is not
    the escape), `'a%' ILIKE 'aX%' ESCAPE 'X'` is TRUE.
  - **No constant folding.** jed folds no general expression (planner.md §3), so an invalid
    constant escape is reported when the LIKE is first evaluated: over zero rows it raises nothing,
    where PostgreSQL folds `like_escape` of two constants at plan time and raises 22025 — the
    same class as `SELECT 1/0 FROM empty_table`.
  - **Cost:** the `like_escape` call is an ordinary scalar function (one `operator_eval`) and a
    growth kernel ([cost.md](cost.md) §8.1 — it can double backslashes, and nests), so a
    non-constant one also charges `scalar_byte` per result byte; a constant pattern + escape
    charges nothing extra.
- **`ILIKE` — case-insensitive `LIKE`** (collation Slice 3e). `x ILIKE p` / `x NOT ILIKE p` is `LIKE`
  with both operands **simple-lowercased** first — ASCII-only when no Unicode property bundle is
  loaded, full simple (1:1) Unicode mappings when one is ([collation.md §16](collation.md)). Simple
  1:1 folding (never the expanding `ß`→`SS` form) keeps `_`/length semantics intact. Same precedence,
  NULL propagation, `ESCAPE` clause, `22025` trailing-escape, and non-text `42804` as `LIKE`.
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

## 22b. `~` / `~*` / `!~` / `!~*` — regular-expression match

`s ~ pattern` is the **regular-expression match**: TRUE iff `pattern` matches somewhere in subject
`s`. It extends `comparison` with `regex_op concat` (precedence 35, the LIKE slot), where
`regex_op` is one of `~` `~*` `!~` `!~*`. The flavor is jed's own **RE2-able subset**
([regex.md](regex.md)), deliberately **not** PostgreSQL-compatible — the *overriding reason* is
determinism + untrusted-query safety (the engine is a linear-time **Pike VM**, immune to ReDoS,
[regex.md](regex.md) §4/§6). `SIMILAR TO`, backreferences, and lookaround are excluded.

- **Punctuation operators, two base + two negations.** `~` is case-sensitive match, `~*`
  case-insensitive (two `[[operator]]` rows, `regex_match`/`regex_imatch`, symbols `~`/`~*`).
  `!~`/`!~*` are their **negations**, carried on the AST `Regex.negated` flag (the `NOT LIKE`
  precedent — no separate row, and no `NOT ~` keyword spelling). Each is a genuine operator with a
  dedicated resolved node (`Regex`) and the hand-written matcher, like LIKE — *not* desugared.
- **Unanchored — matches a SUBSTRING.** Unlike `LIKE` (whole-subject), `s ~ p` is TRUE iff `p`
  matches any substring of `s`: `'abc' ~ 'b'` is TRUE, `'abc' ~ '^b'` FALSE. Anchor with `^`/`$`
  for whole-string. (Implemented by an implicit lazy `.*?` program prefix, regex.md §3.2.)
- **Text only / NULL propagates.** Both operands must be `text`; a non-text operand is **42804**
  (`compare.toml` lists only text×text). NULL on either side yields NULL (`null = "propagates"`),
  short-circuiting **before** the matcher — so a malformed pattern against a NULL subject is NULL,
  not an error (the LIKE rule). A bind parameter operand adopts `text`, as for LIKE.
- **`~*` — case-insensitive** via the **ILIKE mechanism** ([collation.md §16](collation.md)): both
  operands are simple-lowercased (1:1; ASCII baseline without a Unicode property bundle, full simple
  mappings with one) before compiling/matching. So `'FOO' ~* 'f.o'` is TRUE.
- **Code-point matching — a §8 determinism surface.** `.`, classes, and quantifiers count one
  **Unicode code point** (not byte, not UTF-16 unit) — every core iterates by code point (Rust
  `chars()`, Go `[]rune`, **TS `Array.from`/spread**), pinned by an astral-character conformance case.
- **Pattern grammar (the RE2 subset, regex.md §2):** literals, `.` (any code point except `\n`),
  classes `[...]`/`[^...]`, `\d \w \s` (+ negations), anchors `^ $`, alternation `|`, groups
  `(...)`/`(?:...)`, quantifiers `* + ? {n} {n,} {n,m}` (greedy + lazy `?`-suffixed), with
  leftmost-first greedy semantics. Excluded (each handled per regex.md §2): backreferences,
  lookaround, named groups, inline flags.
- **Errors.** A **malformed** pattern (unbalanced `(`/`[`, trailing `\`, unknown escape, bare
  quantifier, `{n,m}` with `m<n`) is **2201B** (`invalid_regular_expression`); a **well-formed but
  too-large** compiled program (> `MAX_REGEX_PROGRAM`, 32768 instrs after `{n,m}` unroll) is
  **54001** (`statement_too_complex`, regex.md §6). For a constant pattern both surface at resolve;
  for a per-row pattern, at eval.
- **Lexer note.** `~*`/`!~*` are single tokens; the `!` arm must match `!~`/`!~*` *before* the
  lone-`!` error and the `!=`→`<>` fold (regex.md, grammar.ebnf `regex_op`).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the `Regex` node, plus `regex_compile`
  (`|program|`, once for a constant pattern — §5) and `regex_step` (per Pike-VM thread-step) — the
  per-step work LIKE's matcher leaves unmetered (regex is program × input, LIKE input alone). Output
  name for a bare `SELECT s ~ …` is `?column?` (§8).

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
- **Lazy first-match evaluation — a sanctioned short-circuit (like `AND`/`OR` and `COALESCE`,
  §51).** Conditions are evaluated in source
  order and evaluation **stops at the first TRUE** branch, returning that `THEN`. A FALSE or
  NULL/UNKNOWN condition falls through (a NULL `WHEN` is *not* true — like `WHERE`). With no
  matching branch, the `ELSE` result is returned, or **NULL** if there is no `ELSE` (an implicit
  `ELSE NULL`). Later arms are **never evaluated**, so `CASE WHEN a = 0 THEN 0 ELSE 1 / a END`
  does not divide by zero on the `a = 0` rows — one of the sanctioned short-circuits of the
  cost rules ([cost.md](cost.md) §3), and it stays deterministic because the order is fixed.
- **Result-arm type unification.** The `THEN` results and the `ELSE` (or NULL for an implicit
  ELSE) unify to one **common type** — the CASE's output type — by folding the **common-type rule
  of §25** (the same function set operations and `VALUES` use). NULL-typed arms are dropped (they
  adapt); an **all-NULL CASE is `text`** (PostgreSQL — verified against the live oracle); numerics
  promote (decimal if any arm is decimal, else the widest integer), and an integer result widens
  to decimal at eval when the common type is decimal (so `CASE WHEN c THEN 1 ELSE 1.5 END` renders
  `1` / `1.5`); mixed float widths unify to `f64`, each `f32` arm wrapped in an ordinary `f32 →
  f64` cast (as `GREATEST`, §52); any other arms must be **identical types** — and identical types
  always unify (`json`, `jsonpath`, ranges, arrays, composites, …). A pair with no common type —
  e.g. an integer `THEN` and a text `ELSE`, or `i32range` and `i64range` — is **`42804`** ("CASE
  types … cannot be matched"). Bare integer-literal arms keep their natural width (defaulting to i64), so width
  differences from PostgreSQL are unobservable (every integer renders under the `I` tag). A bare
  `$N` result arm adopts the unified scalar result type before parameters are bound (§5).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the CASE node, plus the
  `operator_eval`s of the conditions tested up to the match and of the selected result only
  (the lazy-eval exception). Output name for a bare `SELECT CASE … END` is `?column?` (§8) —
  any non-column expression.

## 24. `INSERT ... SELECT`

`INSERT` may take its rows from a **query** instead of a `VALUES` list: the `insert`
production's source is now `( "VALUES" row ("," row)* | subquery_expr )`. `INSERT INTO dst SELECT a, b
FROM src WHERE …` inserts whatever the embedded query produces. The whole query surface
is reachable as a source — `WHERE`, `JOIN`, `GROUP BY`/`HAVING`, `DISTINCT`, `ORDER BY`,
`LIMIT`/`OFFSET`, aggregates, `CASE` — because the source *is* a query, parsed and executed
by the same path as a top-level query.

**A set operation, a parenthesized query, or a `WITH` prefix is a source too** (PostgreSQL's
`SelectStmt`): `INSERT INTO t SELECT … UNION [ALL] SELECT …` (likewise `INTERSECT`/`EXCEPT`, with
§25's precedence), parenthesized operands with their own `ORDER BY`/`LIMIT`, a trailing `ORDER BY`/
`LIMIT`/`OFFSET` over the combined result, a parenthesized whole source (`INSERT INTO t (SELECT …)
…`, `INSERT INTO t (a, b) (SELECT … EXCEPT SELECT …)`), and `INSERT INTO t WITH w AS (…) SELECT …`
(a **nested** `WITH`, cte.md §7 — so a data-modifying CTE there is `0A000`, PostgreSQL's "must be
at the top level", while the statement-level `WITH … INSERT` of writable-cte.md stays the place for
one). The source is planned as the ordinary set-operation plan — no special case — so its output
column types are §25's per-position unified types, and those are what the assignability gate below
checks (`SELECT 1 UNION SELECT 3000000000` is `i64`, assignable to an `i32` target, and then traps
`22003` per row). An all-`NULL` set-op column stays untyped (§25) and so is assignable to any
target — PostgreSQL resolves it to `text` first and rejects a non-text target (`42804`); jed's
untyped-NULL rule makes the difference observable here (ledgered). Right after the target, a `(`
is ambiguous between the column list and a parenthesized source; it **begins the query** when it is
followed by another `(`, a `WITH` clause, `VALUES (`, or a bare `SELECT` that is not itself a listed
column name — keywords are not reserved (§3), so `(select)` and `(select, b)` stay column lists
(PostgreSQL reserves `SELECT` and rejects both; ledgered). The optional **column list** and
**`DEFAULT`-for-omitted columns** (§16) apply unchanged; a `DEFAULT` *keyword* value slot is a
`VALUES`-only thing and does not exist in the SELECT source.

**`VALUES` — the INSERT's own list, or a `VALUES` query (PostgreSQL's `transformInsertStmt`).** A
`VALUES` row list that stands alone — also wrapped in any number of parentheses, `INSERT INTO t
(VALUES (1, DEFAULT))` — is the INSERT's own `VALUES` source (§12/§16): its slots take `DEFAULT` and
adapt to their target columns, at its own cost. When a set operator or a trailing `ORDER BY` /
`LIMIT` / `OFFSET` follows the list (inside or outside those parentheses), the whole source is
instead a query whose operand is a **`VALUES` query** (§25): `INSERT INTO t VALUES (1, 'a') UNION
ALL SELECT 2, 'b'`, `INSERT INTO t VALUES (2, 'b'), (1, 'a') ORDER BY 1 LIMIT 1`, `INSERT INTO t
(VALUES (1, 'a')) UNION (VALUES (2, 'b'))`, and a `WITH`-prefixed `VALUES` main query. Such a list's
columns are typed by the `VALUES` relation's own rules — a string or all-`NULL` column is `text` — and
then gated by the assignability check below like any query column (`INSERT INTO t (i32_col) VALUES
(NULL) UNION ALL SELECT NULL` is `42804`, as in PostgreSQL), and a `DEFAULT` in it is `42601`
("DEFAULT is not allowed in this context"). The parser decides by parsing the list as the INSERT's
own and rewinding when a set operator or trailing clause follows.

**Arity — the query's output column count must match the target**, exactly as a `VALUES`
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
projection is assignable to an integer column of **any** width — a narrowing (`i64` into `i32`,
say) is an assignment cast that traps `22003` per row when the value does not fit, as in
PostgreSQL ([types.md](types.md) §5) — **or** to a decimal column (int→decimal widens); a
decimal only to a decimal column (decimal→int is explicit-`CAST` only); `f32` to an `f32` or
`f64` column and `f64` only to `f64` (an integer or decimal never assigns to a float column); a
text projection only to a text column; `jsonb` to `jsonb` or
`json` (the `jsonb → json` assignment cast, [json.md](json.md) §6.1); every other scalar only to
its own type; and a **NULL-typed** projection to **any** column (a `NOT NULL` target then traps
`23502` per row, if any). A column the list omits is not type-checked — it takes its default
else NULL.

**A bare string literal in the select list adapts.** PostgreSQL leaves a string literal `unknown`
until it meets the target column, so `INSERT INTO t (d) SELECT '2024-01-01'` stores a date and
`INSERT INTO t (u) SELECT id, '…'` a uuid. jed admits such a literal into a uuid, bytea, date,
timestamp, timestamptz, or interval column, the per-row store parsing it (`22P02` / `22007` /
`22008` on malformed input) — under the same conditions PostgreSQL leaves it untyped: the item of a
plain `SELECT` list (WHERE, GROUP BY / ORDER BY on other columns, LIMIT, a nested WITH, an alias,
and parentheses are all fine). A set operation, a `VALUES` query, `SELECT DISTINCT`, a derived table
or CTE column, or an ORDER BY / GROUP BY key that names the item by ordinal or alias resolves the
literal to `text` first, and then the text gate above makes it `42804`. A text-typed *expression* —
a text column, `||`, `CAST(… AS text)`, a scalar subquery, `CASE` / `COALESCE` — is text, and is
`42804` into any non-text column, as in PostgreSQL (`spec/conformance/suites/dml/text_storage_assign.test`).
A numeric or boolean target does not take the literal (`42804`, where PostgreSQL parses it —
[types.md](types.md) §5's "a bare string never becomes a number").

**A container column** — an array, a range, or a composite — takes a projection of **exactly its
own type** or a NULL-typed one, the container rule `UPDATE` and an `INSERT … VALUES` expression
already share ([array.md](array.md) §4, [ranges.md](ranges.md) §4): a different element, range, or
composite type is `42804` (PostgreSQL would assignment-cast — e.g. a bare `ARRAY[1]`, which is `i64[]`
in a select list, into an `i32[]` column; ledgered), and an anonymous `ROW(…)` projection into a
composite column is `0A000`. The values are stored through the same container codec as `INSERT …
VALUES`. (Formerly every container column was `0A000` here; `CREATE TABLE … AS` fills its container
columns through this path — [create-table-as.md](create-table-as.md) §3.)

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

**Cost = the embedded query's accrued cost** ([cost.md](cost.md)) — for a set operation its
`lhs + rhs` (§25). The `SELECT` already
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
it — byte-for-byte the pre-set-operations `SELECT`. An operand may also be a **parenthesized
query** (below), which is how an operand carries its own `ORDER BY`/`LIMIT`/`OFFSET`. `UNION`/`INTERSECT`/`EXCEPT`/`ALL`/`DISTINCT`
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

**Column types — unified per position, the ONE common-type rule.** Each output column's type is the
fold of the operands' types at that position under a single pairwise **common-type** rule. The same
rule — the same per-core function — also unifies a `VALUES`-body column across rows (§42), a `CASE`'s
result arms (§23), and a `COALESCE`/`GREATEST`/`LEAST` argument list (§51/§52), so the constructs
cannot drift apart. The pair `(X, Y)` unifies to:

- **`Y` when `X` is `NULL`-typed** (and vice versa): a `NULL` alongside a concrete type takes the
  concrete type;
- integer widths **promoted** to the widest (`i16` < `i32` < `i64`), and integer with `decimal` →
  **`decimal`** (oracle: `int2 ∪ int4` → `integer`, `int4 ∪ int8` → `bigint`, `int ∪ numeric` →
  `numeric`);
- float widths **promoted** to the widest: `f32 ∪ f64` → **`f64`** (oracle: `float4 ∪ float8` →
  `double precision`). Float never meets integer/`decimal` — that pair stays `42804`, the float
  island ([float.md](float.md) §6; PostgreSQL would promote to `double precision`);
- two **arrays** element-wise, when their elements differ **only in integer width** (`i32[] ∪
  i64[]` → `i64[]`, a value no-op);
- two **anonymous records** (`ROW(…)` results) of the same arity field-wise, by the same
  integer-width rule (`ROW(1::i32) ∪ ROW(2::i64)` → a record of `i64`, a value no-op —
  [composite.md](composite.md) §13);
- otherwise **the type itself, when `X` and `Y` are identical** — structurally equal: the same
  scalar, the same element for an array/range, the same named composite (or the same anonymous
  shape). **Identical types always unify, whatever the family**: `json`, `jsonb`, `jsonpath`,
  `date`, `interval`, `f32`, arrays (`json[]` included), every range, every composite (oracle: each
  such pair is accepted by PostgreSQL in `VALUES`, `UNION ALL`, `CASE`, and `COALESCE`);
- any other pairing is **`42804`** (PostgreSQL: "UNION types `X` and `Y` cannot be matched").
  `json ∪ jsonb`, `i32range ∪ i64range`, `i32[] ∪ text[]`, two different named composites, and a
  named composite with an anonymous `ROW(…)` are all `42804` — PostgreSQL rejects each too, as
  `42846` "could not convert type" for the same-category pairs (jed's standing `42804` convention for
  an unconvertible pair — types.md §9).

A column that is **`NULL`-typed in every operand** stays untyped in a set operation (PostgreSQL
resolves it to `text` — the type is not observed in output, see `setops/types.test`); an all-`NULL`
`CASE`/`COALESCE`/`GREATEST`/`LEAST` is `text`.

**Deliberate narrowing — element-wise array widening that rewrites elements.** PostgreSQL unifies
array element types by its full coercion lattice (`int[] ∪ numeric[]` → `numeric[]`, `float4[] ∪
float8[]` → `float8[]`). jed unifies array elements only across integer widths, the one widening
that leaves every element value untouched; a pair whose unification would convert each element
(int → `decimal`, `f32` → `f64`, inside an array or a composite field) is `42804` — deferred, not
foreclosed (recorded in the override ledger). Likewise an anonymous `ROW(…)` unifies only with an
anonymous `ROW(…)` of the same arity whose fields differ at most in integer width. PostgreSQL types
every anonymous record as `record` and accepts any mix, keeping each row's own field types — so it
then cannot compare two rows whose field types differ (`42804` *cannot compare dissimilar column
types*), where jed's widened column compares.

**Matching needs equality.** Unifying is not matching: every set operation except `UNION ALL` —
`UNION`, `INTERSECT`, `EXCEPT`, `INTERSECT ALL`, `EXCEPT ALL`, and a `UNION` recursive CTE
([recursive-cte.md](recursive-cte.md)) — compares rows, so each unified column type must have an
**equality operator**. `json` and `jsonpath` have none (PostgreSQL ships no btree/hash operator class
— [json.md](json.md) §5), nor does an array or composite that contains one, so such a column is
**`42883`** ("could not identify an equality operator for type json" — PostgreSQL's code and
wording), decided by the comparability matrix. `UNION ALL`, `VALUES`, `CASE`, and `COALESCE` never
compare, so `json` flows through them freely.

**Value conversion.** When the unified type is `decimal`, an integer operand's **values are
converted** to `decimal` (scale 0), and when it is `f64` an `f32` operand's values are widened
exactly to `f64` (oracle: `SELECT 1.1::float4 UNION ALL SELECT 2.5::float8` → `1.100000023841858`)
— both *before* rows are matched — this is load-bearing for correctness, not just for the
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
"missing FROM-clause entry"; the engine reports `42P01`/undefined). An **ordinal** (`ORDER BY 1`)
names an output column by position (`42P10` out of range — §10). Direction and
`NULLS FIRST|LAST` work exactly as §10 (the same `key_cmp` over the output-row value). `LIMIT`/
`OFFSET` then window the ordered result (§9). Output order **without** a trailing `ORDER BY` is
unspecified (CLAUDE.md §8/§10; the corpus compares such queries `rowsort`); the result *multiset*
is exact and identical across cores regardless.

**Parenthesized queries.** A set-operation operand may be a parenthesized query — `(SELECT …)
UNION (SELECT …)` — whose body is a full `subquery_expr`: its own nested `WITH`, set operations
(so parentheses override precedence: `(a UNION b) INTERSECT c`), and `ORDER BY`/`LIMIT`/`OFFSET`,
which apply to **that operand** before the combine (oracle: `(SELECT a FROM t ORDER BY a LIMIT 2)
UNION ALL (SELECT a FROM u ORDER BY a DESC LIMIT 1)`). The same form is accepted wherever a query
expression is: a whole statement (`(SELECT a FROM t) ORDER BY b`), the main query of a `WITH`, a CTE
or view body, a derived table, a scalar / `IN` / `EXISTS` / `ANY`/`ALL` subquery, and after
`EXPLAIN`. Each `(` is one parser nesting level (`54001` past `MAX_EXPR_DEPTH`, [cost.md](cost.md)
§7). A bare `select_core` operand still takes no trailing clause: `SELECT … ORDER BY a UNION …`
leaves the `UNION` as a leftover token, `42601`, as in PostgreSQL.

- **A parenthesized query is its contents (PostgreSQL).** The parentheses add no query level, so
  trailing clauses written after the `)` **fold** onto the inner query: a clause it lacks is added
  (`(SELECT a FROM t) ORDER BY b` may name a non-output column of `t`; `(SELECT … ORDER BY a)
  LIMIT 1`), while a clause it already has is **`42601`** — PostgreSQL's "multiple ORDER BY clauses
  not allowed", likewise `LIMIT` and `OFFSET`. A parenthesized nested `WITH` folds onto its main
  query, and a `WITH` whose main query is itself a parenthesized `WITH` query (`WITH a AS (…) (WITH
  b AS (…) SELECT …)`) is **`42601`** ("multiple WITH clauses not allowed"). A parenthesized `WITH`
  statement is the top-level `WITH` query.
- **Disambiguation (a §8 cross-core surface).** In operand and statement position a `(` can only
  begin a query. In an **expression or FROM position** — where `(` also begins a parenthesized
  expression, row, or join — the §26 leading-`SELECT`/`WITH` lookahead is extended: a `(` begins a
  parenthesized query when a run of `(` ends at `SELECT`, `VALUES (`, or a `WITH` clause and
  **every** `(` in the run is closed by a `)` followed by `UNION`, `INTERSECT`, `EXCEPT`, `ORDER`,
  `LIMIT`, `OFFSET`, or another `)` — tokens that can follow a query but never a parenthesized
  expression (all six are stop keywords, never an implicit alias, §15). So `((SELECT 1) UNION SELECT 2)` and `((SELECT 1))`
  are subqueries, `x IN ((SELECT a FROM t))` is the subquery form (a multi-row result is fine — the
  oracle agrees), while `((SELECT 1) + 1)` and `((SELECT 1), 2)` stay expressions and `FROM ((SELECT
  1) AS a JOIN b ON …)` stays a parenthesized join (§15). After `EXPLAIN`, a `(` opens the option list
  unless `(`, `SELECT`, `VALUES (`, or a `WITH` clause follows it. Each core precomputes the
  classification for every token in one backward pass over a matching-parenthesis table, so a long
  run of `(` costs linear time and no recursion.
- **Pushdown.** A predicate over a derived table is never moved into an arm that has its own
  `LIMIT`/`OFFSET` (the existing body-pushability rule, [planner.md](planner.md) §3.2): the arm is
  limited first, then filtered.

**`VALUES` queries.** A `VALUES` list is a query wherever a `SELECT` core is (PostgreSQL's
`values_clause` is a `simple_select`): a statement `VALUES (1, 'a'), (2, 'b')`, a set-operation
operand (`SELECT a FROM t UNION VALUES (2), (4)`, `VALUES (1) EXCEPT SELECT …`, parenthesized
`(VALUES …)`), a `WITH` main query, and a CTE, view, derived-table, scalar, `IN`, `EXISTS`, or
`ANY`/`ALL` subquery body, plus an operand of an INSERT query source (§24).

- **It is `SELECT * FROM (VALUES …)`.** The parser builds the query `SELECT *` over a `VALUES`
  relation (§42) labeled `*VALUES*` — PostgreSQL's name for it — so the planner, the column typing,
  and the cost are the derived-table `VALUES` machinery unchanged. Its columns are `column1`,
  `column2`, …, each typed by the `VALUES` body's per-column unification: integer widths widen,
  integer with `decimal` is `decimal`, `NULL` takes the other rows' type, and an all-`NULL` (or
  all-string) column is **`text`** — resolved before any set operation sees it, so `SELECT 1 UNION
  VALUES (NULL)` is `42804` (PostgreSQL: "UNION types integer and text cannot be matched") where
  `SELECT 1 UNION SELECT NULL` is fine.
- **Trailing clauses.** Like a `select_core`, a `VALUES` operand takes no trailing clause of its own
  (`VALUES (1) ORDER BY 1 UNION …` is `42601`); a lone `VALUES` query's `ORDER BY` / `LIMIT` /
  `OFFSET` fold onto it, so a key names `column1`, an ordinal (`42P10` out of range), an expression
  over the columns, or `"*VALUES*".column1` (PostgreSQL); after a set operator the keys see only the
  output columns (`"*VALUES*".column1` is `42P01` there, as in PostgreSQL).
- **Values** are the §42 general expressions — a column reference only correlates to an enclosing
  query, an aggregate is `42803`, a no-context `$N` is `42P18`. A value that is exactly the bare word
  `DEFAULT` is **`42601`** ("DEFAULT is not allowed in this context"), as everywhere but an INSERT's
  own `VALUES` list (§24); jed reserves no keywords, so the rule is positional, as in the INSERT slot.
- **Disambiguation.** In statement and operand position the `VALUES` keyword begins the list; in an
  expression or FROM position `VALUES (` after a `(` begins a query (the §26 lookahead), so a column
  named `values` stays an expression. In FROM, a parenthesized `VALUES` list with no set operator
  stays the §42 `VALUES` relation itself (its alias and rename list apply directly).

**Cost** is `lhs + rhs`, the combine itself unmetered — see [cost.md](cost.md) §3. A `VALUES` query
costs its rows twice — once produced by the `VALUES` relation, once by the wrapping query's scan —
the §42 derived-table charges, no new unit.

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
keys before removing; `UPDATE` validates all rows before writing). An `INSERT ... VALUES` slot
admits a subquery too (§16): with no outer row it is always uncorrelated, folds once against the
pre-statement snapshot, and runs before any row is written — as `INSERT ... SELECT` (§24) does for
subqueries inside its source query.

- **Scalar subquery** — `( query_expr )` in expression position, anywhere a `primary` is
  allowed: `WHERE x = (SELECT max(id) FROM t)`, in the select list, or nested in a larger
  expression `(SELECT …) + 1`. It yields the value of the subquery's single row and single
  column.
- **`x [NOT] IN ( query_expr )`** — membership of `x` in the subquery's single output column.
- **`[NOT] EXISTS ( query_expr )`** — whether the subquery produces at least one row.

A row constructor on the left of a comparison, `IN`, or `ANY`/`ALL` makes the subquery
**row-valued** (multi-column) — see [Row-valued subqueries](#row-valued-subqueries).

**Disambiguation.** A `(` that begins a `primary` starts a scalar subquery when the next token
is `SELECT`, otherwise a parenthesized expression. `IN (` likewise: a leading `SELECT` is the
IN-subquery, otherwise the §20 value list. `EXISTS` is a keyword prefix taking `( query_expr )`.
Because the operand is a full `query_expr`, `IN (SELECT … UNION …)` and `EXISTS (… INTERSECT …)`
parse. A nested `WITH` (cte.md §7) or a parenthesized query (§25 — `((SELECT …) UNION (SELECT …))`,
recognized by the §25 run-of-`(` lookahead) also begins a subquery.

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

### Row-valued subqueries

A **row constructor** on the left — `ROW(a, b, …)` or the bare `(a, b, …)` (composite.md §1) — makes
the subquery **row-valued** in three positions:

```
row op ( query_expr )                 -- op ∈ = <> < <= > >= (single-row comparison)
row [NOT] IN ( query_expr )
row op ANY | SOME | ALL ( query_expr )
```

- **Arity.** The body must return **exactly** as many columns as the row has fields: fewer is
  **`42601`** "subquery has too few columns", more is `42601` "subquery has too many columns"
  (PostgreSQL's messages). A one-field `ROW(x)` takes a one-column body.
- **Value.** Each body row packages its columns as one **anonymous composite** (fields `f1…fN` over
  the column types — exactly the type a `ROW(…)` constructor resolves to), so the comparison is the
  ordinary row comparison of composite.md §5: field types must be pairwise comparable, and the result
  is the **row-constructor 3VL** — `=` is FALSE if any field is FALSE, else NULL if any field is NULL;
  `< <= > >=` are decided by the first unequal field (NULL there is NULL).
- **Cardinality.** The single-row comparison keeps the scalar rules: more than one body row is
  **`21000`**, zero rows is the NULL row (so the comparison is NULL). `IN` / `ANY` / `ALL` take any
  row count; an empty body is FALSE (`IN`, `ANY`) / TRUE (`NOT IN`, `ALL`) regardless of the row, as in
  the single-column forms. The `ANY` / `ALL` per-row comparison is the same row-constructor 3VL —
  **not** the composite total order the array quantifier applies to composite elements
  ([array-functions.md §13](array-functions.md)) — matching PostgreSQL (`ROW(1,NULL) = ANY (SELECT 1,
  NULL)` is NULL).
- **Only a syntactic row constructor, only on the left.** A composite-valued expression on the left
  (a composite column, a parenthesized subquery), a subquery on the **left** (`(SELECT 1, 2) = ROW(1,
  2)`), or any other operator (`IS DISTINCT FROM`) keeps the single-column rule (`42601` "subquery
  must return only one column"). A row constructor **always** compares field-wise, so `ROW(1, 2) =
  (SELECT ROW(1, 2))` is `42601` (too few columns) — PostgreSQL's rule.
- **Errors.** An incomparable field pair is jed's composite gate `42804` for the comparison and `IN`
  forms (PostgreSQL: `42883`, ledgered) and `42883` for `ANY` / `ALL` (the quantifier's posture, §41).
- **Evaluation and cost** are the single-column forms' (planned once; uncorrelated folds to a constant
  row / `IN` list / row-wise constant-array quantifier; correlated re-executes per outer row): packaging
  a body row is free, and the row constructor on the left charges its ordinary `operator_eval` each
  time it is evaluated ([cost.md](cost.md) §3).

```sql
SELECT * FROM orders WHERE (customer, region) IN (SELECT id, home FROM customers WHERE vip)
UPDATE t SET flag = true WHERE (a, b) = (SELECT x, y FROM u WHERE u.id = t.id)
SELECT ROW(1, 2) < ALL (SELECT x, y FROM u)
```

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

- **A correlated reference as a `GROUP BY` / `ORDER BY` key** — landed: grouping or ordering a
  subquery by an enclosing-query column (a per-outer-row constant) is `query.group_by_correlated` /
  `query.order_by_correlated` (above). A correlated window `PARTITION BY` / `ORDER BY` key stays
  `0A000`.
- **Derived tables** — `FROM ( query_expr ) AS t` (a subquery as a relation) landed as its own
  slice; see §42.
- **`ANY` / `ALL` over a subquery** — `x op ANY/ALL(SELECT …)` landed (the subquery spelling of `IN`);
  see §41 / [array-functions.md §11.6](array-functions.md). **Row-valued** subqueries landed —
  see [Row-valued subqueries](#row-valued-subqueries).
- **Subqueries in `GROUP BY`** are not reachable — a `GROUP BY` key is grammatically a
  `column_ref` only (§18), so `(SELECT …)` there is a `42601` syntax error by the existing rule.

## 27. Transaction control (`BEGIN` / `COMMIT` / `ROLLBACK` / savepoints)

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
savepoint   ::= "SAVEPOINT" identifier
release     ::= "RELEASE" "SAVEPOINT"? identifier
rollback_to ::= "ROLLBACK" ("TRANSACTION" | "WORK")? "TO" "SAVEPOINT"? identifier
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
  (active_sql_transaction). Nesting is what savepoints are for; a nested `BEGIN` has no defined
  action, so it errors (PostgreSQL only warns). It does not abort the block.
- A **`COMMIT`/`ROLLBACK` with no open block** is a **lenient no-op success** — it always has a
  well-defined action (publish/discard the current work, of which there is none), so it succeeds
  rather than erroring. PostgreSQL emits a *warning* here; jed has no warning channel
  (CLAUDE.md §4), so `25P01` is deliberately **not** raised — a documented divergence.

The asymmetry is the rule "error where the action is undefined, succeed where it is defined."

**Savepoints** (transactions.md §11). Inside a block, `SAVEPOINT name` marks the current state;
`RELEASE [SAVEPOINT] name` forgets the newest savepoint of that name and every later one, keeping
their changes; `ROLLBACK [TRANSACTION|WORK] TO [SAVEPOINT] name` restores the newest savepoint of
that name — rows, DDL, sequences, temp tables, attachment writes — forgets every later one but keeps
it, and clears a failed block. Names shadow and fold like any identifier. Outside a block each form
is **`25P01`** (no_active_sql_transaction); an unknown name is **`3B001`**
(invalid_savepoint_specification), which aborts a clean block and leaves a failed one failed; in a
failed block `SAVEPOINT` and `RELEASE` are `25P02`. The noise word `SAVEPOINT` after `RELEASE` or
`ROLLBACK TO` is consumed only when a name follows it (`RELEASE savepoint` names a savepoint called
`savepoint`, as in PostgreSQL). `ROLLBACK TO` reports the command tag `ROLLBACK`, as in PostgreSQL.

**Keywords stay non-reserved (§3):** `BEGIN`, `START`, `COMMIT`, `END`, `ROLLBACK`, `WORK`,
`TRANSACTION`, `READ`, `ONLY`, `WRITE`, `SAVEPOINT`, `RELEASE`, `TO` are recognized positionally —
a column may still be named `begin` or `work`.

**Deferred (transactions.md §12):** `SET TRANSACTION ISOLATION LEVEL` (snapshot isolation is the single level); the
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
create_index  ::= "CREATE" "UNIQUE"? "INDEX" ( "IF" "NOT" "EXISTS" identifier | identifier? )
                  "ON" identifier using_clause?
                  "(" index_element ("," index_element)* ")" ( "WHERE" expr )?
index_element ::= column_ref | function_call | "(" expr ")"
drop_index    ::= "DROP" "INDEX" identifier
```

**The optional trailing `WHERE predicate`** makes the index **partial** — only rows whose
predicate is TRUE are indexed ([indexes.md §9](indexes.md)). `WHERE` is recognized
positionally after the closing `)` (it stays non-reserved — a column may be named `where`);
the predicate is an ordinary boolean `expr` over the table's columns. B-tree only this
slice (a `WHERE` on `USING gin`/`gist` is `0A000`).

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

**`CREATE INDEX IF NOT EXISTS name` — the idempotent create.** A name already taken in the target
database's relation namespace — a table, index, sequence, or view (the namespace is shared) — turns
the bare form's `42P07` into a **no-op success** (PostgreSQL emits a *"relation already exists,
skipping"* notice; jed has no notice channel, so it is silent, like `CREATE TABLE IF NOT EXISTS`
§13 and `CREATE SEQUENCE IF NOT EXISTS`). It is not a "create or replace": the existing relation
keeps its definition whatever the new one says (a skipped `UNIQUE` definition leaves a non-unique
index non-unique). **Unlike `CREATE TABLE IF NOT EXISTS`, the skip comes late** — PostgreSQL checks
the name in `index_create`, after `DefineIndex` has validated the whole definition — so jed decides
it exactly where the bare form raises `42P07`: every error listed under *Where the errors fire*
below (`42P01`, `42704`, `42703`, `0A000`, `42P17`, …) still fires, and only the build scan is
skipped (zero cost, and no `23505` build-time check). The statement gating, the database
qualifier, and the `jed_` reservation (`42939` — a reservation is not a collision) also still apply;
the collision is checked in the namespace the bare form checks (an attachment's own namespace for
`db.table`, the temp-aware session namespace otherwise — so under preclude-overlaps a temp table's
index whose name a persistent relation holds is a no-op). PostgreSQL's grammar **requires a name**
after `IF NOT EXISTS` (its production uses `name`, not `opt_single_name`), so the unnamed lookahead
above does not apply there: `CREATE INDEX IF NOT EXISTS ON t (a)` is `42601`. The keywords are
recognized by a **two-token lookahead** (`IF NOT`), so `CREATE INDEX if ON t (a)` still names an
index `if`.

**Key elements: column, function call, or parenthesized expression** ([indexes.md §1](indexes.md),
PostgreSQL's `index_elem`). Each parser reads one element by peeking:

- `(` → a **parenthesized expression** `( expr )`. If the inner expression is structurally
  a bare column reference (`(a)`, `((a))`), it **normalizes to a column key** — PG treats
  `(a)` as the column `a`.
- an identifier **followed by `(`** → a **function-call** expression (`lower(email)`),
  parsed by the ordinary function-call path; the element must then end (`,` or `)`) — a
  trailing operator (`lower(x) + 1`) is a syntax error, so a general expression must be
  parenthesized.
- an identifier **not** followed by `(` → a **bare column** key (as before).

A bare operator expression `a + b` is therefore a `42601` syntax error (the identifier `a`
is a column key, and the following `+` is unexpected) — matching PostgreSQL, which requires
`(a + b)`. `ASC` / `DESC` / `NULLS` / `COLLATE` after an element are likewise unexpected
tokens (deferred narrowings; PostgreSQL accepts them). The `USING method` clause is
recognized positionally after the table name (as for the plain-column form).

**Where the errors fire.** The parser knows no catalog; CREATE INDEX's execution validates
in PostgreSQL's order — table `42P01`, then each key element in list order (a column key:
`42703` unknown / `0A000` unindexable; an expression key: resolve against the table
[unknown column `42703`], then `42803` aggregate / `42P20` window / `0A000` subquery /
`42P02` param / `42P17` non-immutable, then `0A000` for a composite result type), then the
explicit name against the shared relation namespace (`42P07`, or the `IF NOT EXISTS` no-op) — and
DROP INDEX raises
`42704` (missing) / `42809` (names a table). Semantics: [indexes.md §2](indexes.md).

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
`returning_clause ::= "RETURNING" returning_aliases? select_items` — turning the statement into one that
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
anywhere else (api.md §5); a parameter typed by nothing is `42P18`. An `INSERT INTO t AS a`
target alias ([upsert.md](upsert.md) §5) replaces the target's label: items qualify it as `a`,
and `t.col` is `42P01` with the table-alias hint ([error-fields.md](error-fields.md) §11.2).

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
- **The table name shadows an unaliased qualifier** (probed): if the target table is itself
  named `old` (or `new`), that qualifier keeps its ordinary table-qualified meaning and the
  same-named row-version pseudo-relation is suppressed. The aliasing form below recovers the
  hidden version. The shadowing label is the target's *label*: an `INSERT INTO old AS a` frees
  `old` for the row version, and `INSERT INTO t AS old` shadows it.
- An unknown column under a qualifier is `42703`; **bare** `old`/`new` are ordinary column
  references (a column may be named either — they resolve normally, never to a row
  version). `old.*`/`new.*` are **qualified stars** (§15) — they expand that row version's
  columns (`new.*` = the produced row, `old.*` = the opposite version, all-NULL under
  `INSERT`), matching PostgreSQL 18.
- **Output names** follow §8 unchanged: `RETURNING old.v` names the column `v` (the
  qualifier never leaks — matches PostgreSQL).

Operationally the projection row is the **concatenation** `[base | other]` of the two
versions (base = what bare references read; other = the opposite version, the all-NULL
row when the statement has no such version), and `old`/`new` are **qualifier-only**
pseudo-relations over it — invisible to bare-column resolution (no new ambiguity) and to
every other statement's scope. Cost: the qualifiers are leaves like any column, and the
**touched set** distinguishes the sides — [cost.md](cost.md) §3 "`RETURNING`".

**Row-version aliases** (PostgreSQL 18, probed). An optional list immediately after
`RETURNING` renames either or both pseudo-relations before the item list:

```sql
UPDATE account SET balance = balance + 10
RETURNING WITH (OLD AS o, NEW AS n) o.balance, n.balance;
```

The production is
`returning_aliases ::= "WITH" "(" returning_alias ("," returning_alias)* ")"`, where
`returning_alias ::= ("OLD" | "NEW") "AS" identifier`. `AS` is required. Order is free,
but `OLD` and `NEW` may each appear at most once; repeating either is `42601`. Naming both
versions with the same alias, or colliding an explicit alias with the target relation's
label, is `42712` (*duplicate alias*). An explicit alias is qualifier-only and works for
qualified columns, qualified stars, and correlated references inside item subqueries.
Consistent with §3's no-reserved-word rule, the prefix is recognized by the structural
`WITH ( OLD|NEW AS` shape; `with` remains an ordinary identifier in every other expression
shape.

Aliasing one version hides that version's standard name; the other version keeps its
standard name when the name is otherwise free. Explicit aliases are installed before
unaliased defaults: consequently `WITH (OLD AS new)` makes `new` name the old row and
suppresses the unaliased new-row pseudo-relation. This priority is what lets an alias recover
`OLD` for a target table literally named `old`, while preserving the target table's ordinary
meaning under `old.*`. Bare and target-table-qualified references are unchanged.

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

**Syntax.** `table_function ::= identifier "(" function_arg ("," function_arg)* ")"` — a `(`
immediately after the leading identifier in `table_ref` position marks the function form (a
**one-token lookahead**, a §8 cross-core determinism surface; a bare table name has no `(` there).
The arguments are general expressions parsed exactly like a `function_call`'s, minus the `*` /
`DISTINCT` forms (those are the aggregate/star spellings, not an SRF argument list). An argument may
be named (`name => value`, §17; a positional argument after a named one is `42601`). Only
`jsonb_path_query` / `jsonb_path_query_tz` declare parameter names (PostgreSQL's `target`, `path`,
`vars`, `silent`, with `vars` and `silent` defaulted — [jsonpath.md](jsonpath.md) §5); a named
argument to any other SRF is `42883`. The optional
alias is `table_function_alias ::= "AS"? identifier ("(" identifier ("," identifier)* ")")?`:
`AS` remains optional, while a parenthesized column-alias list requires the relation alias before it.

**Labeling and column aliases.** The relation's **label** is the alias, or the function name when
there is none. Without a column list, a one-column function follows PostgreSQL's
single-column function-alias rule (`chooseScalarFunctionAlias`): a function that declares a named
OUT parameter is named by it, alias or not (`jsonb_array_elements(doc) AS e` ⇒ column `value`, so
`e.value` resolves); any other one-column function takes the relation alias as its column name
(`generate_series(1, 5) AS g` ⇒ column `g`, so `g.g` resolves and `g.generate_series` is
`42703`), and an unaliased one its catalog column name (`generate_series`). Which functions declare
an OUT parameter is catalog data (`column` / `column_source`, [functions.md](functions.md) §10).

A bare name that labels a one-column function relation but names no column in scope is a
**whole-row reference**, which for a scalar function is its one column (PostgreSQL's
`makeWholeRowVar`): `SELECT e FROM jsonb_array_elements(doc) AS e` and `SELECT g FROM
generate_series(1, 3) AS g(n)` read the produced value, and the output column is named by the
reference (`e`, `g`). Columns win: every scope level is searched for a column before the name is
tried as a relation label (nearest level first). A `USING` column, a qualified `g.g`, and any other
relation kind (a table, derived table, CTE, multi-column or record function) never take this
reading — jed has no implicit per-table row types.

An explicit list overrides output names left-to-right: `generate_series(1, 5) AS g(n)` exposes
the column as `g.n`, and `jsonb_each(doc) AS e(k, v)` exposes `e.k` / `e.v`. It applies to every
fixed-shape table function, including multi-column SRFs. Fewer aliases than output columns is a
partial rename and leaves the remaining catalog names intact; more aliases than available columns
is `42P10` (*invalid column reference*). The list changes names only — never column order, types,
cardinality, evaluation, or cost. A typed column-definition list (`AS r(c type, …)`) remains the
separate record-returning-function form ([json-table.md](json-table.md) §1, [composite.md](composite.md)
§13 for `unnest(record[])`); the parser distinguishes it structurally by the type word after the first
column name, and `AS (c type, …)` with no alias name is always that form. Oracle-verified against PostgreSQL.

**Composition.** An SRF relation is a first-class FROM item: it joins/cross-joins other
relations (`t CROSS JOIN generate_series(1, 3)` is the product), and `WHERE` / `ORDER BY` /
`LIMIT` / subqueries compose over it. Its arguments are **implicitly `LATERAL`** (§44, matching
PG): a `$N` parameter, a **correlated outer-query column** (`(SELECT count(*) FROM
generate_series(1, o.n)) FROM t o`), **and** a **column of an earlier sibling FROM relation**
(`FROM t CROSS JOIN generate_series(1, t.n) g`) are all legal arguments — a sibling reference
re-evaluates the SRF once per left-hand row (§44). (For the first/only FROM item there is no
sibling, so an arg sees only `$N`/outer, as before.)

**Deferred narrowings** (each the relevant error, relaxable later, and shared by both SRFs): the
**SELECT-list** SRF position (`SELECT generate_series(1, 5)` / `SELECT unnest(…)` — an SRF is not a
scalar function, `42883`), **`WITH ORDINALITY`**, and `generate_series`'s non-integer variants
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
| `interval` | PostgreSQL's `interval_in` — units, SQL-standard, ISO 8601 ([interval.md](interval.md) §3) | `22007` / `22008` |
| `bytea` | `\x`-hex input ([types.md](types.md) §13) | `22P02` |
| `uuid` | PG-flexible uuid input ([types.md](types.md) §14) | `22P02` |
| `text` | identity (the string itself) | — |
| `i16` / `i32` / `i64` | optional sign + decimal digits, surrounding whitespace trimmed | `22P02` malformed / `22003` out of range |
| `decimal` / `numeric` | jed's decimal-literal grammar (sign, digits, one `.`, an optional `e`-notation exponent — §14), whitespace trimmed; capped | `22P02` malformed / `22003` over cap |
| `boolean` | PG's `boolin`: `t`/`tr`/`tru`/`true`, `f`/`fa`/`fal`/`fals`/`false`, `y`/`ye`/`yes`, `n`/`no`, `on`/`off`, `1`, `0` (case-insensitive, trimmed) | `22P02` malformed |

The native-syntax types (`integer`/`decimal`/`boolean`) are where `type 'string'` is a genuine
**cast from text** — coercing a *string* to a number/bool. The literal form folds the coercion at
**resolve** (a string-literal operand); a **runtime** text→`T` cast on a *non-literal* text
expression (`CAST(text_col AS int)`, `s :: numeric(10,2)`) runs the **same per-type coercion
per-row in the evaluator** (`evalCast`), so it is now admitted for the **numeric + boolean** scalar
targets — `i16`/`i32`/`i64`, `decimal`/`numeric` (with the typmod re-scale), `f32`/`f64`, and
`boolean`. "One coercion, three syntaxes" extends to "one coercion, literal **or** runtime": the
runtime path calls the identical `parse_int_literal` / `parse_decimal_literal` / `parse_f*_literal`
/ `parse_bool_literal`, so the literal form's grammar governs — PostgreSQL's input functions,
including the 0x/0o/0b and `_` integer/decimal forms (below); `NaN` / `±Infinity` trap `22P02` for
the integer and decimal targets — the same documented PG divergence as the literal form; a float
target accepts `NaN` / `±Infinity`, float.md §4), the cast
node's existing `operator_eval` charge meters it, and a malformed / out-of-range value traps
`22P02` / `22003` **per row** during the scan rather than at plan time. The runtime text→`T` cast to
the **string-native** targets has landed by the same rule — each runs its literal's parser per row:
text→`date` (`parse_date`, `22007`/`22008`; STABLE and un-indexable, [date.md](date.md) §6),
text→`timestamp` / `timestamptz` (an offset-less `timestamptz` string read in the session zone —
[timezones.md](timezones.md) §9.3), text→`interval` ([interval.md](interval.md) §6), and
text→`bytea` (the `\x` hex input, `22P02`; immutable) — [types.md](types.md) §5 — and text→`uuid`
(the uuid cast slice, [types.md](types.md) §14). A **bare** string still does **not**
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

- For `integer`/`decimal`, jed coerces with **PostgreSQL's input functions** (`pg_strtoint16/32/64`,
  `numeric_in`), including PostgreSQL 16+'s **hex/octal/binary** (`integer '0x10'` = `16`,
  `numeric '0xff'` = `255` at scale 0, of any size) and **digit underscores** (`integer '1_000'`,
  `numeric '1_000.5'`) — the same forms as the native literals (§4) — and **scientific notation**
  for decimal (`numeric '1.5e3'` is `1500`; an integer target rejects it, as `int4in` does). The
  rules are PostgreSQL's exactly: surrounding `isspace` whitespace (including vertical tab) is
  skipped; a `_` must sit between two digits (after a radix prefix it may lead; in a decimal it
  may not touch the `.`); an integer whose magnitude overflows **while its digits are read** is
  `22003` even with junk after it (`'99999999999x'::int`), but one that overflows only at the final
  range check loses to the junk (`'0x80000000x'::int` is `22P02`). The one divergence is
  **`NaN` / `±Infinity`** (`numeric 'NaN'` — jed's decimal is always finite,
  [decimal.md](decimal.md) §2), which traps `22P02` where PG accepts it.
- jed uses the **canonical single-word** type names: PG's multi-word `TIMESTAMP WITH TIME ZONE
  '…'` and the **precision typmod** form `TIMESTAMP(p) '…'` / `NUMERIC(p,s) '…'` are not typed
  literals here (a `(` after the name breaks the lookahead; typmod rides only on `CAST`).
- **ISO-8601 / SQL-standard combined interval forms** inside `INTERVAL '…'` remain deferred
  ([interval.md](interval.md) §3) — a parse-subset gap, not a literal-syntax one.

`TIME '…'` is absent only because `time` is not in the scalar set (`42704`).

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
- the **text casts** — casting **to** `text` renders any built-in scalar in its output form
  (`5 :: text` is `'5'`, identical to `CAST(5 AS text)`; a `timestamptz` renders in the session
  zone — [types.md](types.md) §5); `'5' :: text` is the string identity `'5'`. Casting **from**
  `text` reaches the numeric + boolean scalars at runtime (`s :: int`, `s :: numeric(10,2)`,
  `s :: boolean`, the §36 runtime coercion), `uuid` (the uuid cast slice), and the string-native
  `date` / `timestamp` / `timestamptz` / `interval` / `bytea` ([types.md](types.md) §5). `iv ::
  interval` is the identity; any other source to `interval` is `42804`;
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
context is still `42P18` indeterminate.) `$1 :: text` declares `$1` as text, and `$1 ::
varchar(n)` truncates the bound value.

**Lexing.** `::` is two colons scanned greedily as one token; a **lone** `:` is a `42601` syntax
error (jed has no `:name` host parameters, array slices, or `psql` meta-syntax — nothing else uses
a colon). See [grammar.ebnf](../grammar/grammar.ebnf) `double_colon`.

**No divergence of its own.** `::` adds no behavior; it only adds the spelling. Any divergence
of a cast from PostgreSQL (a forbidden pair is `42804` where PG says `42846`, the to-text casts are
explicit-only, [types.md](types.md) §5) is the same for both spellings.

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
- **Errors.** Field lookup is byte-exact on the folded name (§3, as PG). An unknown field is
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
- **AST + resolution.** One `BinaryOp::Concat` node; the resolver (`resolve_concat`) routes a jsonb,
  bytea, or (with no typed array operand) text-like operand to its own concatenation
  (string-functions.md §4/§5). With a typed array operand it does overload resolution over the
  three anyarray `concat` catalog rows in order (cat, append, prepend) and reuses the AF1 array
  kernels. A bare untyped `NULL` beside an array resolves to `array_cat` (the NULL array is the
  identity), matching PostgreSQL; a typed null element resolves to `array_append`
  (array-functions.md §8.1). No matching overload (`1 || 2`) is `42883`.

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
and IN-subqueries use (§26), so the three hand-written parsers stay byte-identical; so is one
followed by a nested `WITH` or a parenthesized query (`FROM ((SELECT …) UNION (SELECT …)) s`, the §25
lookahead). A leading `(` followed by `VALUES` is the **VALUES-body derived table** (below). Any other
leading `(` begins a **parenthesized join** `FROM (a JOIN b ON …)` (§15) — an aliased one
is exactly the derived table `(SELECT * FROM a JOIN b ON …) AS g`. The base-table / SRF forms are unchanged: a `(` *after* a leading
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
  Each is resolved with **no FROM row**: the body is **non-`LATERAL`** (like every derived table, it
  sees no FROM sibling), so a column reference inside resolves only as a **correlated reference to an
  enclosing query level** (below) and is otherwise unresolved (`42703`/`42P01`); an **aggregate** is
  `42803`, and a bind parameter with no inferable
  local type is `42P18` (jed does not infer a bare `$N` from sibling rows — the documented `$N`
  posture of §26; write `$1::int`). This is the one place a VALUES body is *richer* than
  `INSERT … VALUES`: lifting general expressions into the INSERT slot too is a separate future slice.
- **Shape and column typing.** Every row must have the **same arity** (`42601`,
  `VALUES lists must all be the same length` — the §12 rule). The relation has one column per value;
  its **default names are `column1`, `column2`, …** (PostgreSQL), overridable by the alias's
  column-rename list. Each **column's type unifies across the rows** by the set operation's
  common-type rule (§25): `int`-widths widen, `int`+`decimal` → `decimal`, `f32`+`f64` → `f64`,
  anything + `NULL` keeps the other, identical types always unify (`jsonb`, `json`, ranges,
  arrays, composites, …), an all-`NULL` column is `text` (unknown→text); a pair with no common type
  is **`42804`** (`datatype_mismatch`). The unified column type then coerces each row's value (the
  only runtime changes are `int`→`decimal` and `f32`→`f64`, as for a set operation).
- **A trailing `ORDER BY` / `LIMIT` / `OFFSET` on the body.** `(VALUES (3),(1),(2) ORDER BY 1 LIMIT 2)
  AS v` sorts and windows the rows before the outer query sees them (PostgreSQL). The body is then the
  §25 `VALUES` query `SELECT * FROM (VALUES …) ORDER BY … LIMIT … OFFSET …` over an inner VALUES
  relation labeled `*VALUES*`, so a key names `column1`, `column2`, … by name, as
  `"*VALUES*".column1`, by ordinal (`42P10` past the last column), or in an expression — never the
  derived table's alias or rename list (`v.column1` is `42P01`, a renamed `x` is `42703`;
  oracle-identical), and the alias and rename list apply to the query's output unchanged. Its cost
  is the VALUES rows plus that query's scan and sort — the ordinary derived-table and `ORDER BY`
  charges, no new unit ([cost.md](cost.md) §3). `LATERAL` applies to the whole body. A body whose
  `VALUES` list is followed by a set operator (`FROM (VALUES (1) UNION VALUES (2)) AS v`) is a query
  with `VALUES` operands (§25).
- **`DEFAULT`.** A value that is exactly the bare word `DEFAULT` is `42601` ("DEFAULT is not allowed
  in this context", PostgreSQL) — only an INSERT's own `VALUES` list takes it (§24).

**The body sees no sibling (no `LATERAL`), but does see enclosing query levels.** A derived-table
body sees its own FROM, the catalog, and the statement's **CTE bindings** (so `WITH c AS (…) SELECT
* FROM (SELECT * FROM c) AS d` works), but **never** a sibling FROM relation of the query that owns
the FROM clause: a reference to a sibling column inside the body is unresolved (`42P01`/`42703`) —
without the `LATERAL` keyword (§44) a FROM-subquery cannot see the other FROM items. It *is*,
however, planned against the **enclosing query's scope** (the `parent` of the query that owns the
FROM clause), so when that query is itself a subquery, a column of an **enclosing query level**
inside the body is an ordinary correlated reference (§26), in the **same correlation frame** as the
owning query's own outer references: `SELECT (SELECT z FROM (SELECT a.x AS z) d) FROM a` returns
each row's `x`. This matches PostgreSQL, where the non-`LATERAL` rule hides only the FROM *siblings*
(they are skipped and resolution continues outward — so a sibling label that shadows an enclosing
one resolves to the enclosing relation). A body with such a reference makes the owning subquery
correlated (it is never folded once, §26); it is re-executed per enclosing row, running the body in
place each time (the body itself is still materialized once per run of the owning query — it is not
re-run per sibling row, so a `RIGHT`/`FULL JOIN` to it stays legal). At top level there is no
enclosing query, so such a reference is unresolved. Its cost is the ordinary per-outer-row sum of
the inline body costs — no new unit. (A nested `WITH`'s **CTE** bodies follow the same rule — they
see the levels enclosing the query that owns the `WITH`, never its FROM, [cte.md](cte.md) §7.) A derived table itself nests freely: its
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
| Sibling column referenced inside the body | `42703` / `42P01` | The body is not LATERAL (§44). An enclosing query level's column correlates instead (above). |
| Leading `(` in FROM not followed by `SELECT`, `VALUES`, or `WITH` | ✅ | **Landed** (§15): a parenthesized join. `FROM (a)` / `(a, b)` (no join inside) is `42601`, as in PG. |
| Qualified reference to a duplicated body output column (`d.a`) | `42702` | Ambiguous, like the bare reference (PG). |
| Nested `WITH` inside the body | ✅ | **Landed** (cte.md §7): a derived-table body may be a `WITH`-prefixed query, establishing a child scope over the enclosing visible CTEs. |
| A bare `DEFAULT` value in a VALUES body | `42601` | `DEFAULT is not allowed in this context` (PG). |
| VALUES-body rows of differing arity | `42601` | `VALUES lists must all be the same length` (the §12 rule). |
| VALUES-body columns whose row types do not unify | `42804` | `datatype_mismatch`, the set-operation unification rule (§25). |
| Column reference (not of an enclosing query level) / aggregate / no-context `$N` in a VALUES-body value | `42703` / `42803` / `42P18` | The body is a non-`LATERAL` relation with no FROM row. |
| Trailing `ORDER BY` key naming the derived table's alias / rename list | `42P01` / `42703` | The keys see only the VALUES columns `column1`, … (PG). |
| Body nesting exceeds `MAX_EXPR_DEPTH` | `54001` | The parser depth gate (cost.md §7). |

**Deliberate narrowings (each relaxable later, [../../TODO.md](../../TODO.md)).**

- **`LATERAL`** — ✅ **landed** (§44): a derived table (or SRF, §35) preceded by `LATERAL` (SRFs
  implicitly) sees earlier FROM relations. A *non-`LATERAL`* derived-table body sees no sibling but
  correlates to enclosing query levels, the rule above.
- **Parenthesized join / table-reference FROM** — ✅ **landed** (§15): `FROM (a JOIN b ON …) c` is
  the derived table `(SELECT * FROM a JOIN b ON …) AS c`; unaliased, the inner labels stay visible.
- **A `WITH` *inside* the body** (nested `WITH`) — ✅ **landed** (cte.md §7): a derived-table body,
  like any parenthesized subquery, may be a `WITH`-prefixed query (`subquery_expr`), establishing its
  own child CTE scope over the enclosing statement's visible bindings. *(The
  `VALUES` body itself has **landed** — see "The VALUES body" above, including a trailing `ORDER
  BY`/`LIMIT`/`OFFSET`; its residual narrowing is general expressions in the `INSERT … VALUES`
  slot.)*
- **Top-level `UPDATE`/`DELETE` FROM** stays single-table (§15) — a derived table reaches them only
  inside a subquery.

## 43. `FOREIGN KEY` constraints (`REFERENCES` / `FOREIGN KEY ( cols ) REFERENCES …`)

Both constraint positions gain the referential form ([constraints.md §6](constraints.md)). A
`column_constraint` may be a bare `references_clause` (the one-member form — the local column is the
column being defined):

```
references_clause  ::= "REFERENCES" identifier ("(" identifier ("," identifier)* ")")?
                       ("MATCH" ("FULL" | "SIMPLE" | "PARTIAL"))?
                       ("ON" ("DELETE" | "UPDATE") referential_action)*
referential_action ::= "NO" "ACTION" | "RESTRICT" | "CASCADE" | "SET" "NULL" | "SET" "DEFAULT"
```

and a `table_constraint` may be
`["CONSTRAINT" identifier] "FOREIGN" "KEY" "(" identifier ("," identifier)* ")" references_clause`
(the local-column list reuses the `PRIMARY KEY` list shape — bare names, non-empty). The
referenced-column list is optional and defaults to the parent's primary key. The optional match
type follows it and precedes the actions, at most once (PostgreSQL's `key_match`): `MATCH SIMPLE`
(the default) or `MATCH FULL` ([constraints.md §6.3](constraints.md)); `MATCH PARTIAL` parses but is
rejected `0A000` (`MATCH PARTIAL not yet implemented`, PostgreSQL's parse-time error). A `MATCH`
after an action, a second `MATCH`, or `MATCH` followed by anything else is `42601`.

**Disambiguation.** `FOREIGN`, `KEY`, `REFERENCES`, `ON`, `DELETE`, `CASCADE`, `RESTRICT`, `ACTION`,
`NO`, `SET`, `MATCH`, `SIMPLE`, `PARTIAL` stay non-reserved (§3); `MATCH` is recognized only directly
after the referenced table / column list, where no other clause can begin with it. A table element beginning with the two keywords `FOREIGN KEY`
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
the per-table constraint namespace), and stores the selected `referential_action` for parent-side
execution ([constraints.md §6](constraints.md)). PostgreSQL *reserves*
`FOREIGN` / `REFERENCES`, so the jed-only non-reserved surface carries oracle overrides like `on`'s
(conformance.md §5). The match type is a parse-time choice stored with the FK; only `MATCH PARTIAL`
errors at parse.

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
  sibling-blind relation of §42 (a sibling reference is `42P01`/`42703`; an enclosing query level
  still correlates). *With* it, the
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

**Spelled through `JOIN` or a comma.** `LATERAL` follows any join keyword — `CROSS JOIN LATERAL`,
`[INNER] JOIN LATERAL … ON …`, `LEFT [OUTER] JOIN LATERAL … ON …`, and (below) `RIGHT`/`FULL` — or a
comma (`FROM t, LATERAL (…)`, §15), where the lateral item sees every earlier comma item.

**Join kinds.** `INNER` / `CROSS` / `LEFT` work as expected: `LEFT JOIN LATERAL … ON <pred>` keeps every
left row, NULL-extending it across the lateral columns when the lateral side produces no matching row
(so `LEFT JOIN LATERAL (…) ON true` keeps a left row even when the lateral body returns **zero** rows —
the common "optional explode"). On the right side of a **`RIGHT`/`FULL` join**, a reference to **that
join's left side is `42P10`** (`invalid_column_reference`, matching PG's `check_lateral_ref_ok`): the
right side cannot both be kept whole *and* be evaluated per left row. PostgreSQL leaves those relations
visible but refuses them, so the check is made at the reference — positioned there, with the message
*invalid reference to FROM-clause entry for table t* (an unaliased derived table is `unnamed_subquery`,
a column of an unaliased join `unnamed_join`) and the Detail *The combining JOIN type must be INNER or
LEFT for a LATERAL reference.* — and it fires before the reference's column is looked up and before any
ambiguity with another relation. The left side is every relation to the left of the join in its comma
item; a comma ends it, so a later comma item may reference those relations again. A reference that
passes the left side to a relation **outside** the join is allowed: an enclosing query level
(`SELECT (SELECT … FROM b RIGHT JOIN LATERAL (SELECT a.x) s ON …) FROM a`), an earlier comma item
(`FROM a, b RIGHT JOIN LATERAL (SELECT a.x) s ON …`, a comma item §15 parenthesizes), or a relation to
the left of an enclosing parenthesized join (§15). Such an item is not dependent on its siblings: it is
planned against the relations outside the join and materialized once per evaluation of the FROM it
belongs to (like a non-`LATERAL` derived table, §42) — once per outer row of a correlated subquery,
once per left-hand row of a correlated comma item or group. A lateral-eligible item with **no**
correlation (`RIGHT JOIN generate_series(1, 2) g`, `RIGHT JOIN LATERAL (SELECT 1) s`) is materialized
once. Either way **any** join kind is allowed — matching PG.

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
| A reference from the right side of a `RIGHT` / `FULL JOIN` to that join's left side | `42P10` | `invalid_column_reference`, at the reference, with a Detail: the combining join type must be INNER or LEFT for a LATERAL reference (PG). |
| A lateral SRF / body whose own argument / clause errors (arity, type, `54001` depth, …) | *(as §35/§42)* | Lateral changes only the *scope* the body resolves against, not its other rules. |

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

The target may carry an alias (`INSERT INTO t AS a …`, `AS` required): it replaces the table's
name as the qualifier for the existing row in `SET`/`WHERE` (and in `RETURNING`), so `t.col` is
`42P01`; a target labeled `excluded` makes `excluded.col` `42P09` ([upsert.md](upsert.md) §5).

- **`DO NOTHING`** skips the offending row; **`DO UPDATE SET … [WHERE …]`** updates the
  existing conflicting row. In `DO UPDATE`, the **`excluded`** pseudo-relation names the row
  *proposed for insertion* (`excluded.col`), while a **bare or table-qualified** column names
  the **existing** row — the `SET`/`WHERE` reuse the `UPDATE` `update_assignment`/`where_clause`
  productions (so `SET col = DEFAULT` resets the existing row's column to its default, §16), and
  `excluded` is a **qualifier-only** pseudo-relation exactly like `old`/`new` in `RETURNING` (§32).
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
(`"en-US"`, `"C"`), so `COLLATE` takes **only** the quoted form ([grammar.ebnf](../grammar/grammar.ebnf)
`quoted_identifier`, §3) — a bare word after `COLLATE` is `42601`. (This was the lexer's first use of
the quoted token; quoted identifiers are now general names, §3.)

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

## 49. `AT TIME ZONE` ([timezones.md](timezones.md))

`expr AT TIME ZONE zone` is PostgreSQL's time-zone conversion operator, the single tz consumer the
first slice ships ([timezones.md §6](timezones.md)). It converts between an instant and a wall-clock
reading in `zone`, both directions:

```sql
SELECT ts AT TIME ZONE 'America/New_York'        -- timestamptz → timestamp (local wall clock in the zone)
SELECT wall AT TIME ZONE 'Europe/Paris'          -- timestamp → timestamptz (interpret wall clock as in the zone)
SELECT now() AT TIME ZONE 'UTC'                  -- UTC and fixed ±HH:MM offsets need no loaded data
```

- **Result type.** `timestamptz AT TIME ZONE zone → timestamp` (the zone-less local reading);
  `timestamp AT TIME ZONE zone → timestamptz` (the UTC instant). Any other left operand type — **or
  a non-text `zone`** — is `42883` (PG resolves `AT TIME ZONE` via the `timezone(zone, value)`
  function overload, so a wrong-typed argument is "no such function", oracle-pinned). The `zone`
  operand is a **text** expression evaluated per row.
- **Precedence.** `AT TIME ZONE` binds **tighter than the comparison operators and `||`, looser than
  the additive/`::`/postfix operators** ([grammar.ebnf](../grammar/grammar.ebnf), matching PG): it is
  parsed as a left-associative infix operator at PG's `AT` precedence rung, above `BETWEEN`/`IN` and
  the comparisons. `a AT TIME ZONE z = b` parses as `(a AT TIME ZONE z) = b`.
- **The `zone`** is a quoted **string literal or text expression** (not a bare identifier): `UTC` and
  fixed numeric offsets `±HH`, `±HH:MM`, `±HH:MM:SS` are built-in and need no loaded data
  ([timezones.md §3.2](timezones.md)); every **named IANA zone** must be provided by a **loaded `JTZ`
  bundle** (the corpus `# load-timezone:` directive declares the dependency, [timezones.md §11](timezones.md)).
- **Resolution / errors** ([timezones.md §6](timezones.md)): a zone name no loaded bundle provides (and
  not a built-in `UTC`/offset) is **`22023`** (`invalid_parameter_value`, PG's "time zone not
  recognized") — never a silent substitution. The `timestamp → timestamptz` direction at a DST gap /
  overlap resolves the ambiguous wall clock to PostgreSQL's branch (oracle-pinned), never an error. The
  operator is metered by the `timezone` cost unit ([timezones.md §10](timezones.md)).

## 50. `EXTRACT` and the conversion surface ([timezones.md §9](timezones.md))

`EXTRACT(field FROM source)` is the SQL special form for a single datetime field; the second tz slice
also ships `date_trunc` and the cross-family datetime casts ([timezones.md §9](timezones.md)).

```sql
SELECT EXTRACT(hour FROM timestamp '2024-03-15 13:47:23')   -- 13   (numeric)
SELECT EXTRACT('dow' FROM date '2024-03-15')                -- 5    (field as a string literal)
SELECT EXTRACT(hour FROM tstz)                              -- in the SESSION zone, for a timestamptz
SELECT date_trunc('day', timestamptz '…', 'America/New_York')  -- truncate in an explicit zone
SELECT (tstz)::date                                          -- cross-family cast (session zone)
```

- **Grammar.** `EXTRACT "(" ( identifier | string ) "FROM" expr ")"` ([grammar.ebnf](../grammar/grammar.ebnf)).
  The **field is syntactic** — PostgreSQL's `extract_arg`: an identifier, bare (`hour`) or
  double-quoted (`"HOURS"`), or a single-quoted string, case-insensitive (the unit lookup folds a
  quoted or string field; a zero-length `""` is the lexer's `42601`), spelled
  with any of PostgreSQL's unit aliases (`hours`, `msec`, `y`, `jd`, …, [timezones.md §9.6](timezones.md)) — recognized
  only when `EXTRACT` is immediately followed by `(` (a one-token lookahead, §8; `EXTRACT` stays usable as
  a column / function name otherwise). It is validated at **resolve** time.
- **Result type.** `EXTRACT` → `numeric` (PG 14+, matchable exactly; jed has exact `decimal`). The
  function spelling `date_part('field', src)` returns `double precision`
  ([timezones.md §9.2](timezones.md)).
- **Source types & field validity.** `source` is `timestamp` / `timestamptz` / `date` / `interval` (any
  other → `42883`). The field-validity matrix matches PG ([timezones.md §9.2](timezones.md)): an
  unsupported field for the source type is **`0A000`** (e.g. `EXTRACT(hour FROM date)`), an unrecognized
  field is **`22023`**. `julian` is the Julian Day (date: the integer day number; timestamp: plus the
  exact fraction of the day). For a `timestamptz` source
  every field is computed **in the session zone** except `epoch` (the instant).
- **`date_trunc`.** `date_trunc(unit, source)` (2-arg) over `timestamp` / `timestamptz` / `interval`,
  plus `date_trunc(unit, timestamptz, zone)` (3-arg) — `unit` is a runtime `text` value (case-
  insensitive, with PostgreSQL's aliases; an unrecognized unit is `22023` **at evaluation**). A `timestamptz` is truncated in the
  session zone (2-arg) or the explicit zone (3-arg); a `date` argument is `42883` (no implicit
  `date`→`timestamp` cast — [timezones.md §9.1](timezones.md)).
- **Cross-family casts.** The `timestamp` / `timestamptz` / `date` cast matrix
  ([spec/types/casts.toml](../types/casts.toml), [timezones.md §9.3](timezones.md)); every cast crossing
  the `timestamptz` boundary consults the session zone, as does the runtime `text → timestamptz` cast
  for an offset-less string ([types.md](types.md) §5).
- **Session zone & cost.** The session zone drives *computation*, not yet *rendering*
  ([timezones.md §9.5](timezones.md)). A zone consultation charges the `timezone` cost unit; the
  zone-free forms charge only `operator_eval`.

## 51. `COALESCE`

`COALESCE(a, b, …)` returns its **first non-NULL argument**, or NULL when every argument is
NULL (`coalesce_expr ::= "COALESCE" "(" expr ( "," expr )* ")"`). It is the second deliberately
**lazy** expression after `CASE` (§23) — `COALESCE(a, b)` behaves exactly like
`CASE WHEN a IS NOT NULL THEN a ELSE b END`, except each argument is evaluated **at most once**
(no separate test-then-reuse evaluation), which is also why it is a grammar form with its own
node rather than a catalog function: a catalog call is eager and fixed-arity, and the
double-evaluation of the CASE rewrite would change the deterministic cost.

```sql
SELECT COALESCE(nickname, name, '(anonymous)') FROM u   -- first non-NULL, left to right
SELECT COALESCE(SUM(amount), 0) FROM sales               -- the empty-input aggregate default
SELECT COALESCE(a, 1 / a) FROM t                          -- a ≠ NULL ⇒ 1/a is never evaluated
```

- **Grammar.** At least one argument (`COALESCE()` is a `42601`, PostgreSQL's shape — its
  grammar has no empty form). `COALESCE` stays **non-reserved**: the form is recognized only
  when the keyword is immediately followed by `(` (the `EXTRACT`/`JSON(` one-token lookahead,
  §8), so `coalesce` remains usable as a column name — observably PostgreSQL's
  `col_name_keyword` classification (a column name, never a function name).
- **Lazy left-to-right evaluation — a sanctioned short-circuit** ([cost.md](cost.md) §3). Arguments are evaluated in source order and evaluation **stops at the first non-NULL**
  value, which is returned; later arguments are **never evaluated** — `COALESCE(1, 1/0)` is `1`,
  no `22012` — and each evaluated argument is evaluated exactly once. Required by PostgreSQL
  semantics, deterministic because the order is fixed.
- **Argument type unification — exactly CASE's result-arm rule** (§23, the §25 common-type
  fold). NULL-typed arguments are dropped (they adapt); an **all-NULL COALESCE is `text`**;
  numerics promote (decimal if any arm is decimal, else the widest integer; an integer result
  widens to decimal at eval when the common type is decimal), mixed float widths widen to `f64`,
  and any other arguments must be identical types. A pair with no common type is **`42804`**
  (`COALESCE types must be compatible`). A bare
  `$N` argument adopts the unified scalar result type before parameters are bound (§5).
- **Where it is legal**: anywhere an expression is — projections, `WHERE`, `GROUP BY`/`HAVING`,
  `ORDER BY`, `CHECK` constraints, expression `DEFAULT`s, and **index expressions**
  ([indexes.md](indexes.md) §9 — immutable iff its arguments are, like any pure combinator;
  an index on `COALESCE(a, 0)` matches the same expression in a query and pushes down).
  Aggregates nest as in any expression (`COALESCE(SUM(x), 0)` — the empty-input default that
  aggregate partitioning needs, [conformance.md](conformance.md) §5).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the COALESCE node, plus the
  `operator_eval`s of the arguments evaluated **up to and including the first non-NULL** (each
  argument charges its own; a leaf argument — column or literal — charges nothing). Output name
  for a bare `SELECT COALESCE(…)` is **`coalesce`** (PostgreSQL) — the fixed keyword lowercased,
  the same no-expression-printer rationale as the §8 rule-4 function names.

## 52. `GREATEST` / `LEAST`

`GREATEST(a, b, …)` returns the **largest** of its arguments and `LEAST(a, b, …)` the **smallest**
(`greatest_least_expr ::= ( "GREATEST" | "LEAST" ) "(" expr ( "," expr )* ")"`). They are the
variadic min/max — a single-node grammar form like `COALESCE` (§51), sharing its non-reserved
lookahead, but with three deliberate differences: NULL arguments are **ignored** (not
stop-at-non-NULL), evaluation is **eager** (every argument is evaluated), and — because the fold
actually **compares** its arguments (COALESCE never does) — the common type must be **orderable**
and the comparisons are **metered** (below).

```sql
SELECT GREATEST(a, b, c) FROM t                  -- the row-wise maximum across three columns
SELECT LEAST(price, ceiling) FROM item           -- clamp price down to a ceiling
SELECT GREATEST(low, NULL, high)                 -- NULLs are skipped; = GREATEST(low, high)
```

- **Grammar.** At least one argument (`GREATEST()` / `LEAST()` is a `42601`, PostgreSQL's shape —
  the grammar has no empty form). Both stay **non-reserved**: each form is recognized only when
  the keyword is immediately followed by `(` (the `EXTRACT`/`JSON(` one-token lookahead, §8), so
  `greatest` / `least` remain usable as column names — observably PostgreSQL's `col_name_keyword`
  classification (a column name, never a function name).
- **NULL arguments are ignored** (the PostgreSQL rule, unlike most other systems and unlike a
  plain comparison). The result is `NULL` **only when every argument is NULL** — `GREATEST(1,
  NULL, 2)` is `2`, `LEAST(NULL, NULL)` is `NULL`. This is the opposite of a NULL-propagating
  operator: NULLs do not poison, they drop out of the min/max.
- **Eager — NOT a short-circuit** ([cost.md](cost.md) §3). Every argument is evaluated (all must
  be, to be compared): `GREATEST(1, 1/0)` **traps `22012`**, unlike the lazy `COALESCE(1, 1/0)`.
  So `GREATEST`/`LEAST` are *not* on the sanctioned short-circuit list; they are eager like an
  ordinary scalar function.
- **Argument type unification — CASE's result-arm rule, plus an orderability gate.** The
  arguments unify by exactly CASE's fold of the §25 common-type rule: NULL-typed arguments are
  dropped; an **all-NULL `GREATEST`/`LEAST` is `text`** (PostgreSQL's all-unknown rule); numerics
  promote (decimal if any argument is decimal, else the widest integer; an integer winner widens
  to decimal at eval), **float widths promote to the widest** (the mixed-width `f32`/`f64` set
  widens to `f64` so the comparator sees one width), arrays unify across element integer widths,
  and any other family must be **identical** (same element type for `range`, same shape for
  `composite`). Because the fold compares its winner, the common type must additionally be
  **orderable**, so the resolved type is gated through the comparability matrix — a **non-orderable** type (`json`/`jsonpath`, which ship
  no ordering operator) is **`42883`**, and a cross-family or structurally-mismatched pair
  (including `i32range` × `i64range`, or `float` × `int`/`decimal` — the float island) is
  **`42804`** (`GREATEST/LEAST types must be compatible`). The winner is the max (`GREATEST`) or
  min (`LEAST`) under the unified type's **total order** — the same order `ORDER BY` and `MIN`/`MAX`
  use, so the three never disagree.
- **Collation.** When the common type is `text`, one comparison collation is derived from the
  arguments by the ordinary PostgreSQL rules (`spec/design/collation.md` §1/§7): an explicit
  `COLLATE` dominates, conflicting explicit collations are **`42P21`**, and an indeterminate
  implicit pair is **`42P22`**. The fold then orders text under that collation (byte order when
  none is derived), so `GREATEST`/`LEAST` agree with `ORDER BY` under the same collation.
- **Where it is legal**: anywhere an expression is — projections, `WHERE`, `GROUP BY`/`HAVING`,
  `ORDER BY`, `CHECK` constraints, expression `DEFAULT`s, and **index expressions**
  ([indexes.md](indexes.md) §9 — immutable iff its arguments are, like any pure combinator; an
  index on `GREATEST(a, 0)` matches the same expression in a query and pushes down, and a
  `GREATEST` index never answers a `LEAST` query nor one resolved under a different collation).
  Aggregates nest as in any expression. Because they are a distinct grammar node (not a function
  call), the modifier forms have no place to attach: `GREATEST(*)`, `GREATEST(DISTINCT …)`,
  `GREATEST(…) FILTER (…)`, `GREATEST(VARIADIC …)`, named-argument notation, and `GREATEST(…) OVER
  (…)` are all `42601` at parse — and, not being catalog functions, they carry **no** `EXECUTE`
  privilege (a session that has revoked function `EXECUTE` still runs them, matching PostgreSQL,
  which has no such grantable object).
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the node, plus the cost of **all**
  arguments (each charges its own; a leaf — column or literal — charges nothing), plus, for **each
  comparison after the first** non-NULL argument, the same **size-scaled** work an explicit `<`
  charges — `decimal_work` for wide decimals, `varlen_compare` for `text`/`bytea`/containers, or
  `collate` (proportional to the two operands' code-point lengths) under a derived collation. The
  comparisons are metered — not free — so a `GREATEST` over many large values cannot do unbounded
  comparison work under the cost ceiling (§13). Output name for a bare `SELECT GREATEST(…)` /
  `SELECT LEAST(…)` is **`greatest`** / **`least`** (PostgreSQL) — the fixed keyword lowercased, the
  same no-expression-printer rationale as the §8 rule-4 function names.

## 53. `EXPLAIN` options

`EXPLAIN` accepts both the legacy `EXPLAIN ANALYZE <statement>` spelling and a non-empty,
comma-separated boolean option list:

```sql
EXPLAIN (ANALYZE, VERBOSE TRUE, COSTS OFF, LANE ON) SELECT ...
```

The four option names are `ANALYZE`, `VERBOSE`, `COSTS`, and `LANE`. An omitted value means
`TRUE`; explicit values are `TRUE`, `FALSE`, `ON`, or `OFF`, case-insensitively. Repeating an option,
using an unknown option, writing an empty list, or mixing the legacy positional `ANALYZE` spelling
with a parenthesized list is `42601`. These words remain non-reserved: they acquire option meaning
only between the parentheses immediately following the leading `EXPLAIN`. The result shape and each
option's behavior are specified in [explain.md](explain.md).

## 54. `CREATE VIEW` / `DROP VIEW` / `ALTER VIEW` ([views.md](views.md))

```
CREATE [OR REPLACE] VIEW [IF NOT EXISTS] [main.]name [( col [, …] )] AS subquery_expr
    [WITH [CASCADED | LOCAL] CHECK OPTION]
DROP VIEW [IF EXISTS] name [, …] [CASCADE | RESTRICT]
ALTER VIEW [IF EXISTS] [main.]name RENAME TO new_name
ALTER VIEW [IF EXISTS] [main.]name RENAME [COLUMN] col TO new_col
```

`VIEW` is recognized **positionally** after `CREATE`, `CREATE OR REPLACE`, `DROP`, and `ALTER`, and is
not reserved (§3): `CREATE TABLE view (…)` names a table `view`, and `SELECT view FROM t` reads a
column. The body runs from the token after `AS` to the end of the statement, or to a trailing `WITH
[CASCADED | LOCAL] CHECK OPTION`, and is a `subquery_expr`
(an optional `WITH`, then a query expression with its own `ORDER BY` / `LIMIT`). Its token sequence
is the persisted definition, rendered by the closed per-token table of
[../fileformat/format.md](../fileformat/format.md) *Check-expression text*, after the planner has
frozen every expanded `*` into an explicit, quoted column list ([views.md](views.md) §3/§5). `IF NOT
EXISTS` (a jed extension) and `IF EXISTS` are two-token lookaheads, so `CREATE VIEW if AS …` creates a
view named `if`. `OR REPLACE` with `IF NOT EXISTS` is `42601`. In `ALTER VIEW … RENAME`, `TO` right after
`RENAME` selects the view rename, as in `ALTER TABLE … RENAME` ([alter.md](alter.md) §1), so a column
named `to` is renamed with `RENAME COLUMN to TO x`.

Deferred forms are **recognized** so they fail with `0A000` rather than a syntax error: `CREATE TEMP`/
`TEMPORARY VIEW`, `CREATE RECURSIVE VIEW`, `CREATE MATERIALIZED VIEW`, `CREATE VIEW v WITH ( … )`
(view options), and every `ALTER VIEW` action other than `RENAME`. Each is detected by its keyword
sequence before the body is parsed.

The trailing `WITH [CASCADED | LOCAL] CHECK OPTION` is recognized at the end of the token stream and
cut off before the body is parsed, so the unreserved `with` is never read as a table alias; the rest
must parse as the body (a body syntax error is reported as one). Bare `WITH CHECK OPTION` is
`CASCADED`. The clause gives the view a check option ([views.md](views.md) §15.6) and is not part of
the stored definition. Every error code, the validation order, the namespace
matrix, dependencies, `RENAME` rewrites, and privileges are specified in [views.md](views.md).

A view reference in `FROM` is the equivalent derived table (§42), so §8's output-name rules, §15's
labels and column alias lists, and §42's column-rename rules apply to it unchanged.
An automatically updatable view is also a valid `INSERT` / `UPDATE` / `DELETE` target, with the
statement's grammar unchanged ([views.md](views.md) §15).

## 55. `SUBSTRING` ([string-functions.md §4](string-functions.md))

`SUBSTRING(x FROM start FOR count)` is the SQL special form of `substring`, for `text` and `bytea`.

```sql
SELECT substring(bytea '\x0102030405' FROM 2 FOR 3)  -- \x020304
SELECT substring('hello' FROM 2)                     -- ello   = substring('hello', 2)
SELECT substring('hello' FOR 2)                      -- he     = substring('hello', 1, 2)
SELECT substring('hello' FOR 2 FROM 3)               -- ll     (either order)
```

- **Grammar.** `SUBSTRING "(" expr ( FROM expr [FOR expr] | FOR expr [FROM expr] ) ")"`
  ([grammar.ebnf](../grammar/grammar.ebnf) `substring_expr`). The parser starts an ordinary
  `function_call`; when the callee is an unquoted `substring` and its first positional argument is
  followed by `FROM` or `FOR`, the call switches to the special form. `SUBSTRING` stays an ordinary,
  non-reserved name, and `substring(x, s, l)` is an ordinary call.
- **Desugaring.** The form becomes `substring(x, start)` or `substring(x, start, count)`; a FOR-only
  form uses `start = 1`. This is PostgreSQL's own rewrite, so resolution, NULL handling, errors, cost,
  and the output label (`substring`) are those of the call. No `WITHIN GROUP`, `FILTER`, or `OVER`
  may follow, since the form is not a function application.
- **Not supported.** PostgreSQL also uses this syntax for regular-expression extraction
  (`substring(text FROM pattern)`, `substring(text SIMILAR pattern ESCAPE e)`). jed parses the first
  as this form, where `substring(text, text)` has no overload (`42883`); `SIMILAR` is a syntax error.

## 56. `POSITION` and `OVERLAY` ([string-functions.md §3/§4](string-functions.md))

`POSITION(substring IN string)` and `OVERLAY(string PLACING new FROM start [FOR count])` are the SQL
special forms of `position` and `overlay`, for `text` and `bytea`.

```sql
SELECT position('ig' IN 'high')                          -- 2      = position('high', 'ig')
SELECT position(bytea '\x02' IN bytea '\x010203')        -- 2
SELECT overlay('abcdef' PLACING 'XY' FROM 2 FOR 3)       -- aXYef  = overlay('abcdef', 'XY', 2, 3)
SELECT overlay(bytea '\x0102' PLACING bytea '\xff' FROM 2)  -- \x01ff (FOR defaults to length(new))
```

- **Grammar.** `POSITION "(" concat IN concat ")"` and
  `OVERLAY "(" expr PLACING expr FROM expr [FOR expr] ")"` ([grammar.ebnf](../grammar/grammar.ebnf)
  `position_expr`, `overlay_expr`). Neither word is reserved.
- **`POSITION`.** An unquoted `position(` always starts the special form, as in PostgreSQL, where
  `POSITION` is a column-name keyword: the comma form `position(a, b)` is `42601`. The quoted name
  `"position"(string, substring)` is an ordinary call. Each operand is parsed at the "any other
  operator" level (`||`, `+`, …), one above the predicates, so the operand's own `IN` ends it rather
  than starting an `IN (…)` list. A predicate operand must be parenthesized; PostgreSQL's `b_expr`
  also admits a bare comparison there, but no `position` overload takes a `boolean`.
- **`OVERLAY`.** Like `SUBSTRING` (§55): the parser starts an ordinary `function_call`, and when the
  callee is an unquoted `overlay` and its first positional argument is followed by `PLACING`, the
  call switches to the special form. `FROM` is required (`42601` without it). The comma form
  `overlay(s, new, start[, count])` is an ordinary call, as in PostgreSQL.
- **Desugaring.** `POSITION(sub IN s)` becomes `position(s, sub)` (arguments swapped) and
  `OVERLAY(s PLACING n FROM st [FOR c])` becomes `overlay(s, n, st[, c])` — PostgreSQL's own
  rewrites — so resolution, NULL handling, errors, cost, and the output labels (`position`,
  `overlay`) are those of the calls. No `WITHIN GROUP`, `FILTER`, or `OVER` may follow.

## 57. `CREATE TABLE … AS` ([create-table-as.md](create-table-as.md))

`CREATE [TEMP | TEMPORARY] TABLE [IF NOT EXISTS] [db.]name [( col [, …] )] [ON COMMIT …] AS query
[WITH [NO] DATA]` (`create_table` / `create_table_as` in the grammar) is PostgreSQL's `CREATE TABLE
AS`. The query is a `subquery_expr` — what a derived table or view body accepts (§42). Each output
column becomes a column with the output's name (or the column list's), type, typmod, and collation,
and nothing else (create-table-as.md §2); `WITH DATA` (the default) fills the table exactly as
`INSERT INTO t <query>` would (§24) — rows, cost, errors — and `WITH NO DATA` creates it empty without
running the query.

Recognition after the table name: the keyword `AS`, the keyword `ON` (an `ON COMMIT` clause comes
before `AS`, PostgreSQL's position), or a parenthesized list of bare names followed by `AS` or `ON`.
The column-definition form always has `(` there and a type after each column name, so the two never
overlap and no word becomes reserved. A trailing `WITH DATA` / `WITH NO DATA` ends the statement; it
is cut off before the query is parsed, the CREATE VIEW check-option precedent (§54), because an
unreserved `with` after a `FROM` item would otherwise read as its alias — `AS SELECT … FROM t AS
"with" WITH NO DATA` still parses. Any other trailing word is `42601`. The query's `$N` placeholders
bind as a query's do (create-table-as.md §1); PostgreSQL's `AS TABLE name` / `AS EXECUTE name` forms
are absent. The query's own `WITH` may hold a data-modifying CTE, which makes it the statement's
writable `WITH` (create-table-as.md §1).

`SELECT … INTO [TEMP | TEMPORARY] [TABLE] [db.]name` (`into_clause` after a `select_core`'s select
items) is the same statement spelled PostgreSQL's older way (create-table-as.md §6). Only the
statement's leftmost `SELECT` may carry it — elsewhere `42601` (*INTO is only allowed on first
SELECT of UNION/INTERSECT/EXCEPT* on a later set-operation arm, *SELECT ... INTO is not allowed
here* in any nested query), and `0A000` in a `CREATE VIEW` body. `INTO` is a bare-label stop word
(§8), so after an expression it always begins the clause. `UNLOGGED` (here and in `CREATE UNLOGGED
TABLE`) is `0A000`. `EXPLAIN` accepts `CREATE TABLE … AS` and `SELECT … INTO` (create-table-as.md
§7).

## 58. `NULLIF`

`NULLIF(a, b)` returns **NULL when `a = b`**, and `a` otherwise (`nullif_expr ::= "NULLIF" "(" expr
"," expr ")"`). It is PostgreSQL's `CASE WHEN a = b THEN NULL ELSE a END` with `a` evaluated once —
a single-node grammar form beside `COALESCE` (§51) and `GREATEST`/`LEAST` (§52), sharing their
non-reserved lookahead, because the CASE rewrite would evaluate `a` twice (a second error, a second
`nextval`, a doubled cost).

```sql
SELECT total / NULLIF(count, 0) FROM stats       -- a zero divisor becomes NULL instead of 22012
SELECT NULLIF(trim(name), '') FROM u              -- an empty string becomes NULL
```

- **Grammar.** Exactly two arguments; any other count (`NULLIF(a)`, `NULLIF(a, b, c)`, `NULLIF()`)
  is `42601`, PostgreSQL's shape. `NULLIF` stays **non-reserved**: the form is recognized only when
  the keyword is immediately followed by `(` (the `EXTRACT`/`JSON(` one-token lookahead, §8), so
  `nullif` remains usable as a column name — observably PostgreSQL's `col_name_keyword`
  classification. Being a distinct grammar node (not a catalog function), it has no modifier forms
  and carries **no** `EXECUTE` privilege, like `GREATEST`/`LEAST`.
- **Eager — NOT a short-circuit** ([cost.md](cost.md) §3). Both operands are evaluated, `a` first,
  whatever their values: `NULLIF(1, 1/0)` and `NULLIF(NULL, 1/0)` trap `22012` (PostgreSQL
  evaluates both arguments before comparing).
- **NULLs never match.** The comparison is `=`'s three-valued equality, and only TRUE yields NULL:
  `NULLIF(NULL, b)` is NULL (it is `a`), and `NULLIF(a, NULL)` is `a`.
- **Typing — the `=` operand pair.** The operands resolve exactly as the operands of `a = b`: an
  adaptable literal or bind parameter takes its type from its sibling (`NULLIF(d, '2024-01-01')`
  compares dates; `NULLIF($1, col)` types `$1` as `col`'s type), and the pair must pass `=`'s
  comparability gate — a non-comparable `json`/`jsonpath` is `42883`, a cross-family pair is jed's
  comparison `42804` (PostgreSQL reports `42883` for, e.g., `integer = text`; the error-class
  divergence every jed comparison carries). A text pair derives the comparison collation as `=`
  does, so conflicting explicit collations are `42P21` and an indeterminate implicit pair `42P22`;
  equality itself is byte equality under every deterministic collation
  ([collation.md](collation.md) §7).
- **Result type — `a`'s, after PostgreSQL's `=` resolution.** PostgreSQL's result is the left input
  type of the `=` operator it resolves, so: an unknown (NULL) `a` takes `b`'s type, and an all-NULL
  `NULLIF(NULL, NULL)` is `text`; an integer `a` facing a `decimal` `b` is `decimal` (there is no
  `integer = numeric` operator, so both become numeric — `NULLIF(1, 1.0)` is NULL, `NULLIF(1, 1.5)`
  is the decimal `1`); every other comparable pair keeps `a`'s type, because the integer widths and
  `f32`/`f64` have cross-type `=` operators (`NULLIF(1::i16, 2::i32)` is `i16`; `NULLIF(0.1::f32,
  0::f64)` is the `f32` `0.1`, the pair compared at `f64`). The result keeps `a`'s typmod (an
  integer widened to decimal has none — PostgreSQL's `exprTypmod`) and carries the collation the
  two operands combine to ([collation.md](collation.md) §1).
- **Where it is legal**: anywhere an expression is — projections, `WHERE`, `GROUP BY`/`HAVING`,
  `ORDER BY`, `CHECK` constraints, expression `DEFAULT`s, and **index expressions**
  ([indexes.md](indexes.md) §9 — immutable iff its arguments are; an index on `NULLIF(a, 0)` matches
  the same expression in a query and pushes down). Aggregates nest as in any expression.
- **Cost** ([cost.md](cost.md) §3): one `operator_eval` for the node, plus both operands' own costs,
  plus the size-scaled work of the equality — `decimal_work` for wide decimals and `varlen_compare`
  for `text`/`bytea`, exactly what an explicit `=` charges. Output name for a bare
  `SELECT NULLIF(…)` is **`nullif`** (PostgreSQL) — the fixed keyword lowercased, the same
  no-expression-printer rationale as the §8 rule-4 function names.

## 59. `REINDEX` ([indexes.md §10](indexes.md))

`REINDEX [(option, …)] {INDEX | TABLE} [CONCURRENTLY] [db.]name` and `REINDEX [(option, …)]
{DATABASE | SYSTEM} [CONCURRENTLY] [name]` (`reindex` in the grammar) is PostgreSQL's `REINDEX`.
`REINDEX` is recognized as the leading word; none of the words that follow it is reserved. The
option list holds `VERBOSE` and `CONCURRENTLY`, each with an optional boolean — `TRUE`/`FALSE`/`ON`/
`OFF`, bare or quoted, or the integers `1`/`0`, PostgreSQL's `defGetBoolean` — and `TABLESPACE name`,
which is `0A000`. An unknown option name, or a value that is not a boolean, is `42601`; a repeated
option is accepted and the last value wins (PostgreSQL does not reject repeats here, unlike jed's
`EXPLAIN` option list, §53). The kind word `SCHEMA` is `0A000` (no schemas, §3). The positional
`CONCURRENTLY` is read as the keyword after `INDEX`/`TABLE` only when an identifier follows it —
`REINDEX TABLE concurrently` rebuilds a table named `concurrently` — and always after
`DATABASE`/`SYSTEM`, whose name is optional. A missing target name after `INDEX`/`TABLE` is `42601`.

