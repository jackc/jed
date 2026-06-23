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

> **Status: SPEC-FIRST (design ratified, implementation pending).** Implemented by the
> P-series slices (§9), after the `jsonb` type foundation ([json.md §12](json.md), J0–J2).

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
               | "." method "(" arg? ")"          (* item method, §3 *)
               | "?" "(" predicate ")"            (* filter, §4 *)
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
method       ::= "type" | "size" | "double" | "ceiling" | "floor" | "abs"
               | "keyvalue" | "datetime" | "bigint" | "number" | "boolean" | "string"
```

Faithful inclusions: `last` (array-length-minus-1 sentinel inside a subscript), `to` slices,
named variables `$name` (bound by the `vars` argument, §5), `.datetime(template?)`,
`like_regex` with an optional `flag` clause (§4.3). Nothing in PG's path grammar is excluded
as obsolete; the only narrowing is the `like_regex` flag set (§4.3), a consequence of jed's
deliberate regex subset.

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
  - `.method()` maps each item through the method (§3.1).
- **Result.** The query functions (§5) interpret the final sequence: exists = non-empty;
  query = one row per item; query_first = first or NULL; match = the sequence must be a
  single boolean.

Determinism is automatic: `jsonb` object members are stored in canonical key order
([json.md §2.3](json.md)) and array order is preserved, so `.*` and `.keyvalue()` emit in a
deterministic order across cores.

### 3.1 Item methods

| method | on | result | error (non-applicable) |
|---|---|---|---|
| `.type()` | any | string (`"null"`/`"boolean"`/`"number"`/`"string"`/`"array"`/`"object"`) | — |
| `.size()` | array (lax: any → 1) | number (element count) | — |
| `.double()` | number / numeric-string | number | `2203A` cast failure / `22036` non-numeric |
| `.ceiling()` / `.floor()` / `.abs()` | number | number | `22036` non-numeric |
| `.bigint()` / `.number()` | number / string | number (`decimal`) | `2203A` / `22036` |
| `.boolean()` | boolean / `"true"`/`"false"` / number | boolean | `2203A` |
| `.string()` | scalar | string | `2203A` |
| `.keyvalue()` | object | sequence of `{"key":k,"value":v,"id":n}` objects | `2203C` on non-object |
| `.datetime(template?)` | string | a date/time `jsonb` scalar (zone via the `_tz` seam, §5.1) | `22031` bad arg |

**Crucial:** item-method *coercion* failures (`.double()` on a non-number, etc.) are **not**
suppressed by lax mode — only structural *navigation* failures are (§4.2). This is the
classic PG-compat trap; both modes raise `2203A`/`22036` on a bad method coercion.

---

## 4. `lax` (default) vs `strict`

Lax differs from strict in exactly two ways: **automatic array unwrapping** and
**structural-error suppression**. Stated operationally:

### 4.1 Automatic unwrapping (lax only)

1. **Member accessor on an array.** In lax, `.key` / `.*` applied to an array first **unwraps
   it one level** — the accessor is applied to each element and the results concatenated.
   (`lax $.a` over `[{"a":1},{"a":2}]` → `1, 2`.) Strict: a member accessor on an array is a
   structural error.
2. **Element accessor on a non-array.** In lax, `[i]` / `[*]` on a non-array treats the item
   as a **singleton array** `[item]` first (`lax $[0]` over a scalar → the scalar). Strict: a
   structural error.
3. **`.size()` on a non-array** is `1` in lax (the implicit singleton); strict requires an
   array.
4. Unwrapping is **one level**, applied *before* each accessor step (PG does not transitively
   flatten nested arrays in one shot).

### 4.2 Structural-error suppression (lax only)

5. A **navigation** failure — a missing object member, an out-of-range subscript, a type
   mismatch on a structural step — contributes **no item** in lax (the sequence is just
   shorter), where strict raises `2203F` (member not found), `22033` (subscript), etc.
6. **Item-method coercion failures are NOT suppressed** even in lax (§3.1). Lax suppresses
   *navigation* failures, not *coercion* failures.

The `silent` argument of the query functions (§5) is **orthogonal**: when true it suppresses
the remaining errors that even strict mode (and the singleton checks of `_match`/`_query_first`)
would otherwise raise, returning NULL/false/empty instead.

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

---

## 5. Path query functions

The catalog plan ([../functions/catalog.toml](../functions/catalog.toml)): five functions
(one set-returning) + their `_tz` variants, plus two operators (§6). All take a `jsonb`
context, a `jsonpath`, and optional `vars jsonb` + `silent boolean` trailing arguments
(the existing `arg_defaults` / named-notation facility; `silent` defaults `false`).

| function | kind | result | semantics |
|---|---|---|---|
| `jsonb_path_exists(jsonb, jsonpath [, vars, silent])` | scalar | `boolean` | sequence non-empty |
| `jsonb_path_match(jsonb, jsonpath [, vars, silent])` | scalar | `boolean` | sequence must be a single boolean (`22034` otherwise unless silent) |
| `jsonb_path_query(jsonb, jsonpath [, vars, silent])` | **SRF** | setof `jsonb` | one row per sequence item |
| `jsonb_path_query_array(jsonb, jsonpath [, vars, silent])` | scalar | `jsonb` | wrap the sequence in a JSON array |
| `jsonb_path_query_first(jsonb, jsonpath [, vars, silent])` | scalar | `jsonb` | first item, or NULL if empty |

- **`vars`** — a `jsonb` object whose members bind the path's `$name` variables (substituted
  as literal items during evaluation).
- **`silent`** — suppress the errors that strict mode / singleton checks would raise,
  returning NULL/false/empty (§4.2).

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
| `jsonb @? jsonpath` | `jsonb_path_exists` | `boolean` |
| `jsonb @@ jsonpath` | `jsonb_path_match` | `boolean` |

New `[[operator]]` rows (kind `"json_path"`). The lexer's `@` arm (today only `@>`) gains
`@?` and `@@` — the exact precedent set by `@>`/`<@`. The parser binds them at the
containment-operator precedence level (the `@>` level); the resolver routes a `jsonb @?
jsonpath` to the path-exists kernel — hand-written dispatch, like the `@>` containment
dispatch.

---

## 7. Cost units

Two new cost units ([../cost/](../cost/) / `gen_costs.rb`), the regex precedent:

- **`jsonpath_compile`** — one unit per emitted program step, charged once at compile (the
  `regex_compile` model; a stored `jsonpath` recompiled on load charges it on first use).
- **`jsonpath_step`** — one unit per evaluated step per input item (the `regex_step` model),
  so the metered cost is proportional to the path-evaluation work and a `max_cost` ceiling
  aborts a pathological fan-out deterministically.

The cost of `(path, document)` is fully deterministic and identical across cores (CLAUDE.md
§13).

---

## 8. Error codes

Register the SQL/JSON class-22 subcodes in [../errors/registry.toml](../errors/registry.toml)
(by **code**; names per PG). These are shared with the SQL/JSON standard functions
([json-sql-functions.md](json-sql-functions.md)):

| code | name | use |
|---|---|---|
| `2203A` | `sql_json_item_cannot_be_cast_to_target_type` | `.double()` / cast failure |
| `2203C` | `sql_json_object_not_found` | object expected (`.keyvalue()` / strict member) |
| `2203F` | `sql_json_member_not_found` | strict-mode missing member |
| `22030` | `duplicate_json_object_key_value` | `WITH UNIQUE KEYS` / `*_unique` agg |
| `22031` | `invalid_argument_for_sql_json_datetime_function` | `.datetime()` bad arg |
| `22032` | `invalid_json_text` | malformed JSON (path-surface parse) |
| `22033` | `invalid_sql_json_subscript` | bad array subscript (strict) |
| `22034` | `more_than_one_sql_json_item` | singleton required, >1 item |
| `22035` | `no_sql_json_item` | `JSON_VALUE`/`JSON_QUERY` empty, no `ON EMPTY` default |
| `22036` | `non_numeric_sql_json_item` | arithmetic / numeric method on a non-number |
| `22037` | `non_unique_keys_in_a_json_object` | object construction unique-keys |
| `22038` | `singleton_sql_json_item_required` | `JSON_VALUE` requires a scalar |

Reuse existing codes where PG does: **`42601`** for a malformed path *literal* (syntax-error
class, at resolve), **`2201B`** for a malformed `like_regex` pattern, **`0A000`** for the
unsupported `s`/`m`/`x` regex flags (§4.3), and **`22P02`** for malformed JSON in the
`json_in`/`jsonb_in` document-input path ([json.md §6.3](json.md)).

---

## 9. Delivery — vertical slices

After the `jsonb` foundation ([json.md §12](json.md), J0–J2):

- **P1** — the `jsonpath` type (type_code 20) + `JsonPath::compile` program + the
  `jsonpath_compile`/`jsonpath_step` cost units + the lax/strict evaluation engine (§3–§4) +
  `like_regex` → Pike VM (§4.3) + the `2203x` error registration (§8). **Highest novelty
  after `JSON_TABLE`** — a whole sub-language and evaluator. Capability `types.jsonpath`.
- **P2** — the path query functions + `@?`/`@@` + the `jsonb_path_query` SRF + `vars`/
  `silent` (§5–§6). Depends on P1. Capability `func.jsonb_path`.
- **P3** — the `_tz` variants (§5.1): route `.datetime()` through the clock/tz seam,
  `stable` volatility. Small follow-on to P2.

**Riskiest, least-precedented:** the lax-mode auto-unwrapping + the navigation-vs-coercion
error distinction (§4.2 rule 6) — the cross-core determinism contract makes any subtle
divergence a hard failure, so it is oracle-pinned against PG with a dedicated conformance
suite. The `jsonpath` surface is consumed by the SQL/JSON standard functions and
`JSON_TABLE` — see [json-sql-functions.md](json-sql-functions.md) and
[json-table.md](json-table.md).
