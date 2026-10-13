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

> **Status: landed in part.** The type, the structural accessors, filters, top-level predicates,
> the query functions and operators, and the item methods (except the datetime family) are
> implemented in all three cores; §9 lists what remains deferred (`0A000`).

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
jsonpath     ::= mode? path_expr
mode         ::= "strict" | "lax"                 (* default: lax *)
path_expr    ::= accessor_expr ( arith_op accessor_expr )*   (* arithmetic in filter ctx *)
accessor_expr::= primary accessor*
primary      ::= "$"                              (* root document *)
               | "@"                              (* current item (filter context only) *)
               | "$" varname                      (* named variable: $"x" / $x — bound via vars *)
               | literal                          (* number | string | true | false | null *)
               | "(" path_expr ")"
accessor     ::= "." member                       (* .key  or  ."key with spaces" *)
               | ".*"                             (* wildcard member *)
               | "[" subscript ( "," subscript )* "]"
               | "[" "*" "]"                      (* wildcard element *)
               | "." method "(" ")"               (* item method, §3.1 *)
               | "." "decimal" "(" ( int ( "," int )? )? ")"
               | "." "datetime" "(" string? ")"
               | "." ( "time" | "time_tz" | "timestamp" | "timestamp_tz" ) "(" uint? ")"
               | "?" "(" predicate ")"            (* filter, §4 *)
int          ::= ( "+" | "-" )? uint              (* an integer literal: 0, or no leading zero *)
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
               | path_expr "is" "unknown"
comparison   ::= "==" | "!=" | "<>" | "<" | "<=" | ">" | ">="
method       ::= "type" | "size" | "double" | "ceiling" | "floor" | "abs" | "keyvalue"
               | "bigint" | "boolean" | "integer" | "number" | "string" | "date"
```

Method names are keywords matched case-insensitively (`$.TYPE()` is `.type()`); whitespace may
separate the `.`, the name, and the `(`. Without the parentheses the same word is a member key
(`$.type` is `$."type"`). An unknown method name, arguments where a method takes none, or more
than two `.decimal()` arguments is `42601`.

A literal primary takes accessors too (`"12".number()`, `null.type()`). A number literal followed
by an accessor renders parenthesized (`1.5.type()` → `(1.5).type()`), and `(` *literal* `)` is
accepted back, so the render round-trips; a parenthesized expression or predicate followed by an
accessor is otherwise deferred (`0A000`). Number literals follow PostgreSQL's jsonpath scanner
(`1`, `1.5`, `1.`, `.5`, `1e3`); a number running into a letter or digit (`1.type()`, `01`, `1e`)
is trailing junk (`42601`). `-1.5.abs()` is unary minus applied to the path — arithmetic, deferred.
Like jed's SQL literals (grammar.md §36), jsonpath numbers take no `_` digit separators and no
hex/octal/binary prefixes, which PostgreSQL 16+ accepts.

Faithful inclusions: `last` (array-length-minus-1 sentinel inside a subscript), `to` slices,
named variables `$name` (bound by the `vars` argument, §5), `.datetime(template?)`,
`like_regex` with an optional `flag` clause (§4.3). Nothing in PG's path grammar is excluded
as obsolete; the only narrowing is the `like_regex` flag set (§4.3), a consequence of jed's
deliberate regex subset.

### 2.1 String escapes

A quoted string (a `."key"` member or a string literal) takes PostgreSQL's jsonpath scanner escapes
(`jsonpath_scan.l`), which are wider than JSON's:

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

An *unquoted* member name (`$.key`) still takes only ASCII letters, digits, `_`, and `$`, and no
escapes. PostgreSQL's scanner admits any character outside its special set there (`$.é`, `$.a~b`)
and the same backslash escapes (`$.a\u0041`); that wider lexing is a follow-on.

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
PostgreSQL's binary jsonb layout. The base is the context item (number 0) while a `$` path runs,
and the `.keyvalue()` object whose continuation is running for anything reached through one.
`.keyvalue()` objects are numbered in evaluation order (depth-first, §3), starting one past the
variable base objects the caller supplies: 2 for the `jsonb_path_*` functions (their default
`vars` object counts as one), 1 for `@?`/`@@` and the SQL/JSON functions. The layout is a pure
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

**Deferred (`0A000`).** The datetime methods `.datetime([template])`, `.date()`, `.time([p])`,
`.time_tz([p])`, `.timestamp([p])`, and `.timestamp_tz([p])`. Even without a template PostgreSQL
runs its `to_timestamp` engine over a fixed list of ISO templates, with that engine's field-reading
rules (`"20-01-05"` is year 20, `"10:00:00.+05"` reads microseconds) and its time zone abbreviation
table (`"10:00:00PST"`), which jed has not decided to load (timezones.md §15); the methods also
produce date/time items that PostgreSQL compares across types. They need that engine first.

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

**Silent mode** is **orthogonal**: it suppresses the SQL/JSON (class `22`) errors that remain,
ending the evaluation at the first one. `@?` then returns SQL NULL; `@@` decides on the items found
before the error (`'[true, "x"]'::jsonb @@ '$[*].boolean()'` is true). Filter operands are always
evaluated silently (§4.4 rule 1). A cost, budget, or cancellation error is never suppressed. The
`silent` argument of the query functions (§5) is still deferred.

### 4.3 `like_regex` onto the Pike VM

`like_regex` maps to jed's regex VM: `regex::compile(pattern)` + `Program::is_match` (boolean
result, captures ignored — exactly how the `~` operator already uses it). The XQuery `flag`
string is constrained by jed's flagless VM ([regex.md](regex.md)):

- **`i`** (case-insensitive) — supported via the existing `~*`/ILIKE simple-lowercase path
  (lowercase the input item and compile a lowercased pattern).
- **`q`** (quote / literal) — supported as a compile mode that treats the whole pattern as a
  literal string (no metacharacters).
- **`s`** (dotall), **`m`** (multiline), **`x`** (extended whitespace) — **unsupported**:
  jed's VM is single-line, `.` excludes `\n`, `^`/`$` are whole-subject anchors, and there is
  no extended mode. A path using `s`/`m`/`x` raises **`0A000`** (feature not supported), a
  documented divergence driven by jed's deliberate ReDoS-immune regex subset — **not**
  obsolescence (it owns its regex surface; the `~` operator is already explicitly a jed
  subset).

A malformed regex pattern is the existing **`2201B`**. `starts with` is a plain prefix test
on string items (no regex). `is unknown` tests whether a predicate evaluated to the third
truth value.

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
5. **Results.** A filter keeps an item only when its predicate is true. A top-level predicate
   yields one item: `true`, `false`, or JSON `null` for unknown; `jsonb_path_match` and `@@` turn
   that `null` into SQL NULL.

---

## 5. Path query functions

The catalog plan ([../functions/catalog.toml](../functions/catalog.toml)): five functions
(one set-returning) + their `_tz` variants, plus two operators (§6). All take a `jsonb`
context, a `jsonpath`, and optional `vars jsonb` + `silent boolean` trailing arguments
(the existing `arg_defaults` / named-notation facility; `silent` defaults `false`).

| function | kind | result | semantics |
|---|---|---|---|
| `jsonb_path_exists(jsonb, jsonpath [, vars, silent])` | scalar | `boolean` | sequence non-empty |
| `jsonb_path_match(jsonb, jsonpath [, vars, silent])` | scalar | `boolean` | sequence must be a single boolean, or a single JSON `null` → SQL NULL (`22038` otherwise unless silent) |
| `jsonb_path_query(jsonb, jsonpath [, vars, silent])` | **SRF** | setof `jsonb` | one row per sequence item |
| `jsonb_path_query_array(jsonb, jsonpath [, vars, silent])` | scalar | `jsonb` | wrap the sequence in a JSON array |
| `jsonb_path_query_first(jsonb, jsonpath [, vars, silent])` | scalar | `jsonb` | first item, or NULL if empty |

- **`vars`** — a `jsonb` object whose members bind the path's `$name` variables (substituted
  as literal items during evaluation). Deferred; the default `{}` object still counts as one base
  object for `.keyvalue()` numbering (§3.1).
- **`silent`** — suppress the errors that strict mode / singleton checks would raise,
  returning NULL/false/empty (§4.2). Deferred; the `@?`/`@@` operators are the silent forms.

**Existence tests stop early.** `jsonb_path_exists`, `@?`, `JSON_EXISTS`, and a `JSON_TABLE`
`EXISTS` column only ask whether the sequence is non-empty. In lax mode the evaluation stops at the
first item, so an error a later item would raise is never reached
(`jsonb_path_exists('[1, "a"]', '$[*].double()')` is true, while `'["a", 1]'` raises `22036`). A
strict path is evaluated in full, as PostgreSQL requires the absence of errors.

**A `json` context is converted to `jsonb`.** The SQL/JSON query functions and `JSON_TABLE` also
accept a `json` context; PostgreSQL casts it to `jsonb` first, so keys are in canonical order and a
duplicate key keeps its last value (`JSON_VALUE('{"a":1,"a":2}'::json, '$.a')` is `2`).

### 5.1 `_tz` variants

`jsonb_path_exists_tz` / `_match_tz` / `_query_tz` / `_query_array_tz` / `_query_first_tz`
behave identically except that `.datetime()` comparisons and zone-aware coercions resolve
through the host clock/tz seam ([entropy.md](entropy.md), the `now()`/`clock_timestamp()`
seam). The non-`_tz` functions are **`immutable`**; the `_tz` ones are **`stable`** (the
catalog `volatility` field), so they stay deterministic-given-the-seam.

### 5.2 The `jsonb_path_query` SRF fit

`jsonb_path_query` slots into jed's existing set-returning-function machinery with one
extension. Today `SrfKind` is `{GenerateSeries, Unnest}`, resolved into a synthetic
single-column `Table` and driven by per-kind row generators (the executor's `resolve_srf` /
`srf_table` path). Plan:

- Add `SrfKind::JsonbPathQuery` (and `JsonbPathQueryTz`).
- The resolver type-checks the args (`jsonb`, `jsonpath`, optional `vars jsonb` / `silent
  boolean`), builds a synthetic one-column table typed `jsonb` (column name
  `jsonb_path_query`), and stores the compiled path + arg expressions in the plan.
- A new row generator runs the path over the context item and yields one row per sequence
  item, charging one `generated_row` each (so a runaway `[*]` fan-out stays cost-proportional
  and a `max_cost` ceiling aborts).
- **SRFs are already implicitly lateral** in jed, so a correlated
  `jsonb_path_query(t.doc, '$.a[*]')` over a table column works with no LATERAL slice. (The
  generic `WITH ORDINALITY` form stays deferred — it is not needed here.)
- Add a `[[set_returning]]` catalog entry mirroring `unnest`, with
  `arg_families = ["jsonb", "jsonpath", "jsonb", "boolean"]`, `result = "set_of_jsonb"`,
  `null = "empty_on_null"`.

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
5. **Connectives and roots.** `&&`, `||`, `!`, seeding `$` or `@`, and producing a top-level
   predicate's single item charge nothing. Both operands of `&&` and `||` are always evaluated;
   there is no short-circuit.

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
| `2203C` | `sql_json_object_not_found` | `.keyvalue()` (both modes) / strict `.*` on a non-object |
| `2203F` | `sql_json_scalar_required` | `JSON_VALUE` of an array or object |
| `22030` | `duplicate_json_object_key_value` | `WITH UNIQUE KEYS` / `*_unique` agg |
| `22031` | `invalid_argument_for_sql_json_datetime_function` | `.datetime()` bad arg |
| `22032` | `invalid_json_text` | malformed JSON (path-surface parse) |
| `22033` | `invalid_sql_json_subscript` | bad array subscript (strict) |
| `22034` | `more_than_one_sql_json_item` | singleton required, >1 item |
| `22035` | `no_sql_json_item` | `JSON_VALUE`/`JSON_QUERY` empty, no `ON EMPTY` default |
| `22036` | `non_numeric_sql_json_item` | every item-method coercion failure (§3.1) |
| `22037` | `non_unique_keys_in_a_json_object` | object construction unique-keys |
| `22038` | `singleton_sql_json_item_required` | `JSON_VALUE` requires a scalar; `jsonb_path_match` requires one boolean |

Reuse existing codes where PG does: **`42601`** for a malformed path *literal* (syntax-error
class, at resolve), **`22023`** for a `.decimal()` precision or scale outside PostgreSQL's
`numeric` typmod ranges, **`2201B`** for a malformed `like_regex` pattern, **`0A000`** for the
unsupported `s`/`m`/`x` regex flags (§4.3), and **`22P02`** for malformed JSON in the
`json_in`/`jsonb_in` document-input path ([json.md §6.3](json.md)). A string escape that decodes
to NUL is **`22P05`**, an unpaired surrogate escape **`22P02`** (§2.1).

**Nesting limit — `54001`.** The compiler counts program nesting (+1 per filter `?(`,
parenthesized predicate group, `!`, and `&&`/`||` chain step) and rejects a path past
`MAX_JSON_DEPTH = 256` with **`54001`** (`statement_too_complex`), the same gate as SQL
expressions ([cost.md §7](cost.md)), so the compile, render, and evaluation walks are
stack-safe. PostgreSQL raises `42601` ("memory exhausted") only after about 10 000 nested
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
  `silent` (§5–§6). Depends on P1. Capability `func.jsonb_path`.
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
  - **Item methods** ✅ — every PostgreSQL 18 item method except the datetime family (§3.1),
    after `$`, `@`, or a literal; the depth-first evaluator (§3) with PostgreSQL's early stop for
    existence tests (§5) and silent `@?`/`@@` (§4.2); `.keyvalue()` ids from PostgreSQL's binary
    layout; a `json` context converted to `jsonb` (§5); PostgreSQL's strict subscript and
    non-array codes (§4.2). Capability `expr.jsonpath_methods`. **Still deferred (`0A000`):** the
    datetime methods (they need PostgreSQL's `to_timestamp` engine, §3.1), arithmetic, the §4.3
    Pike-VM `like_regex` / `starts with` / `is unknown` / `exists` predicates, `$name` variables
    and the `vars`/`silent` arguments, `.**`, and the `_tz` functions — the remaining open P2/P3
    follow-ons.
- **P3** — the `_tz` variants (§5.1): route `.datetime()` through the clock/tz seam,
  `stable` volatility. Small follow-on to P2.

**Riskiest, least-precedented:** the lax-mode auto-unwrapping + the navigation-vs-coercion
error distinction (§4.2 rule 6) — the cross-core determinism contract makes any subtle
divergence a hard failure, so it is oracle-pinned against PG with a dedicated conformance
suite. The `jsonpath` surface is consumed by the SQL/JSON standard functions and
`JSON_TABLE` — see [json-sql-functions.md](json-sql-functions.md) and
[json-table.md](json-table.md).
