# SQL/JSON path language — design

> PostgreSQL's `jsonpath` type and the path-query surface: a first-class scalar type
> (`'$.a[*] ? (@ > 5)'::jsonpath`) compiled once to an internal **program** and run per row
> — the [regex.md](regex.md) Pike-VM precedent — plus the query functions
> (`jsonb_path_exists`/`_match`/`_query`/`_query_array`/`_query_first` and the `_tz`
> variants) and the operators `@?` / `@@`. Evaluation is a deterministic walk producing an
> ordered **sequence** of `jsonb` items; `lax` (default) auto-unwraps arrays and suppresses
> structural-navigation errors, `strict` raises them. This doc is the contract all three
> cores implement in lockstep (CLAUDE.md §2); the type row is data in
> [../types/scalars.toml](../types/scalars.toml), the function/operator catalog in
> [../functions/catalog.toml](../functions/catalog.toml), the error codes in
> [../errors/registry.toml](../errors/registry.toml), and the document type it queries in
> [json.md](json.md). PostgreSQL semantics are the default (CLAUDE.md §1), pinned against the
> live `postgres:18` oracle; the lax/strict edge cases (§4) are subtle and oracle-pinned.

> **Status: landed.** The type, the structural accessors and the any-level accessor `.**`, filters,
> top-level predicates and every predicate form (`exists`, `like_regex`, `starts with`,
> `is unknown`), the query functions (with their `vars` / `silent` arguments and `_tz` forms) and
> operators, the item methods (except the datetime family, which is not supported by decision,
> §3.1), arithmetic with parenthesized expressions, and path variables are implemented in all three
> cores; §9 lists what remains deferred (`0A000`).

---

## 1. Why a first-class type

`jsonpath` is a **new scalar type** (stable `type_code 20`,
[../types/scalars.toml](../types/scalars.toml)), not path strings re-parsed inside each
kernel. This matches PostgreSQL (its `jsonpath` is a real type — you can write
`'$.a'::jsonpath`, store it, and pass it as a parameter) and mirrors jed's own strongest
precedent, the **regex VM** ([regex.md](regex.md)): a path is compiled once to a flat
program and run per row, never re-parsed per row.

- **Compilation seam.** `JsonPath::compile(text) -> Result<JsonPathProgram, Error>`, the
  analogue of `regex::compile`. Hand-written recursive-descent **per core** (a parser — on
  the CLAUDE.md §5 do-not-codegen list), kept byte-identical by conformance + the compiled-
  program shape being a pure function of the source.
- **Storage.** `type_code 20`, **storable but not keyable** (`0A000` as a key, like `float`/
  `json`). The on-disk codec is the **normalized source text** of the parsed path (PG stores
  a binary jsonpath; jed stores the normalized text and recompiles on load — exactly as
  regex recompiles its pattern; a path is small). A `jsonpath` value is **not comparable**
  (`42883`; PG ships no btree opclass for it) — only whole-value `IS [NOT] NULL`.
- **Literal coercion.** A string literal becomes a path by cast (`'$.a'::jsonpath`,
  `jsonpath '$.a'`) at resolve time, routed through `JsonPath::compile`; a malformed path is
  a **`42601`** (syntax error) at resolve, matching PG's syntax-error class for a bad path
  literal. The path *body* needs no new lexer tokens — it is scanned as a string and handed
  to the compiler. New punctuation tokens are needed only for the **operators** `@?`/`@@` (§6).

---

## 2. Grammar subset (PG-faithful, obsolete-free)

EBNF sketch for `JsonPath::compile` (the full grammar lands in
[../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md](grammar.md)):

```
jsonpath     ::= mode? ( path_expr | predicate )
mode         ::= "strict" | "lax"                 (* default: lax *)
path_expr    ::= term ( ( "+" | "-" ) term )*     (* left-associative, §3.2 *)
term         ::= unary ( ( "*" | "/" | "%" ) unary )*
unary        ::= ( "+" | "-" ) unary | accessor_expr
accessor_expr::= primary accessor*
               | "(" path_expr ")" accessor*      (* the accessors continue the inner path *)
               | "(" predicate ")" accessor+      (* the predicate's truth value as an item *)
primary      ::= "$"                              (* root document *)
               | "@"                              (* current item (filter context only) *)
               | "$" varname                      (* named variable: $"x" / $x — bound via vars *)
               | literal                          (* number | string | true | false | null *)
               | "last"                           (* inside a subscript only *)
accessor     ::= "." member                       (* .key  or  ."key with spaces" *)
               | ".*"                             (* wildcard member *)
               | "." "**" ( "{" level ( "to" level )? "}" )?   (* any level, §3.3 *)
               | "[" subscript ( "," subscript )* "]"
               | "[" "*" "]"                      (* wildcard element *)
               | "." method "(" ")"               (* item method, §3.1 *)
               | "." "decimal" "(" ( int ( "," int )? )? ")"
               | "." "datetime" "(" string? ")"
               | "." ( "time" | "time_tz" | "timestamp" | "timestamp_tz" ) "(" uint? ")"
               | "?" "(" predicate ")"            (* filter, §4 *)
int          ::= ( "+" | "-" )? uint              (* an integer literal: 0, or no leading zero *)
level        ::= uint | "last"                    (* uint at most 2147483647 *)
subscript    ::= path_expr ( "to" path_expr )?    (* index, or idx TO idx slice; "last" allowed *)
arith_op     ::= "+" | "-" | "*" | "/" | "%"
predicate    ::= predicate "||" predicate
               | predicate "&&" predicate
               | "!" predicate
               | "(" predicate ")"
               | "exists" "(" path_expr ")"
               | path_expr comparison path_expr
               | path_expr "like_regex" string ( "flag" string )?
               | path_expr "starts" "with" ( string | "$" varname )
               | "(" predicate ")" "is" "unknown"
comparison   ::= "==" | "!=" | "<>" | "<" | "<=" | ">" | ">="
method       ::= "type" | "size" | "double" | "ceiling" | "floor" | "abs" | "keyvalue"
               | "bigint" | "boolean" | "integer" | "number" | "string" | "date"
```

Method names are keywords matched case-insensitively (`$.TYPE()` is `.type()`); whitespace may
separate the `.`, the name, and the `(`. Without the parentheses the same word is a member key
(`$.type` is `$."type"`), as is every other keyword (`$.last`, `$.strict`, `$.true`; §2.1). An unknown method name, arguments where a method takes none, or more
than two `.decimal()` arguments is `42601`.

A literal primary takes accessors too (`"12".number()`, `null.type()`). Number literals follow
PostgreSQL's jsonpath scanner (`1`, `1.5`, `1.`, `.5`, `1e3`, `1.e3`); a number running into a
character that could continue a name (`1.type()`, `01`, `1e`, `1~`, `1.é`) is trailing junk
(`42601`), PostgreSQL's number-plus-one-`other`-byte rules. A sign is an operator, not part
of the number: as in PostgreSQL's grammar, unary `+`/`-` applied to a number literal with no
accessors folds into it (`-1`, `- -1` is `1`, `-(1)` is `-1`), while `-1.5.abs()` is unary minus
applied to `1.5.abs()`, since accessors bind tighter. Like jed's SQL literals (grammar.md §36), jsonpath numbers take no `_` digit separators and no
hex/octal/binary prefixes, which PostgreSQL 16+ accepts.

The operators follow PostgreSQL's `jsonpath_gram.y` precedence, loosest first: `||`, `&&`, `!`, the
comparisons, binary `+ -`, `* / %`, unary `+ -`, then accessors. `||` and `&&` take predicates and
the arithmetic operators and comparisons take expressions, so a misplaced one is `42601`
(`$.a || $.b`, `($.a == 1) + 1`, `$ ? (@ + 1)`, `($.a == 1) == true`). Parentheses group: around an
expression they add nothing (`(($.a))` is `$.a`), and accessors after them continue the inner path
(`($.a).b` is `$.a.b`, `(1 + 2).type()` the type of the sum). A parenthesized predicate is a
predicate unless accessors follow it, when it is an expression whose one item is its truth value
(`($.a == 1).type()` is `"boolean"`; unknown is `null`). A subscript is any expression, in which
`last` names the subscripted array's last index (`$[last - 1]`, `$[$.i to last]`); `last` outside a
subscript is `42601`.

The predicate keywords (`exists`, `like_regex`, `flag`, `starts`, `with`, `is`, `unknown`) are
matched case-insensitively and may be separated by whitespace or comments (`@ STARTS  WITH "a"`). Each
predicate form takes PostgreSQL's operands exactly: `exists` an expression in parentheses (not a
predicate: `exists($ == 1)` is `42601`); `like_regex` a string literal pattern and an optional string
literal flag string (a variable in either place is `42601`); `starts with` a string literal or a
variable (`@ starts with @.a`, `starts with 1`, `starts with ("a")` are `42601`); `is unknown` a
parenthesized predicate directly before it (`@ > 1 is unknown`, `!(@ > 1) is unknown`,
`exists(@) is unknown` are `42601`, while `((@ > 1) is unknown) is unknown` is accepted). `!` applies
to a parenthesized predicate or to `exists (…)` (`!exists(@.a)`).

The any-level accessor is `.` then `**` (whitespace between them is allowed, `$ . **`), optionally
followed by `{level}` or `{level to level}`, where a level is an unsigned integer literal or `last`
(`$.** {2 to last}`; `{}`, `{1,2}`, `{1 to}`, `{+1}`, `{1.0}`, `{1e1}`, `{$x}` are `42601`, and `{01}`
is trailing junk, `42601`). Like a number literal, a level takes no `_` digit separators and no
hex/octal/binary prefix (`{1_000}`, `{0x10}` are `42601`; PostgreSQL 16+ accepts them — the same
divergence as the number literals above). A level above 2147483647 is **`22003`** (`value "…" is out of range for
type integer`), PostgreSQL's `pg_strtoint32` error, raised at compile. `**` after anything but `.`
(`$**`, `$.a**`, `**`) is `42601`. `**` is one token, so `$.***` is `$.**` followed by a stray `*`
(`42601`), and `$.*** 2` is `$.** * 2`, as in PostgreSQL.

Faithful inclusions: `last` (array-length-minus-1 sentinel inside a subscript), `to` slices,
named variables `$name` (bound by the `vars` argument, §5), `.datetime(template?)`,
`like_regex` with an optional `flag` clause (§4.3), `.**` with its level bounds (§3.3). Nothing in
PG's path grammar is excluded as obsolete; the only narrowings are the `like_regex` flag set and
pattern language (§4.3), consequences of jed's deliberate regex subset.

### 2.1 String escapes

A quoted string (a `."key"` member or a string literal) and an unquoted name (§2.2) take PostgreSQL's
jsonpath scanner escapes (`jsonpath_scan.l`), which are wider than JSON's:

- `\b \f \n \r \t \v` are the control characters.
- `\uXXXX` (exactly four hex digits) and `\u{X…}` (one to six) are code points. A run of consecutive
  `\u` escapes is one token: a high surrogate must be followed, within the run, by a low surrogate,
  and the pair combines (`\ud83d\ude00`, `\u{D800}\u{DC00}`).
- `\xHH` (exactly two hex digits) is a code point.
- A backslash before any other character is that character (`\"`, `\\`, `\/`, `\q`).

The errors are PostgreSQL's. A malformed `\u` or `\x`, or a backslash ending the input, is `42601`.
Because the run is lexed whole first, a malformed escape anywhere in it wins over a decode error
earlier in it. A code point above U+10FFFF is `42601` (`invalid Unicode code point`). An unpaired
surrogate is `22P02` `invalid input syntax for type jsonpath` with the Detail `Unicode low surrogate
must follow a high surrogate.` or `Unicode high surrogate must not follow a high surrogate.`. A NUL,
however spelled (`\u0000`, `\u{0}`, `\x00`), is `22P05` `unsupported Unicode escape sequence` with
the Detail `\u0000 cannot be converted to text.`, as for JSON input ([json.md](json.md) §4.2).

A member key renders quoted, escaped as PostgreSQL's `escape_json` does: `\"`, `\\`, `\b \f \n \r
\t`, and any other control character as `\u00XX` (`$.a\v` renders `$."a\u000b"`).

### 2.2 Unquoted names and keywords

An unquoted name (`$.key`) is PostgreSQL's non-quoted string token: a run of any characters outside
the *special* set `? % $ . [ ] { } ( ) | & ! = < > @ # , * : - + /`, whitespace (`[ \t\n\r\f]`),
and `"`, together with the §2.1 backslash escapes. Non-ASCII characters belong to a name (`$.é`,
`$.a€`), as does any other punctuation (`$.a~b`, `$.a;b`). A special character, whitespace, or `"`
ends the name, so `$.a-b` is `$.a - b`, `$.a$b` is `$.a` followed by the variable `$b`, and
`$.a"b"` is a name followed by a string (both syntax errors). An escaped character never ends the
name (`$.a\u002eb` is the key `a.b`, `$.a\ b` the key `a b`). The escape errors are §2.1's: a NUL
is `22P05`, an unpaired surrogate `22P02`, a malformed escape or a backslash ending the input
`42601`. A name is lexed before the grammar sees it, so its escape error comes first even where a
name is not allowed (`$.a \u0000` is `22P05`).

The scanner's numbers compete with names by flex's longest match (in bytes). Right after the `.` a
digit starts a number (`.5`), so `$.5` and `$.1abc` are `42601`. After whitespace, a digit-led run
is a name only when it is longer than every number token there, counting PostgreSQL's
trailing-junk tokens (a number plus one byte) and its `0x`/`0o`/`0b` and `_`-separated forms, which
jed's own number literals do not accept (§2): `$. 1abc`, `$. 0x1Fz`, `$. 12é` are names, `$. 12`,
`$. 1a`, `$. 1e3` are not.

A keyword is a name whose decoded text spells one of PostgreSQL's jsonpath keywords, compared
ASCII-case-insensitively except `null`, `true`, and `false`, which are lowercase only (`STRICT $`,
`$[LAST]`, and `$[l\u0061st]` are keywords; `TRUE` is a name). After `.` every keyword is a member key
(PostgreSQL's `key_name`), and a method keyword followed by `(` is an item method (§2). Elsewhere a
name is valid only as a keyword the grammar expects there; any other (`abc`, `$ ? (@ == abc)`,
`$.a is unknown`) is a syntax error (`42601`). An unquoted variable name (`$name`) is a run of the same
characters without escapes, so a special character ends it (`$a.b` is `$a` then `.b`, `$a$b` a syntax
error) while a digit or non-ASCII character does not (`$1abc`, `$é`); a quoted one (`$"a b"`) takes the
§2.1 escapes. A `$` followed by anything else is the root. A variable renders as `$` and its name
quoted like a member key (`$x` is `$"x"`).

A comment that a name runs straight into discards the name (§2.4).

### 2.3 Canonical render

A path renders as PostgreSQL's `jsonpath_out` prints it: `lax` omitted and `strict` kept, member keys
quoted (§2.1), item methods lowercase, operators spaced (`$."a" + 1`, `@ == 1`, `a && b`), and a
number literal followed by an accessor parenthesized (`(1.5).type()`). Operators are parenthesized
by `printJsonPathItem`'s rule: the top-level expression or predicate is wrapped (`($."a" + 1)`,
`($."a" == 1)`, but not `!(…)` or a plain path), and an operand is wrapped when its operator binds no
tighter than its parent's, by priority `||` 0, `&&` 1, comparisons 2, `+ -` 3, `* / %` 4, unary 5
(`((1 + 2) + 3)`, `(1 - (2 - 3))`, `(1 + 2 * 3)`, `(-(1 + 2))`, `((a || b) && c)`). Filter
predicates and subscripts are not wrapped at their own top level (`$?(@ + 1 > 2)`, `$[last - 1]`).

The predicate forms print as `printJsonPathItem` does. `starts with` is a comparison (priority 2):
`($ starts with "a")` at the top level, `$?(@ starts with $"p")`, and its left operand bracketed by
priority (`($ + 1 starts with "a")`). `like_regex` is wrapped like an operator at the top level
(`($."a" like_regex "x")`) and wraps its operand whenever that operand is an operator
(`$?(($."a" + 1) like_regex "x")`); the pattern prints as a quoted string (§2.1) and the flags, when
any are set, as ` flag "…"` in the fixed order `i s m x q` with duplicates dropped (`flag "qiqi"` prints
`flag "iq"`, `flag ""` prints nothing). `exists` prints `exists (…)` and `is unknown` prints
`(…) is unknown`; neither is wrapped further, even at the top level (`exists ($."a")`,
`(1 > "a") is unknown`). `.**` prints `.**`, `.**{n}`, `.**{n to m}`, `.**{last}`, `.**{n to last}`, or
`.**{last to n}`, with `{0 to last}` collapsing to `.**` and `{n to n}` to `{n}`.

**Divergence: an operator followed by accessors is always parenthesized.** PostgreSQL decides the
brackets by priority alone, so as an operand it prints `($.a + 1).type() == "number"` as
`$."a" + 1.type() == "number"` (which does not parse) and `(-@.a).abs()` as `-@."a".abs()` (which
parses as a different path). jed evaluates a path from its canonical text (§1, §7), so that text
must read back as the same path; it renders `($."a" + 1).type()` and `(-@."a").abs()` wherever they
appear (an oracle override). The same holds for the predicates that print without their own brackets:
PostgreSQL prints `(exists ($[*])).type()` as `exists ($[*]).type()`, `(($ > 1) is unknown).type()` as
`($ > 1) is unknown.type()`, and `(!($ > 1)).type()` as `!($ > 1).type()`, none of which parse; jed
keeps the outer parentheses.

### 2.4 Comments and empty input

PostgreSQL's scanner skips a C-style `/* … */` comment between tokens as it skips whitespace
(`$ /* the root */ .a`, `1 /**/ + /**/ 2`, `$.type(/**/)`), and the canonical render (§2.3) drops it.
A comment may hold anything but `*/` (quotes, newlines, `/*`); comments do not nest, so in
`$ /* a /* b */ */` the first `*/` ends the comment and the stray `*/` is a syntax error. A comment
without its `*/` is `42601` (`unexpected end of comment`). Inside a quoted string or quoted variable
name `/*` is literal text (`$."a/*b*/"`). `/` beside a comment stays the division operator:
`$.a//**/2` is `$."a" / 2`, and `$.a /**/* 2` is `$."a" * 2`, since the comment ends at its first
`*/`.

The scanner's non-quoted-string state (§2.2) enters the comment state at `/*` without returning the
name it was building, so **a name that runs straight into a comment is discarded**: `$.a/*c*/b` is
`$."b"`, `$.a/*c*/` is `$.` (`42601`), `strict/**/$` is `$` (lax), `$.type/**/()` is `$.()`
(`42601`), and `$. 1abc/**/x` is `$."x"`. The name is still lexed, so its escape error is raised
(`$.a\u0000/**/b` is `22P05`). Whitespace before the comment ends the name first, keeping it
(`$.a /**/`, `strict /**/ $`). A number, variable, or quoted string before a comment is kept
(`1/**/`, `$x/**/`, `"a"/**/`), and an escaped `/` belongs to the name, so `\/*` does not open a
comment. jed keeps this quirk as PostgreSQL's behavior.

A path with no tokens at all — empty, or only whitespace, comments, and discarded names (`''`,
`'/**/'`, `'strict/**/'`) — is PostgreSQL's empty grammar result: `22P02` `invalid input syntax for
type jsonpath`. A mode keyword with nothing after it (`strict`, `lax /**/`) is `42601`.

---

## 3. Evaluation model — an ordered sequence of `jsonb` items

A compiled path evaluates over a context item to an **ordered SQL/JSON sequence** of `jsonb`
items. This single abstraction unifies accessors, methods, filters, and the query functions
(§5):

- **Seed.** `$` seeds the sequence with the single context item; `@` (filter context) seeds
  with the current item under test; `$name` seeds with the bound variable's value.
- **Composition.** Each accessor is a function `seq → seq`, applied left to right:
  - `.key` maps each **object** item to its member value (and, in lax, unwraps an array
    first — §4.1); a missing key contributes no item (lax) or raises (strict).
  - `.*` maps each object item to all its member values.
  - `.**` maps each item to itself and its descendants, within level bounds (§3.3).
  - `[i]` selects element `i` (negative / `last` allowed); `[i to j]` a contiguous slice;
    `[*]` all elements; on a non-array, lax treats the item as a singleton array (§4.1),
    strict raises.
  - `?(predicate)` keeps each item for which the predicate is definitely true (in lax,
    unwrapping an array item one level first — §4.1).
  - `.method()` maps each item through the method (§3.1).
- **Result.** The query functions (§5) interpret the final sequence: exists = non-empty;
  query = one row per item; query_first = first or NULL; match = the sequence must be a
  single boolean.
- **Depth-first order.** Like PostgreSQL's executor, each item a step emits runs through the rest of
  the path before the step emits its next item. The sequence is the same as applying the steps one
  after another, but the order fixes which error is raised first, lets an existence test stop early
  (§5), and numbers `.keyvalue()` objects (§3.1). Each core runs the steps on an explicit stack of
  step applications, so a long path does not deepen the native stack.

Determinism is automatic: `jsonb` object members are stored in canonical key order
([json.md §2.3](json.md)) and array order is preserved, so `.*` and `.keyvalue()` emit in a
deterministic order across cores.

### 3.1 Item methods

PostgreSQL's item methods, ported from `executeItemOptUnwrapTarget`. In lax mode every method
except `.type()` and `.size()` applies to each element of an array item (one level — an element
that is itself an array is not unwrapped again, so it fails the method's kind check). A method's
result feeds the next step; a method follows `$`, `@`, or a literal.

| method | applies to | result | failure (code, message) |
|---|---|---|---|
| `.type()` | any | its JSON type name: `"null"` `"boolean"` `"number"` `"string"` `"array"` `"object"` | — |
| `.size()` | any | an array's element count; lax: `1` for a non-array | strict non-array: `22039` |
| `.abs()` `.floor()` `.ceiling()` | number | `numeric` `abs`/`floor`/`ceil` (floor/ceiling at scale 0) | `22036` "can only be applied to a numeric value" |
| `.double()` | number, string | a number: the item itself, once its text reads as a finite double; a string: read as a double, then `float8_numeric` | `22036`: out of range (either way, `1e400`, `"1e-400"`) or unreadable "argument … is invalid for type double precision"; NaN/±Infinity; other kinds |
| `.bigint()` `.integer()` | number, string | a number rounds half away from zero; a string is read with jed's integer grammar; must fit i64 / i32 | `22036` "argument … is invalid for type bigint/integer"; other kinds |
| `.boolean()` | boolean, number, string | a number's text must read as an i32 (`1.0` fails), nonzero is true; a string by PostgreSQL's `parse_bool` (any prefix of `true`/`false`/`yes`/`no`, `on`, `off`/`of`, `1`, `0`, case-insensitive, no surrounding space) | `22036` "argument … is invalid for type boolean"; null/array/object |
| `.number()` `.decimal()` | number, string | a number: the item itself; a string: read with jed's decimal grammar (keeping its written scale) | `22036`: unreadable, NaN/Infinity, other kinds |
| `.decimal(p [, s])` | number, string | rounded half away from zero to scale `s` (default 0; negative rounds left of the point), then fewer than `p − s` digits before the point | `22036` overflow; precision/scale outside the integer range `22036`; precision outside 1..1000 or scale outside −1000..1000 `22023` |
| `.string()` | boolean, number, string | `"true"`/`"false"`, the number's text, the string | `22036` null/array/object |
| `.keyvalue()` | object | one `{"id", "key", "value"}` object per member, in key order | `2203C` non-object (both modes) |

The failures are coercion failures: lax mode does **not** suppress them (§4.2). `22036` messages
follow PostgreSQL's ("argument "x" of jsonpath item method .double() is invalid for type double
precision", "jsonpath item method .abs() can only be applied to a numeric value", "NaN or Infinity
is not allowed for jsonpath item method .number()").

**`.double()` on a string** reads the text with `float8in` (surrounding whitespace, sign, digits,
point, exponent; a nonzero mantissa that underflows to zero is out of range, a subnormal is kept)
and converts the double back with PostgreSQL's `float8_numeric`: `%.15g`, the double's *exact*
decimal expansion rounded to 15 significant digits with ties to even (glibc), trailing fractional
zeros dropped, read back at scale `max(0, digits − exponent)`. `"0.1"` → `0.1`, `"1e20"` →
`100000000000000000000`, `"1234567890123455"` → `1234567890123460`. Exact integer arithmetic on
the binary64 value, so every core agrees; no float-render ledger entry is needed. A number item is
returned as written — PostgreSQL validates it and keeps the `numeric`.

**Number grammars.** `.bigint()`/`.integer()`/`.boolean()`/`.number()`/`.decimal()` read strings
with jed's own integer and decimal grammars (grammar.md §36) and `.double()` with jed's float
grammar (float.md §4). PostgreSQL 16+ also accepts `_` digit separators and hex/octal/binary
integers (`"0x10".bigint()` is `16`), and its `float8in` accepts glibc's hex floats; jed reports
`22036` for those, the same documented divergence as its SQL literals (a per-core test).

**`.keyvalue()` ids.** `"id"` identifies the object a pair came from: PostgreSQL computes
`10^10 × base + offset`, where `offset` is the object's byte offset inside its *base object* in
PostgreSQL's binary jsonb layout. The base is the context item (number 0) while a `$` path runs, a
variable's base object while a `$name` path runs (§5: the whole `vars` object, number 1, or the
`PASSING` value itself, numbered by its position in the clause), and the `.keyvalue()` object whose
continuation is running for anything reached through one. `.keyvalue()` objects are numbered in
evaluation order (depth-first, §3), starting one past the variable base objects the caller
supplies: 2 for the `jsonb_path_*` functions (their `vars` object counts as one, given or
defaulted), 1 for `@?`/`@@`, and one past the number of `PASSING` values for the SQL/JSON functions
and `JSON_TABLE`. The layout is a pure
function of the value, so jed computes it arithmetically: a container starts 4-byte aligned with a
4-byte header and one 4-byte entry per array element (two per object member), then the members'
key bytes, then the element/value data; a string takes its UTF-8 bytes, `null`/`true`/`false` take
none, and a number takes its aligned `numeric` datum — a 4-byte varlena header, a 2-byte (short)
or 4-byte (long) header, and 2 bytes per base-10000 digit group from the most to the least
significant nonzero group (short when the display scale ≤ 63 and the leading group's weight is in
−64..63). A `.keyvalue()` object `{"id": i, "key": k, "value": v}` lays out its value at
`align4(40 + size(i) + len(k))`. So `$[*].keyvalue()` over `[{"a": 1, "b": [1, 2]}, {"c": …}]`
gives ids 12 and 72, and `$.keyvalue().value.keyvalue()` under `jsonb_path_query` gives
`20000000048` for the first pair's value. A `.keyvalue()` object is an ordinary object to every
other step (`.key`, `.*`, `.type()`, filters, comparisons); it is built as `jsonb` only when it is
returned.

**Not supported, by decision (`0A000`, decided 2026-10-13).** The datetime methods
`.datetime([template])`, `.date()`, `.time([p])`, `.time_tz([p])`, `.timestamp([p])`, and
`.timestamp_tz([p])` are not part of jed's surface ([CLAUDE.md §1](../../CLAUDE.md): we own our
surface), and jed's jsonpath has no datetime items at all. To use a date or time held in JSON, extract it and cast it: `(doc->>'d')::timestamptz`, or
`JSON_VALUE(doc, '$.d' RETURNING timestamptz)`. That is the whole answer in MySQL, SQL Server,
DuckDB, and SQLite, none of which has these methods.

Why. PostgreSQL implements the methods by reusing its `to_timestamp` engine, even without a
template (it tries a fixed list of ISO templates), so exact compatibility means porting that
engine's field-reading rules (`"20-01-05"` is year 20, `"10:00:00.+05"` reads microseconds) to every
core, and loading PostgreSQL's time zone abbreviation table (`"10:00:00PST"`), which is a server
setting rather than data and which jed does not load (timezones.md §15). The methods would also
bring datetime items with their own `.type()` and cross-type comparison rules, a session-zone path
for the `_tz` functions (§5.1), and a `time`/`timetz` type jed does not have. That is a large,
quirk-bound surface for a convenience the extract-and-cast form already covers. Other options were
weighed and set aside with it: a strict ISO 8601 subset that errors wherever PostgreSQL's reading
differs (no abbreviations, numeric offsets only), optionally with the SQL standard's small template
language for `.datetime(template)`. The conformance suite `json/json_datetime_extract.test` pins
both the `0A000` and the extract-and-cast form.

**Revisitable.** This is a scope decision, not a technical barrier. If a real need appears, the
strict-subset option is the likely path, and it gets cheaper if SQL `to_timestamp` is built for its
own sake. Reopening it also reopens the date/time `PASSING` value (json-sql-functions.md §5.4) and
the `_tz` functions' session-zone comparisons (§5.1).

### 3.2 Arithmetic

PostgreSQL's `executeBinaryArithmExpr` and `executeUnaryArithmExpr`, over the current item (`@`) and
the context item (`$`) of the enclosing expression.

- **Binary `+ - * / %`.** Both operands are evaluated, left then right, to their whole sequences; in
  lax an array item of either is unwrapped one level (§4.1 rule 4). Then the left and then the right
  must be exactly one number, else `22038` `left operand of jsonpath operator + is not a single
  numeric value` (`right …`, the operator as written). So `$.a + 1` is `2` over `{"a": [1]}` in lax,
  and `22038` in strict or over `{"a": [1, 2]}`; a missing member is an empty operand (`22038` in
  lax, the navigation error `2203A` in strict). The result is one number, computed as `decimal`
  arithmetic, which is PostgreSQL's `numeric` ([decimal.md](decimal.md)): the division scale of
  `select_div_scale` (`1 / 3` is `0.33333333333333333333`), `%` with the dividend's sign
  (`-7 % 3` is `-1`), `22012` on a zero divisor, `22003` past the format cap.
- **Unary `+ -`.** The operand is evaluated to its sequence (lax unwraps an array item one level),
  and the operator applies to each item in turn, its result running through the rest of the path
  before the next item (§3): `-$[*]` over `[1, -2]` is `-1, 2`. A non-number is `2203B` `operand of
  unary jsonpath operator - is not a numeric value` when it is reached — except in a lax existence
  test (§5) with no accessor after the operator, which skips it, as PostgreSQL does
  (`jsonb_path_exists('["x", 1]', '-$[*]')` is true).
- **Where errors go.** Arithmetic errors are SQL/JSON errors. Inside a comparison operand they make
  the comparison unknown (§4.4: `$ ? (10 / @ > 4)` drops a `0`); the silent operators end the
  evaluation on them (§4.2: `'{"a": 1}' @? '$.a / 0'` is NULL); elsewhere they raise.
- **Subscripts.** A subscript expression is evaluated (without unwrapping) with `last` naming the
  subscripted array's last index — for the subscripts and for every continuation of their elements,
  as PostgreSQL's `innermostArraySize` — and must be one number, else `22033` `jsonpath array
  subscript is not a single numeric value`. An index, literal or computed, is truncated toward zero
  (`$[1.9]` is `$[1]`) and must fit the integer range (`22033`).

### 3.3 The any-level accessor `.**`

`.**{first to last}` emits the item it is applied to and its descendants whose **level** lies in
`[first, last]`: the item is level 0, its elements or member values level 1, theirs level 2, and so
on. Bare `.**` is `{0 to last}`, `{n}` is `{n to n}`, and `last` as a bound means unbounded. Order is
PostgreSQL's `executeAnyItem`: the item first (when `first` is 0), then a depth-first preorder walk
in stored order (array elements in order, object members in canonical key order): each child is
emitted before the walk descends into it, and each emitted item runs through the rest of the path
before the walk continues (§3, depth-first). The walk descends into a child only when the child's
level is below `last`, so `{1}` visits only the item's children.

- **Containers.** A document array or object, or a `.keyvalue()` object (whose members `id`, `key`,
  `value` are its level-1 children), is a container; any other item has no descendants. A scalar
  context item is not a container (`jsonb_path_query('1', '$.**')` is one `1`).
- **`{last}` means leaves.** PostgreSQL stores `last` as the largest level, so `{last}` (and
  `{last to last}`) emits every descendant that is not a container, at any level ≥ 1, and never
  the item itself (`'1'` gives nothing). `{last to n}` emits nothing, and neither does `{n to m}`
  with `n > m`.
- **No unwrapping of the step.** `.**` applies to an array as it is (lax does not unwrap it first,
  §4.1), but every emitted item runs the rest of the path with lax's usual unwrapping, so
  `lax $.**.a` over `{"a":[{"a":1}]}` emits `[{"a":1}]`, `1`, `1`: once from the unwrapped array,
  once from the object inside it.
- **Structural errors are suppressed below it, in either mode.** Every item `.**` emits runs the
  rest of its path with navigation errors suppressed (§4.2 rule 5 as in lax), even in strict:
  `strict $.**.a` over `{"a":{"a":1}}` is `{"a":1}`, `1`, with no error for the object that has no
  `a` or the number that is not an object. In strict the suppressed step emits nothing — a
  subscript or `[*]` on a non-array does not treat it as a singleton, `.size()` of a non-array emits
  nothing, and an out-of-bounds subscript is clamped. The suppression covers filter operands too:
  `strict $.** ? (!(@.x == 1))` keeps every item that has no `x`, because a missing `x` is an empty
  operand (false) rather than an error (unknown). Everything else keeps strict's meaning — array
  unwrapping, the comparison pairing of §4.4, an existence test's evaluation of all items — and
  item-method coercion errors, `.keyvalue()` of a non-object, and an out-of-integer-range subscript
  still raise (`strict $.**.double()` over `[1]` raises for the array).

---

## 4. `lax` (default) vs `strict`

Lax differs from strict in exactly two ways: **automatic array unwrapping** and
**structural-error suppression**. Stated operationally:

### 4.1 Automatic unwrapping (lax only)

Unwrapping is **one level**, applied to each item *before* the step that unwraps it — PG does not
transitively flatten nested arrays in one shot (an element that is itself an array reaches the step
as-is).

1. **Member accessor or filter on an array.** In lax, `.key` / `.*` / `?(…)` applied to an array
   first **unwraps it one level** — the step is applied to each element and the results
   concatenated. (`lax $.a` over `[{"a":1},{"a":2}]` → `1, 2`; `lax $ ? (@ == 1)` over `[1,2]` →
   `1`, and over `[1]` the filter tests `1`, not `[1]`.) Strict: a member accessor on an array is a
   structural error, and a filter tests the array itself as `@` (`strict $ ? (@[0] == 1)` over
   `[1,2]` → `[1,2]`). A filter is unwrapped wherever it appears — after `$`, after another step
   (`$.a ? (…)`), or after another filter.
2. **Element accessor on a non-array.** In lax, `[i]` / `[*]` on a non-array treats the item as a
   **singleton array** `[item]` first (`lax $[0]` over a scalar → the scalar). Strict: a
   structural error.
3. **`.size()` on a non-array** is `1` in lax (the implicit singleton); strict requires an array.
4. **Comparison operands.** In lax, each `@`/`$`-rooted operand of a predicate comparison (inside a
   filter or a top-level predicate) evaluates to a sequence whose array items are **unwrapped one
   level** before the existential comparison, so `lax $ ? (@.a == 2)` keeps `{"a":[1,2]}` and
   `lax $ == 2` over `[1,2]` is true. Combined with rule 1, `lax $ ? (@ == 1)` over
   `[1,2,[1],[[1]]]` → `1, [1]`: the filter hands `[1]` to the predicate as `@`, whose operand
   unwrap reaches `1`; `[[1]]` unwraps only to `[1]`, which is not comparable to a scalar. Strict:
   an array operand is compared as an array — not comparable to a scalar, so unknown.

### 4.2 Structural-error suppression (lax only)

5. A **navigation** failure — a missing object member, an out-of-range subscript, a type
   mismatch on a structural step — contributes **no item** in lax (the sequence is just
   shorter), where strict raises: `2203A` for a missing member or a member accessor on a
   non-object, `2203C` for `.*` on a non-object, `22039` for `[…]`/`[*]`/`.size()` on a
   non-array, and `22033` for a subscript out of bounds — including a strict slice that runs past
   either end or is reversed (`strict $[1 to 10]` over three elements). A subscript outside the
   integer range is `22033` in both modes.
6. **Item-method coercion failures are NOT suppressed** even in lax (§3.1). Lax suppresses
   *navigation* failures, not *coercion* failures.
7. **Below `.**`, strict suppresses navigation failures too** (§3.3).

**Silent mode** is **orthogonal**: it suppresses the SQL/JSON (class `22`) errors that remain,
ending the evaluation at the first one. `@?` then returns SQL NULL; `@@` decides on the items found
before the error (`'[true, "x"]'::jsonb @@ '$[*].boolean()'` is true). Filter operands are always
evaluated silently (§4.4 rule 1). A cost, budget, or cancellation error is never suppressed, nor is
a missing variable (`42704`) or a non-object `vars` argument (`22023`, §5), which are not SQL/JSON
errors. The query functions' `silent => true` argument (§5) selects the same mode.

### 4.3 `like_regex` onto the Pike VM

`like_regex` maps to jed's regex VM ([regex.md](regex.md)): the pattern is compiled once when the
path is compiled, and each string item is tested with `Program::is_match` (captures ignored, the `~`
operator's use). The pattern language is jed's regex subset (regex.md §2), not PostgreSQL's ARE, so a
pattern using an ARE-only construct (`[[:alpha:]]`, `\y`, `\uHHHH`, `(?i)`) is **`2201B`** where
PostgreSQL accepts it — the same documented divergence as the `~` operator. A malformed pattern is
`2201B` and an oversized program `54001`, both raised when the path is compiled (at resolve for a
literal), as PostgreSQL validates the pattern in its grammar.

The XQuery `flag` string is translated as PostgreSQL's `jspConvertRegexFlags` does:

- **No flag** — the newline-sensitive mode (regex.md §8d, PostgreSQL's `REG_NLSTOP`): `.` and a
  negated bracket class `[^…]` do not match `\n`; `^`/`$` anchor the whole string. (The `~` operator
  lets `[^…]` match `\n`.)
- **`i`** (case-insensitive) — the `~*` simple-lowercase fold of the pattern and of each item
  (regex.md §1).
- **`s`** (dot-all) — `.` and `[^…]` match `\n` (regex.md §8d).
- **`q`** (quote) — the pattern is a literal string (regex.md §8d); `s`, `m`, and `x` are then
  ignored, as XQuery specifies.
- **`m`** (multi-line `^`/`$`) — **unsupported, `0A000`**, raised at compile unless `q` is also set: jed's
  VM has no line anchors. A documented divergence; PostgreSQL supports it.
- **`x`** (XQuery whitespace) — **`0A000`** unless `q` is set, as in PostgreSQL (`XQuery "x" flag
  (expanded regular expressions) is not implemented`).

Any other flag character is **`42601`** (`Unrecognized flag character "z" in LIKE_REGEX predicate.`).
Duplicate flags are allowed. The canonical text keeps the flags as a set (§2.3), so `flag "m"` with
`q` reads back with both.

### 4.4 Predicate truth values

A predicate is three-valued: **true**, **false**, or **unknown**. A comparison `l op r` is
existential over every pair `(a, b)` of items from the two operand sequences (each a `@`/`$`/literal
path's items, unwrapped one level in lax — §4.1 rule 4 — or a single literal):

1. **Operand errors.** A SQL/JSON error while evaluating an operand — a strict navigation error
   (a missing member, an out-of-range subscript, an accessor on the wrong type) or, in either
   mode, an item-method failure (`@.double()` on `"x"`) — makes the comparison **unknown**; it
   never propagates (§4.2). The left operand is evaluated first; when it fails the right one is
   not evaluated. In lax a navigation produces an **empty** sequence instead, and an empty operand
   makes the comparison **false** — so `lax $ ? (!(@.a == 1))` keeps an item that has no `a`, and
   `strict` does not.
2. **Pairs.** Two scalars of the same type compare by that type's order — numbers numerically,
   strings by code point, `false < true`, and `null` equal to `null` — under every operator
   (`true > false` and `null <= null` are true). `null` against any other item, including an array
   or object, is **definite**: `!=`/`<>` is true and every other operator false. Every other pair
   — scalars of different non-null types, or two arrays/objects — is **unknown**.
3. **Combining pairs.** **Lax:** true if any pair is true; otherwise unknown if any pair is unknown;
   otherwise false. **Strict:** unknown if any pair is unknown, even when another pair is true;
   otherwise true if any pair is true; otherwise false.
4. **Connectives.** `&&`, `||`, and `!` are Kleene (`unknown || true` is true, `unknown && false`
   is false, `!unknown` is unknown).
5. **`starts with` and `like_regex`** are existential like a comparison (PostgreSQL's
   `executePredicate`): the left operand is evaluated to its sequence (lax unwraps an array item one
   level), an error in it makes the predicate unknown, and the pairs are combined by rule 3. For
   `starts with` each pair is (left item, right item), the right operand — a string literal or a
   variable — is **not** unwrapped, and a pair is true when both are strings and the left begins with
   the right (code point by code point, so `""` is a prefix of every string), false when both are
   strings otherwise, and unknown otherwise (a non-string item, or an array variable). For
   `like_regex` each left item alone is a pair: true when it is a string the pattern matches, false
   when it is a string it does not, unknown otherwise. A missing variable is `42704` (§5), never
   unknown.
6. **`exists (e)`** is true when `e`'s sequence is non-empty, false when empty, and unknown when
   evaluating it raises a SQL/JSON error. Lax stops at the first item (§5's early stop: an error in a
   later item is never reached, so `lax exists(@[*].double())` over `[1, "a"]` is true); strict
   evaluates the whole sequence, so any error makes it unknown. The operand is not unwrapped.
7. **`(p) is unknown`** is true when `p` is unknown and false otherwise; it is never unknown.
8. **Results.** A filter keeps an item only when its predicate is true. A top-level predicate
   yields one item: `true`, `false`, or JSON `null` for unknown; `jsonb_path_match` and `@@` turn
   that `null` into SQL NULL.

---

## 5. Path query functions

Five functions (one set-returning) + their `_tz` variants (§5.1), plus two operators (§6). Each
takes PostgreSQL's parameters `(target jsonb, path jsonpath, vars jsonb DEFAULT '{}', silent boolean
DEFAULT false)`, positionally or in named notation (`vars => …`, `silent => …`, grammar.md §17; in
`FROM` too for the set-returning form, grammar.md §35). They are hand-resolved, like `jsonb_set_lax`,
rather than catalog rows, so the defaults are filled at resolve. All are STRICT: a NULL argument
(including `vars` or `silent`) is SQL NULL, or no rows.

| function | kind | result | semantics |
|---|---|---|---|
| `jsonb_path_exists(target, path [, vars [, silent]])` | scalar | `boolean` | sequence non-empty |
| `jsonb_path_match(target, path [, vars [, silent]])` | scalar | `boolean` | sequence must be a single boolean, or a single JSON `null` → SQL NULL (`22038` otherwise unless silent) |
| `jsonb_path_query(target, path [, vars [, silent]])` | **SRF** | setof `jsonb` | one row per sequence item |
| `jsonb_path_query_array(target, path [, vars [, silent]])` | scalar | `jsonb` | wrap the sequence in a JSON array |
| `jsonb_path_query_first(target, path [, vars [, silent]])` | scalar | `jsonb` | first item, or NULL if empty |

- **`vars`** — a `jsonb` object whose members bind the path's `$name` variables: `$x` reads member
  `x` (names are case-sensitive; `$"a b"` reads `a b`). A variable's value is an item like the
  context item — lax unwraps an array value, `.keyvalue()` numbers it against the `vars` object
  (§3.1). It must be an object: anything else is **`22023`** `"vars" argument is not an object`
  (Detail `Jsonpath parameters should be encoded as key-value pairs of "vars" object.`), raised before
  evaluation, so even when the path uses no variable and in silent mode. A variable with no member is
  **`42704`** `could not find jsonpath variable "x"` when evaluation reaches it; it is not a SQL/JSON
  error, so neither lax mode, silent mode, nor a filter operand (§4.4) suppresses it. The operators
  have no `vars`: any variable is `42704`.
- **`silent`** — `true` evaluates in silent mode (§4.2): the evaluation ends at the first SQL/JSON
  error, `jsonb_path_exists` returns SQL NULL, `jsonb_path_match` decides on the items found before
  it (anything but one boolean or JSON `null` is SQL NULL, not `22038`), and the query functions
  return those items. `false` (the default) raises the error. The operators are the silent forms.

Variables are read by name: a `vars` member by a binary search of the object's canonical key order,
a `PASSING` binding (json-sql-functions.md §5.4) by a binary search of the bindings sorted by name
once per evaluation. Both are flat work (§7).

**Existence tests stop early.** `jsonb_path_exists`, `@?`, `JSON_EXISTS`, and a `JSON_TABLE`
`EXISTS` column only ask whether the sequence is non-empty. In lax mode the evaluation stops at the
first item, so an error a later item would raise is never reached
(`jsonb_path_exists('[1, "a"]', '$[*].double()')` is true, while `'["a", 1]'` raises `22036`). A
strict path is evaluated in full, as PostgreSQL requires the absence of errors.

**A `json` context is converted to `jsonb`.** The SQL/JSON query functions and `JSON_TABLE` also
accept a `json` context; PostgreSQL casts it to `jsonb` first, so keys are in canonical order and a
duplicate key keeps its last value (`JSON_VALUE('{"a":1,"a":2}'::json, '$.a')` is `2`).

### 5.1 `_tz` variants

`jsonb_path_exists_tz` / `_match_tz` / `_query_tz` / `_query_array_tz` / `_query_first_tz` take the
same arguments. In PostgreSQL they differ from the plain functions only in allowing datetime
comparisons and conversions that depend on the session time zone (the plain ones raise `0A000`
`cannot convert value from … without time zone usage` instead), which makes them **STABLE** where the
plain ones are **IMMUTABLE**. jed has no datetime items (the datetime methods are not supported,
§3.1), so no path can reach one and the `_tz` functions behave exactly like the plain ones. They keep
PostgreSQL's volatility: an expression using one is nonimmutable, so it is `42P17` in an index
expression or partial-index predicate ([index-dependencies.md](index-dependencies.md)), and they
stay accepted so PostgreSQL queries that name them still run. The time zone difference would matter
only if the §3.1 decision were revisited.

### 5.2 The `jsonb_path_query` SRF fit

`jsonb_path_query` and `jsonb_path_query_tz` slot into jed's existing set-returning-function
machinery as one generator, `SrfKind::JsonbPathQuery`:

- The resolver places and type-checks the arguments as §5 does (named notation is accepted in
  `FROM`, grammar.md §35), builds a synthetic one-column table typed `jsonb` named after the
  function (`jsonb_path_query` / `jsonb_path_query_tz`), and keeps the four argument expressions.
- The row generator checks `vars` (§5), runs the path over the context item (silently when `silent`
  is true), and yields one row per sequence item, charging one `generated_row` each (so a runaway
  `[*]` fan-out stays cost-proportional and a `max_cost` ceiling aborts). A NULL argument yields no
  rows.
- **SRFs are already implicitly lateral** in jed, so a correlated
  `jsonb_path_query(t.doc, '$.a[*]')` over a table column works with no LATERAL slice. A select-list
  SRF is not yet supported (functions.md §10).
- The catalog keeps a `[[set_returning]]` row for `jsonb_path_query` (its result column); the
  argument list is hand-resolved.

---

## 6. Operators `@?` and `@@`

| operator | meaning | result |
|---|---|---|
| `jsonb @? jsonpath` | silent `jsonb_path_exists` | `boolean` |
| `jsonb @@ jsonpath` | silent `jsonb_path_match` | `boolean` |

New `[[operator]]` rows (kind `"json_path"`). The lexer's `@` arm (today only `@>`) gains
`@?` and `@@` — the exact precedent set by `@>`/`<@`. The parser binds them at the
containment-operator precedence level (the `@>` level); the resolver routes a `jsonb @?
jsonpath` to the path-exists kernel — hand-written dispatch, like the `@>` containment
dispatch. PostgreSQL's operators are the functions' `silent = true` forms with no `vars`: when the
path does not produce exactly one boolean item, `jsonb_path_match` raises `22038` but `@@` returns
SQL NULL, and a SQL/JSON error during evaluation makes `@?` SQL NULL (§4.2).

---

## 7. Cost units

> **Status: landed** in all three cores (`resource/jsonpath_cost.test`). Before it, path
> evaluation charged only one `operator_eval` per final result item. A filter that compares
> every element against a `$`-rooted operand does work quadratic in the document for linear
> cost, and the Rust evaluator copied the whole document for every such operand.

Two units in `spec/cost/schedule.toml`, both weight 1. The cost of `(path, document)` is
deterministic and identical across cores (CLAUDE.md §13). It is defined over the §3 sequence
semantics, not over any core's representation.

**`jsonpath_compile`: one unit per UTF-8 byte of the text being compiled,** charged and guarded
before each compile. For a `jsonpath` value that is its stored canonical text (`'$.a'` is held
as `$."a"`, 5 bytes); for a `JSON_TABLE` path it is the path literal as written. Every evaluation compiles its path, and the evaluators keep no cache. So a
long stored path joined against many rows pays for every pair, the way `varlen_compare` does.
A `'…'::jsonpath` literal is compiled at resolve, which is unmetered (cost.md §3); evaluating
it compiles it again and charges.

**`jsonpath_step`: path-evaluation work,** each charge guarded as it accrues:

1. **Applying a step.** Applying a step to an item charges 1. Under lax unwrapping (§4.1), an
   accessor, filter, or item method applied to an array is applied to each element instead, and
   each element charges 1; the array itself does not. A lax subscript on a non-array (the
   singleton rule) is one application. An item method's own work (a conversion, a rounding, a
   render) is a flat single pass over the item and charges nothing more (cost.md §3, "linear
   single-pass work stays flat"), and so is the layout arithmetic behind a `.keyvalue()` id, done
   at most once per evaluation over the context item.
2. **Emitting items.** Each item a step emits charges 1, so `[*]` over N elements charges N, an
   item method's result charges 1, and `.keyvalue()` charges 1 per pair object.
3. **Member lookup.** `.key` charges 1 for each member it examines: members in stored order
   up to and including the first match, or all of them when the key is absent. Stored order is
   canonical (a `json` context is converted to `jsonb`, §5), so the count is the same in every
   core; a `.keyvalue()` object's members are `id`, `key`, `value` in that order. `.*` emits every
   member and pays through rule 2.
4. **Comparisons.** Each `(a, b)` pair an existential comparison (§4.4) tests charges 1, plus
   the extra a SQL comparison of the same values would charge: two strings
   `varlen_compare × (W − 1)`, with W the shorter length in code points, and two numbers
   `decimal_work × (work_linear(a, b) − 1)` ([cost.md](cost.md) §3). Pairs are tested
   left operand outer, right operand inner. Lax stops at the first true pair and strict at the
   first unknown pair, so untested pairs charge nothing. The left operand is evaluated first and
   the right one only if the left succeeds (an error in the left makes the comparison unknown
   without evaluating the right, as in PostgreSQL); each charges its own steps. A literal operand
   charges nothing, and neither does the lax unwrapping of an array-valued operand item (§4.1): it
   only changes which pairs exist.
5. **Connectives and roots.** `&&`, `||`, `!`, seeding `$`, `@`, `last`, or a variable (§5: its
   lookup is a binary search, flat work), and producing a top-level or parenthesized predicate's
   single item charge nothing. A `PASSING` value costs what its expression costs to evaluate. Both operands of `&&` and
   `||` are always evaluated; there is no short-circuit.
6. **Arithmetic (§3.2).** A binary operator charges its operands' own steps (a literal operand
   nothing, the lax unwrapping of an array operand nothing), then 1 for the application, the SQL
   operator's size-scaled `decimal_work × (W − 1)` ([cost.md](cost.md) §3: `work_linear` for `+ -`,
   `work_mul`, `work_div`, `work_mod`; a zero divisor charges none), and 1 for the emitted result.
   The decimal work is charged and guarded before the operation runs. A unary operator charges its
   operand's steps, then 1 per operand item it is applied to (a skipped non-number too) and 1 per
   number it emits. A subscript expression charges its own steps each time it is evaluated; a
   literal or bare `last` index charges nothing.

7. **Predicate forms (§4.4).** `starts with` charges its operands' steps like a comparison, then
   1 per pair it tests plus `varlen_compare × (W − 1)` when both items are strings, W the shorter
   length in code points; lax stops at the first true pair and strict at the first unknown one, as
   in rule 4. `like_regex` charges its operand's steps, then 1 per item it tests plus the match's
   own `regex_step` units ([regex.md](regex.md) §6), with the same stops. `exists` and `is unknown`
   charge nothing beyond their operand's steps; a lax `exists` stops at its operand's first item.
8. **`.**` (§3.3).** Applying it charges 1 (rule 1); each descendant it visits — every descendant
   down to level `last`, whether or not it is emitted — charges 1, and each item it emits charges 1
   more (rule 2).

**Compiling a `like_regex` pattern** charges `regex_compile` per instruction of its program
([regex.md](regex.md) §6), each time the path is compiled for an evaluation — after the path's own
`jsonpath_compile` charge, before evaluation.

A cost-ceiling, lifetime-budget, or cancellation error raised inside a filter operand
propagates. Only SQL/JSON navigation errors make a comparison unknown (§4.2).

An existence test in lax mode stops at the first item (§5), so the steps it never runs charge
nothing.

Results are not charged here. The query functions copy their result items out of the
document: `jsonb_path_query` rows charge `generated_row` (§5.2), `jsonb_path_query_array` and a
wrapped `JSON_QUERY` charge their built array as growth kernels (cost.md §8.1), and the other
results are single sub-values of the document or small method results. A `.keyvalue()` object
refers to its member value instead of copying it until it is returned. Existence tests and
matches copy nothing.

---

## 8. Error codes

Register the SQL/JSON class-22 subcodes in [../errors/registry.toml](../errors/registry.toml)
(by **code**; names per PG). These are shared with the SQL/JSON standard functions
([json-sql-functions.md](json-sql-functions.md)):

| code | name | use |
|---|---|---|
| `22039` | `sql_json_array_not_found` | strict `[…]` / `[*]` / `.size()` on a non-array |
| `2203A` | `sql_json_member_not_found` | strict missing member, or a member accessor on a non-object |
| `2203B` | `sql_json_number_not_found` | a unary `+`/`-` operand item that is not a number (§3.2) |
| `2203C` | `sql_json_object_not_found` | `.keyvalue()` (both modes) / strict `.*` on a non-object |
| `2203F` | `sql_json_scalar_required` | `JSON_VALUE` of an array or object |
| `22030` | `duplicate_json_object_key_value` | `WITH UNIQUE KEYS` / `*_unique` agg |
| `22031` | `invalid_argument_for_sql_json_datetime_function` | `.datetime()` bad arg |
| `22032` | `invalid_json_text` | malformed JSON (path-surface parse) |
| `22033` | `invalid_sql_json_subscript` | a subscript out of bounds (strict), out of the integer range, or not a single number |
| `22034` | `more_than_one_sql_json_item` | singleton required, >1 item |
| `22035` | `no_sql_json_item` | `JSON_VALUE`/`JSON_QUERY` empty, no `ON EMPTY` default |
| `22036` | `non_numeric_sql_json_item` | every item-method coercion failure (§3.1) |
| `22037` | `non_unique_keys_in_a_json_object` | object construction unique-keys |
| `22038` | `singleton_sql_json_item_required` | `JSON_VALUE` requires a scalar; `jsonb_path_match` requires one boolean; a binary arithmetic operand is not a single number (§3.2) |

A path variable with no binding is **`42704`** (`undefined_object`, `could not find jsonpath variable
"x"`) and a `jsonb_path_*` `vars` argument that is not an object **`22023`**; neither is a SQL/JSON
error, so silent mode and `ON ERROR` do not suppress them (§5).

Reuse existing codes where PG does: **`42601`** for a malformed path *literal* (syntax-error
class, at resolve) and an unrecognized `like_regex` flag character, **`22023`** for a `.decimal()`
precision or scale outside PostgreSQL's `numeric` typmod ranges, **`2201B`** for a malformed
`like_regex` pattern (`54001` for an oversized one), **`0A000`** for the unsupported `m`/`x` regex
flags (§4.3), **`22003`** for a `.**` level above 2147483647 (§2), and **`22P02`** for malformed JSON in the
`json_in`/`jsonb_in` document-input path ([json.md §6.3](json.md)). A string escape that decodes
to NUL is **`22P05`**, an unpaired surrogate escape **`22P02`** (§2.1), and a path with no tokens
**`22P02`** (§2.4). Arithmetic raises the
`decimal` errors: **`22012`** for a zero divisor, **`22003`** past the format cap (§3.2).

**Nesting limit — `54001`.** The compiler counts program nesting (+1 per filter `?(`,
parenthesized group, `exists (`, `!`, `&&`/`||` chain step, binary arithmetic chain step, unary
operator, and subscript list) and rejects a path past `MAX_JSON_DEPTH = 256` with **`54001`**
(`statement_too_complex`), the same gate as SQL expressions ([cost.md §7](cost.md)), so the compile,
render, and evaluation walks are stack-safe. A path is evaluated from its canonical text (§2.3),
whose parentheses can nest deeper than the source (`$ + 1 + 1` renders `(($ + 1) + 1)`, a unary
operator `(-…)`), so `JsonPath::compile` checks that text against the limit too: 255 chained binary
operators fit and 256 do not, and 128 nested unary operators fit. A literal that compiles can
therefore always be evaluated. PostgreSQL raises `42601` ("memory exhausted") only after about 10 000 nested
parentheses, when its generated parser runs out of stack. jed's fixed limit is a documented
divergence ([json.md §6.4](json.md)).

---

## 9. Delivery — vertical slices

After the `jsonb` foundation ([json.md §12](json.md), J0–J2):

- **P1** — the `jsonpath` type (type_code 20) + `JsonPath::compile` program + the
  `jsonpath_compile`/`jsonpath_step` cost units + the lax/strict evaluation engine (§3–§4) +
  `like_regex` → Pike VM (§4.3) + the `2203x` error registration (§8). **Highest novelty
  after `JSON_TABLE`** — a whole sub-language and evaluator. Capability `types.jsonpath`.
- **P2** — the path query functions + `@?`/`@@` + the `jsonb_path_query` SRF + `vars`/
  `silent` (§5–§6). Depends on P1. Capability `func.jsonb_path`; `vars`/`silent` with the variables
  themselves are `expr.jsonpath_vars` (below).
  - **P1b/P2-filters** ✅ — filter expressions `?(predicate)` (§4) + the `@?` exists operator
    (§6) have landed. The predicate subset is the comparison core: `==` `!=`/`<>` `<` `<=`
    `>` `>=` over `@`/`$`-rooted accessor paths and scalar literals, combined with `&&` /
    `||` / `!(…)` and parens, with existential comparison + 3-valued (Kleene) connectives (an
    item is kept only on a definite TRUE; filter operands never raise). `jsonb @? jsonpath`
    binds at the `@>` precedence and routes to `jsonb_path_exists`. Capability
    `expr.jsonpath_filter`.
  - **P1b/P2-match** ✅ — TOP-LEVEL predicates + `jsonb_path_match` + the `@@` operator (§6)
    have landed. A jsonpath body may be a top-level boolean predicate (`$.a == 1`,
    `$.a > 1 && $.b < 2`) — the same predicate grammar as a filter, rooted at the document.
    `jsonb_path_match(ctx, path)` requires the path to produce **exactly one boolean** item (else
    `22038`); `ctx @@ path` is its PostgreSQL-compatible silent form and returns SQL NULL for that
    error. A top-level predicate produces one item — `true`, `false`, or JSON `null` when unknown
    (§4.4), which `jsonb_path_match` and `@@` return as SQL NULL. A top-level predicate, queried, yields the boolean as a single item;
    it renders parenthesized (`($."a" == 1)`), which round-trips through compile. Capability
    `expr.jsonpath_match`.
  - **Item methods** ✅ — every PostgreSQL 18 item method except the datetime family (not
    supported by decision, §3.1),
    after `$`, `@`, or a literal; the depth-first evaluator (§3) with PostgreSQL's early stop for
    existence tests (§5) and silent `@?`/`@@` (§4.2); `.keyvalue()` ids from PostgreSQL's binary
    layout; a `json` context converted to `jsonb` (§5); PostgreSQL's strict subscript and
    non-array codes (§4.2). Capability `expr.jsonpath_methods`.
  - **Arithmetic** ✅ — binary `+ - * / %` and unary `+ -` (§3.2) with PostgreSQL's precedence
    and parenthesized expressions followed by accessors (§2), in comparisons, filters, top-level
    predicates, and subscript expressions (`$[last - 1]`); PostgreSQL's operator parenthesization
    in the render (§2.3), with `&&`/`||` grouping now kept. Capability `expr.jsonpath_arith`.
  - **Variables** ✅ — `$name` / `$"name"` (§2.2), the `jsonb_path_*` `vars` / `silent` arguments
    positionally or in named notation with PostgreSQL's defaults (§5), the `_tz` functions as STABLE
    twins (§5.1), and the SQL/JSON `PASSING` clause of `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` /
    `JSON_TABLE` (json-sql-functions.md §5.4); `.keyvalue()` ids count the variable base objects
    (§3.1). Capability `expr.jsonpath_vars`.
    **Still deferred (`0A000`):** a float `PASSING` value.
    **Not supported, by decision (`0A000`, §3.1):** the datetime methods, and with them the `_tz`
    functions' time zone use and `PASSING` a date/time value (a datetime item).
  - **Predicates** ✅ — `exists (…)`, `like_regex` with its flags (§4.3, onto the Pike VM's
    newline-sensitive / dot-all / quote modes, regex.md §8d), `starts with`, and `(…) is unknown`
    (§4.4 rules 5–7), in filters and as top-level predicates. Capability `expr.jsonpath_predicates`.
    **Unsupported (`0A000`, §4.3):** the `m` flag (and `x`, as in PostgreSQL).
  - **Any-level accessor** ✅ — `.**` with `{n}` / `{n to m}` / `last` bounds (§3.3), suppressing
    navigation errors below it in either mode. Capability `expr.jsonpath_any`.
- **P3** — the `_tz` variants (§5.1): ✅ as STABLE twins. They have no datetime comparisons to
  route through the clock/tz seam, since jed has no datetime items (§3.1).

**Riskiest, least-precedented:** the lax-mode auto-unwrapping + the navigation-vs-coercion
error distinction (§4.2 rule 6) — the cross-core determinism contract makes any subtle
divergence a hard failure, so it is oracle-pinned against PG with a dedicated conformance
suite. The `jsonpath` surface is consumed by the SQL/JSON standard functions and
`JSON_TABLE` — see [json-sql-functions.md](json-sql-functions.md) and
[json-table.md](json-table.md).
