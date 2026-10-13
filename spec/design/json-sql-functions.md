# JSON functions, operators & SQL/JSON standard syntax — design

> The full PostgreSQL `json`/`jsonb` function and operator surface (minus obsolete
> back-compat forms), plus the SQL:2016 **SQL/JSON** syntactic constructs
> (`JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`, the `JSON()` / `JSON_SCALAR()` /
> `JSON_SERIALIZE()` constructors, and the `IS JSON` predicate). The path-query functions
> live in [jsonpath.md](jsonpath.md); `JSON_TABLE` and the record-returning functions in
> [json-table.md](json-table.md); the document types in [json.md](json.md). This doc maps
> every function onto the jed mechanism it reuses — scalar `[[operator]]`, `[[set_returning]]`,
> or `[[aggregate]]` catalog entry — and flags the two new pieces of machinery needed (the
> multi-column SRF and the duplicate-key kernel). PostgreSQL semantics are the default
> (CLAUDE.md §1), pinned against the live `postgres:18` oracle. Catalog rows are data in
> [../functions/catalog.toml](../functions/catalog.toml).

> **Status: SPEC-FIRST (design ratified, implementation pending).** Implemented by the
> B-series (builders/processing/SRFs/aggregates) and S-series (SQL/JSON standard syntax)
> slices (§5), after the `jsonb` foundation ([json.md §12](json.md)) and the `jsonpath`
> type ([jsonpath.md §9](jsonpath.md)).

---

## 1. Operators (`jsonb`, the classic surface)

New `[[operator]]` rows; kernels hand-written per core over the node tree. Most have a
`json` overload that parses-to-node then dispatches to the same kernel (the json/jsonb split
recorded as data, not branchy code — except where PG observably differs, §3).

| op | signature | result | slice | notes |
|---|---|---|---|---|
| `->` | `jsonb -> text` / `jsonb -> i32` | `jsonb` | J4 | field by key / element by index; out-of-range/absent → SQL NULL |
| `->>` | `jsonb ->> text` / `jsonb ->> i32` | `text` | J4 | same, rendered as text |
| `#>` | `jsonb #> text[]` | `jsonb` | J4 | get at path |
| `#>>` | `jsonb #>> text[]` | `text` | J4 | get at path as text |
| `@>` | `jsonb @> jsonb` | `boolean` | J5 | left contains right (deep) |
| `<@` | `jsonb <@ jsonb` | `boolean` | J5 | left contained by right |
| `?` | `jsonb ? text` | `boolean` | J5 | top-level key (or array string element) exists |
| `?\|` | `jsonb ?\| text[]` | `boolean` | J5 | any key exists |
| `?&` | `jsonb ?& text[]` | `boolean` | J5 | all keys exist |
| `\|\|` | `jsonb \|\| jsonb` | `jsonb` | J6 | concatenate / shallow-merge (objects merge, arrays append) |
| `-` | `jsonb - text` / `jsonb - i32` / `jsonb - text[]` | `jsonb` | J6 | delete key / element / each key |
| `#-` | `jsonb #- text[]` | `jsonb` | J6 | delete at path |
| `@?` | `jsonb @? jsonpath` | `boolean` | P2 | path exists — [jsonpath.md §6](jsonpath.md) |
| `@@` | `jsonb @@ jsonpath` | `boolean` | P2 | path match — [jsonpath.md §6](jsonpath.md) |

`@>` / `<@` / `?` / `?|` / `?&` are exactly the predicates a future GIN `jsonb_ops` opclass
accelerates ([gin.md](gin.md) — the seam already seats it); J5 ships them as sequential
predicates, GIN pushdown a deferred follow-on.

### 1.1 The `json` accessor overloads (verbatim)

`->`, `->>`, `#>` and `#>>` also take a `json` base (result `json` for `->`/`#>`). Unlike every
other `json` overload, they do **not** parse to a node tree: a `json` value is its validated input
text ([json.md §4](json.md)), and PostgreSQL's `get_worker` (jsonfuncs.c) returns the matched
**sub-text exactly as written** — inner whitespace, key order, number spelling (`1.50`, `1e2`) and
escapes kept, surrounding whitespace dropped. Each core walks the text with a small span scanner
(hand-written, json.md §4) that mirrors the streaming walk, so the observable rules are PG's:

- **Duplicate keys.** An object step matches *every* member of that name, and the result is the
  **last** match in document order. So `'{"a":1,"a":2}'::json -> 'a'` is `2`, and a path walks every
  duplicate: `'{"a":{"b":1}, "a":{"c":2}}'::json #> '{a,b}'` is `1` (the later `a` has no `b`).
- **Text forms.** `->>` / `#>>` de-escape a matched string and turn a matched JSON `null` into SQL
  NULL; anything else is the sub-text verbatim (`'{"n": 1.50}'::json ->> 'n'` is `1.50`).
- **Empty path.** `#> '{}'` returns the whole document: a container without its surrounding
  whitespace, but a *scalar* from the start of the text (PG keeps the leading whitespace, trims the
  trailing).
- **Negative path index — a PG quirk, matched.** A negative index is rebased onto the length of the
  **first array met at its depth, matched or not**, and the rebased index then carries to every later
  array at that depth: `'{"a":[1,2,3], "b":[4,5]}'::json #> '{b,-1}'` is SQL NULL (it looks for
  `b[2]`), while the same `jsonb` path is `5`. The quirk is deterministic and oracle-pinned, so jed
  follows it (CLAUDE.md §1) rather than ledgering a divergence.
- **Path step → index** (both `json` and `jsonb`). A step parses as an array index by PG's
  `strtoint` rule: optional leading whitespace, an optional sign, digits, nothing trailing, within
  `i32` — so `'1 '` or an out-of-range step never matches.

Cost is the jsonb accessors' (one `operator_eval`); the scan is bounded by the input text.

---

## 2. Scalar processing & builder functions

New scalar `[[operator]]` rows (`kind = "function"`); hand-written kernels. The builders take
`VARIADIC "any"`; `to_json`, `to_jsonb`, and `array_to_json` are hand-resolved over their catalog
rows, because the conversion needs the argument's static type (§2.2).

| function | signature | result | notes |
|---|---|---|---|
| `to_json(anyelement)` / `to_jsonb(anyelement)` | any value | `json` / `jsonb` | any jed value → its JSON image (§2.2) |
| `json_build_array(VARIADIC "any")` / `jsonb_build_array` | variadic | `json`/`jsonb` | array from heterogeneous args (existing `variadic` facility) |
| `json_build_object(VARIADIC "any")` / `jsonb_build_object` | variadic key/value pairs | `json`/`jsonb` | odd arg count → `22023`; keys and values per §2.2 |
| `json_object(text[])` / `json_object(text[], text[])` / `jsonb_object(…)` | text array(s) | `json`/`jsonb` | the one-array (k,v,…) and two-array (keys, values) forms |
| `json_array_length(json)` / `jsonb_array_length(jsonb)` | array | `i32` | `22023` on non-array |
| `json_typeof(json)` / `jsonb_typeof(jsonb)` | any | `text` | `object`/`array`/`string`/`number`/`boolean`/`null` (SQL NULL → SQL NULL) |
| `json_strip_nulls(json[, bool])` / `jsonb_strip_nulls(jsonb[, bool])` | any | `json`/`jsonb` | remove object members whose value is JSON null |
| `jsonb_set(jsonb, text[], jsonb [, create_if_missing bool])` | object/array | `jsonb` | set at path |
| `jsonb_set_lax(jsonb, text[], jsonb [, create_if_missing bool [, null_value_treatment text]])` | | `jsonb` | `jsonb_set` that is not strict in the value; a SQL NULL value follows `null_value_treatment` (below) |
| `jsonb_insert(jsonb, text[], jsonb [, insert_after bool])` | | `jsonb` | insert at path |
| `jsonb_pretty(jsonb)` | any | `text` | indented render |
| `row_to_json(record [, pretty bool])` | composite | `json` | composite value → object (§2.2) |
| `array_to_json(anyarray [, pretty bool])` | array | `json` | array value → JSON array (§2.2) |

**The path mutators' signatures.** `jsonb_set`, `jsonb_set_lax`, and `jsonb_insert` are resolved by
name, not from catalog rows: the `text[]` path takes a bare `'{a,b}'` literal, the value takes a bare
string literal as `jsonb`, and `jsonb_set_lax`'s text DEFAULT is outside the catalog's integer-only
`arg_defaults` ([functions.md §11](functions.md)). They still take PostgreSQL's named notation, with
PostgreSQL's parameter names: `jsonb_in`, `path`, `replacement`, then `create_if_missing`
(`jsonb_set`, `jsonb_set_lax`, default `true`) or `insert_after` (`jsonb_insert`, default `false`), then
`null_value_treatment` (`jsonb_set_lax`, default `'use_json_null'`).

**`jsonb_set_lax`.** PostgreSQL's `jsonb_set_lax` is not STRICT, and jed follows its order of checks
exactly:

1. A NULL `jsonb_in`, `path`, or `create_if_missing` → SQL NULL.
2. A NULL `null_value_treatment` → `22023` `null_value_treatment must be "delete_key",
   "return_target", "use_json_null", or "raise_exception"`, even when the value is not NULL.
3. A non-NULL `replacement` → exactly `jsonb_set`. The treatment is not consulted, so an unrecognized
   one is accepted.
4. A SQL NULL `replacement` → by treatment (matched case-sensitively):
   `'use_json_null'` sets a JSON `null` (as `jsonb_set` with `'null'`, honoring `create_if_missing`);
   `'delete_key'` deletes the path (as `#-`, ignoring `create_if_missing`); `'return_target'` returns
   `jsonb_in` unchanged, even a scalar; `'raise_exception'` → `22004` `JSON value must not be null`, with
   PostgreSQL's Detail and Hint ([error-fields.md](error-fields.md) §9.2, §11.4); anything else → the
   step-2 `22023`.

Each branch inherits its kernel's behavior (§2.1): the scalar-target `22023`s, the `22P02`
non-integer array step, and the `22004` NULL path element. Cost follows the branch:
the set branches are a growth kernel like `jsonb_set` ([cost.md](cost.md) §8.1) and check the result's
depth ([json.md](json.md) §6.4); `delete_key` and `return_target` only remove from or return the target,
so like `#-` they charge one `operator_eval` and no output bytes.

The json-vs-jsonb observable differences (where they exist) are recorded as catalog data:
`json_typeof`/`json_array_length` parse the textual form; the json builders preserve insertion
order and duplicate keys, the jsonb builders canonicalize ([json.md §2.3](json.md)).

### 2.1 The path walk (`jsonb_set`, `jsonb_set_lax`, `jsonb_insert`, `#-`)

The path mutators share one walk, ported from PostgreSQL's `setPath` (jsonfuncs.c); `jsonb_set_lax`
reaches it through `jsonb_set` or, for `'delete_key'`, through `#-`. After the STRICT NULL-argument
check (jsonb_set_lax: the order above), the checks run in PostgreSQL's order:

1. A multidimensional path → `2202E` `wrong number of array subscripts`.
2. A scalar root → `22023` `cannot set path in scalar` (`#-`: `cannot delete path in scalar`), even
   for an empty path.
3. An empty object or array returns the target unchanged for `jsonb_set` without
   `create_if_missing` and for `#-` (not for `jsonb_insert`, which walks an empty container).
4. An empty path returns the target unchanged.
5. The walk, one path element per level, each element checked **when the walk reaches it**:
   - A NULL element → `22004` `path element at position N is null` (N 1-based). The check precedes
     looking at the node, so a NULL under a scalar member still raises (`jsonb_set('{"a":1}',
     '{a,NULL}', …)`), while a NULL past a missing key or an out-of-range index is never reached and
     the target comes back unchanged.
   - A scalar below the root ends the walk; the target comes back unchanged (no error).
   - An object step matches its key. At the last step the member is replaced, deleted (`#-`), or —
     for `jsonb_insert` — refused with `22023` `cannot replace existing key` (hint `Try using the
     function jsonb_set to replace key value.`); a missing last key is added by `jsonb_set` with
     `create_if_missing` and by `jsonb_insert`. A missing earlier key is a no-op.
   - An array step must parse as PostgreSQL's `strtoint`: optional leading whitespace, an optional
     sign, digits to the end, within `i32` — else `22P02` `path element at position N is not an
     integer: "<step>"` (for `#-` too). A negative index counts from the end; one before the start
     makes the adding forms prepend (`jsonb_insert` with `insert_after` included); one past the end
     clamps to the length, so the adding forms append.

`- text[]` (PostgreSQL's `jsonb_delete_array`) shares steps 1–4 with the message `cannot delete from
scalar`, and then **skips** a NULL key. The extractors differ: `#>`, `#>>` (jsonb and the verbatim
json overloads, §1.1) return SQL NULL when the path holds any NULL element.

### 2.2 The JSON image of a value

`to_json`, `to_jsonb`, `array_to_json`, `row_to_json`, `JSON_SCALAR` (§5.2), the builders, and the
aggregates (§4) convert their arguments with one rule, a port of PostgreSQL's `datum_to_json`
(json.c) and `datum_to_jsonb` (jsonb.c). Each core implements it once (`row_json.rs` /
`row_json.go` / `row_json.ts`). The conversion reads the argument's static type, because a
composite's field names key its members. So the five functions are hand-resolved, and the builders
and aggregates carry their arguments' types to evaluation. The catalog rows of `to_json`, `to_jsonb`,
`array_to_json`, and `json_scalar` remain as data; `row_to_json` has none, because the catalog has no
`record` family.

**The value's JSON image:**

| Value | `json` image | `jsonb` image |
|---|---|---|
| `NULL` (a field, element, or builder argument) | `null` | `null` |
| `boolean` | `true` / `false` | the same |
| integer, `decimal` | the number's text, exact (`1.50` keeps its scale) | a number, exact |
| `text` | an escaped JSON string (`\"`, `\\`, `\n`, `\t`, …, other controls as `\u00XX`) | a string |
| `json` | the value's text, verbatim | the parsed document (duplicate keys collapse, the last wins) |
| `jsonb` | the canonical, spaced text (`{"a": 1}`) | the value itself |
| array | `[`…`]` with `,` separators, elements by these rules. A multidimensional array nests one level per dimension; the lower bounds are dropped; an empty array is `[]` | nested arrays, the same shape |
| composite | an object, `{"name":value,…}` in declaration order | an object in canonical key order (json.md §2.3) |
| `date` | `"2024-01-15"` (a BC date keeps its ` BC` suffix) | the same string |
| `timestamp` | ISO 8601 with a `T` separator: `"2024-01-15T10:30:00.5"`, fraction trimmed, ` BC` suffix | the same string |
| `timestamptz` | in the session zone, with an offset of at least hours and minutes: `"2024-01-15T17:30:00+05:30"`, `"…+00:00"`, an LMT `"…-04:56:02"` | the same string |
| `interval`, `uuid`, `bytea`, range, `jsonpath` | the JSON string of the type's text output (`"1 day 02:00:00"`, `"\\x0102"`, `"[1,5)"`). A `timestamptz` range bound renders in the session zone | the same string |
| `f32` / `f64` | PostgreSQL's `float8out` / `float4out` text as a number (`1.5`, `1e+21`, `-0`, `0.1` for `0.1::f32`); NaN and ±Infinity as the strings `"NaN"`, `"Infinity"`, `"-Infinity"` (below) | the `numeric` that text reads as (`1000000000000000000000`; `-0` is `0`), or the same string |

The datetime forms are PostgreSQL's `JsonEncodeDateTime` (the `XSD` date style). They differ from
the text output in two places: the `T` separator, and a `timestamptz` offset that always shows
minutes. `±infinity` renders as the words `"infinity"` and `"-infinity"`. A `json` value renders as
`json` text inside an array or composite, so `to_json(ARRAY['{"a" :1}'::json])` is `[{"a" :1}]`; a `jsonb`
element renders spaced, so `to_json(ARRAY['{"a":1}'::jsonb])` is `[{"a": 1}]`.

**The functions.**

- `to_json(anyelement)` is the value's `json` image; `to_jsonb(anyelement)` its `jsonb` image. Any type
  is accepted. Both are STRICT.
- `array_to_json(anyarray [, pretty boolean])` is an array's `json` image. With `pretty = true`, the
  items of the outermost dimension are separated by `,\n ` (a comma, a newline, and one space). Inner
  dimensions and the elements never break lines.
- `row_to_json(record [, pretty boolean])` is a composite's `json` object. A named composite takes its
  member names from the type's fields. An anonymous `ROW(…)` names its fields `f1`, `f2`, …, as
  PostgreSQL does. `ROW()` renders as `{}`. With `pretty = true`, the separator between top-level
  members is `,\n `; nested composites and arrays never break lines.

`array_to_json` must take an array and `row_to_json` a composite. An untyped `NULL` is accepted and
yields `NULL`. A bare string literal passed to `row_to_json` is `0A000` `input of anonymous composite
types is not implemented`, because PostgreSQL reads it as an anonymous `record`. Any other argument type
is `42883`. The optional flag must be a boolean; a bare string literal adapts to it (`22P02` if it is
not a boolean). Any other flag type, and any other argument count, is `42883`. A `NULL` argument or
flag yields `NULL`.

**Object keys.** `json[b]_build_object` and `json[b]_object_agg` (§4) take any scalar as a key, and a
key is always a JSON string. A number or boolean is its text (`1.50` → `"1.50"`, `true` → `"true"`). A
`date`, `timestamp`, or `timestamptz` is its ISO 8601 form above, without the quotes. Any other scalar is
its text output. An array, composite, `json`, or `jsonb` key is `22023` `key value must be scalar, not
array, composite, or json`. A NULL key is an error, as in PostgreSQL: `json_build_object` and
`json_object_agg` give `22004` `null value not allowed for object key`; `jsonb_build_object` gives
`22023` `argument N: key must not be null`; `jsonb_object_agg` gives `22023` `field name must not be
null`.

**Builders.** `json_build_array` joins its arguments' `json` images with `, ` inside `[`…`]`.
`json_build_object` writes `key : value` pairs joined with `, ` inside `{`…`}`, in argument order, with
duplicate keys kept. The `jsonb` builders build the `jsonb` images; `jsonb_build_object` keeps the last of
duplicate keys. In the `VARIADIC`-array form, each element is converted by the array's element type.

**Whole-row references are not expressible.** PostgreSQL's common `row_to_json(t)` passes a table
alias as its row. jed has no implicit per-table row type ([composite.md](composite.md) §10.4) and
no row-valued table reference ([grammar.md](grammar.md) §15). There, `t` resolves as a missing
column (`42703`). Use a composite column or `ROW(t.a, t.b, …)` instead. This divergence is
recorded in the override ledger.

**Floats take PostgreSQL's exact text.** A float source, element, field, key, or aggregate input is
written as PostgreSQL's `float8out` (`float4out` for an `f32`) text at `extra_float_digits = 1`:
Ryu's shortest digits and its layout, the text every float rendering uses ([float.md](float.md) §9),
byte-identical across cores and with PostgreSQL. A key is the text (`json_build_object(1.5e20::f64,
1)` is `{"1.5e+20" : 1}`).

**Session zone.** A `timestamptz` anywhere in an argument's type (the value itself, an array element,
a field, a range bound, at any depth) is rendered in the session zone. Such a call to `to_json`,
`to_jsonb`, `array_to_json`, `row_to_json`, `JSON_SCALAR`, or a builder is nonimmutable: it is
`42P17` in an index expression or predicate. Other calls are admitted, under the per-operation
dependency rule of [index-dependencies.md](index-dependencies.md). PostgreSQL marks these functions
STABLE for every argument. Its `JSON_SCALAR` is checked per argument type instead
(`to_json_is_immutable`), and also rejects a `date` or `timestamp`, whose text output follows
`DateStyle` there. jed's ISO form follows no setting, so jed admits them.

**Cost.** Each function charges one `operator_eval` for the call; a builder also charges one
`operator_eval` per argument value. Then each finite `timestamptz` rendered charges one `timezone`
unit, charged and guarded before rendering, as for the array → text cast ([timezones.md](timezones.md)
§9.5). An aggregate charges the `timezone` units of each input (key and value) as it folds it, with its
`generated_row`. The functions and builders are growth kernels ([cost.md](cost.md) §8.1): they charge
`scalar_byte` × the result's payload (`json`: UTF-8 bytes), and nothing when every argument is
constant. A result passes the depth check ([json.md](json.md) §6.4), since a `jsonb` field or element
can nest deep.

---

## 3. Set-returning functions (SRFs)

New `[[set_returning]]` rows. The single-column ones reuse the existing SRF machinery
(`unnest`/`generate_series` model). The **two-column** ones (`json[b]_each[_text]`) need the
shared **multi-column synthetic table** built by C0 ([json-table.md §1](json-table.md)).

| function | columns | result | machinery |
|---|---|---|---|
| `json_array_elements(json)` / `jsonb_array_elements(jsonb)` | 1 (`value`) | setof `json`/`jsonb` | existing SRF |
| `json_array_elements_text(json)` / `jsonb_array_elements_text` | 1 (`value`) | setof `text` | existing SRF |
| `json_object_keys(json)` / `jsonb_object_keys(jsonb)` | 1 (`json_object_keys`) | setof `text` | existing SRF; **jsonb** in canonical key order, **json** in input order (the observable json/jsonb difference) |
| `json_each(json)` / `jsonb_each(jsonb)` | 2 (`key text`, `value json`/`jsonb`) | setof record | **needs C0 multi-column SRF** |
| `json_each_text(json)` / `jsonb_each_text(jsonb)` | 2 (`key text`, `value text`) | setof record | **needs C0 multi-column SRF** |
| `jsonb_path_query(…)` | 1 | setof `jsonb` | [jsonpath.md §5.2](jsonpath.md) |
| `json[b]_to_recordset` / `json[b]_populate_recordset` | N | setof record | [json-table.md §4](json-table.md) (needs C0) |

All are `empty_on_null` (SQL NULL or empty input → 0 rows) and `immutable`, and — being SRFs
— **implicitly lateral**, so a correlated `json_each(t.doc)` works with no LATERAL slice.

The `value` / `key` column names are PostgreSQL's declared OUT parameters, so a table alias renames
only the relation (`jsonb_array_elements(doc) AS e` exposes `e.value`); `json[b]_object_keys` and
`jsonb_path_query` declare none, so their single column takes the alias, else the function name
([functions.md](functions.md) §10).

The `json` variants of `array_elements[_text]` and `each[_text]` are verbatim like the `json`
accessors (§1.1): `json_array_elements` yields each element's exact sub-text, `json_each` yields
every top-level member in **input order with duplicate keys kept** (keys de-escaped, values their
exact sub-text), and the `_text` forms de-escape a string / map a JSON `null` to SQL NULL. A
non-array / non-object argument is `22023` with PG's messages (`cannot call json_array_elements on a
non-array` / `… on a scalar`, `cannot deconstruct an array as an object` / `cannot deconstruct a
scalar`). Each row charges one `generated_row`, like the jsonb forms.

---

## 4. Aggregates

New `[[aggregate]]` rows; reuse the existing aggregate facility (accumulator + finalize). The
`_unique` variants and the `IS JSON … WITH UNIQUE KEYS` predicate and the object constructors
share **one duplicate-key kernel** (§6.2).

| aggregate | signature | result | notes |
|---|---|---|---|
| `json_agg(anyelement)` / `jsonb_agg(anyelement)` | any | `json`/`jsonb` | aggregate values into a JSON array (input order under `ORDER BY`) |
| `json_agg_strict` / `jsonb_agg_strict` | any | `json`/`jsonb` | skip SQL NULL inputs |
| `json_object_agg(key, value)` / `jsonb_object_agg(key, value)` | k, v | `json`/`jsonb` | aggregate into an object |
| `json_object_agg_strict` / `jsonb_object_agg_strict` | k, v | `json`/`jsonb` | skip rows with NULL value |
| `json_object_agg_unique` / `jsonb_object_agg_unique` | k, v | `json`/`jsonb` | `22030` on duplicate key (shared kernel §6.2) |
| `json_object_agg_unique_strict` / `jsonb_object_agg_unique_strict` | k, v | | both unique + strict |

Each input is converted as it is folded, by the rule of §2.2 (keys too). `json_agg` joins the element
images with `, `; an array or composite element after the first also starts a new line (`, ` then
`\n `), as PostgreSQL's `json_agg_transfn` does. `json_object_agg` writes `key : value` pairs joined
with `, ` inside padded braces, `{ "a" : 1, "b" : 2 }`. A NULL key is `22004` for `json_object_agg` and
`22023` (*"field name must not be null"*) for `jsonb_object_agg`, as in PostgreSQL; ordering inside the
aggregate follows any `ORDER BY` in the aggregate call (`jsonb_agg(x ORDER BY y)`,
`jsonb_object_agg(k, v ORDER BY ts)` — the generic in-aggregate `ORDER BY`, [aggregates.md](aggregates.md)
§22), so `jsonb_object_agg`'s last-wins duplicate key is the last in key order. `DISTINCT` and `FILTER`
apply as for any aggregate (the object aggregates de-duplicate the `(key, value)` pair; a `json`
argument under `DISTINCT` is `42883`).

---

## 5. SQL/JSON standard syntax (SQL:2016 / PG 16–17)

`JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`, the constructors `JSON()` / `JSON_SCALAR()` /
`JSON_SERIALIZE()`, and the `IS JSON` predicate are **SQL syntax with sub-clauses**, not
ordinary function calls — they need grammar productions + AST nodes + hand-written
resolution (catalog overload dispatch can't express `RETURNING` / `ON ERROR` / `WRAPPER`).

### 5.1 Grammar sketch (full forms in [../grammar/grammar.ebnf](../grammar/grammar.ebnf))

```
json_value_func ::= "JSON_VALUE" "(" ctx "," path json_passing? json_returning?
                       on_clause? on_clause? ")"
json_query_func ::= "JSON_QUERY" "(" ctx "," path json_passing? json_returning?
                       wrapper_clause? quotes_clause? on_clause? on_clause? ")"
json_exists_func::= "JSON_EXISTS" "(" ctx "," path json_passing?
                       ( ("TRUE"|"FALSE"|"UNKNOWN"|"ERROR") "ON" "ERROR" )? ")"
ctx             ::= expr ( "FORMAT" "JSON" )?           (* json/jsonb/text coerced to jsonb *)
path            ::= string_literal                      (* compiled to jsonpath *)
json_passing    ::= "PASSING" passing_arg ( "," passing_arg )*
passing_arg     ::= expr ( "FORMAT" "JSON" )? "AS" identifier   (* binds $identifier in the path, §5.4 *)
json_returning  ::= "RETURNING" type ( "FORMAT" "JSON" )?
wrapper_clause  ::= "WITH" ("CONDITIONAL"|"UNCONDITIONAL")? "ARRAY"? "WRAPPER"
                  | "WITHOUT" "ARRAY"? "WRAPPER"
quotes_clause   ::= ("KEEP"|"OMIT") "QUOTES" ( "ON" "SCALAR" "STRING" )?
on_clause       ::= behavior ( "ON" "EMPTY" | "ON" "ERROR" )
behavior        ::= "ERROR" | "NULL" | "TRUE" | "FALSE" | "UNKNOWN"
                  | "EMPTY" ("ARRAY"|"OBJECT")? | "DEFAULT" expr      (* DEFAULT deferred, §5.3 *)
json_constructor::= "JSON" "(" expr json_returning? ( ("WITH"|"WITHOUT") "UNIQUE" "KEYS"? )? ")"
json_scalar     ::= "JSON_SCALAR" "(" expr ")"
json_serialize  ::= "JSON_SERIALIZE" "(" expr json_returning? ")"
is_json_pred    ::= expr "IS" "NOT"? "JSON"
                       ("VALUE"|"SCALAR"|"ARRAY"|"OBJECT")?
                       ( ("WITH"|"WITHOUT") "UNIQUE" "KEYS"? )?
```

`JSON_VALUE`/`JSON_QUERY`/`JSON_EXISTS`/`JSON()`/`JSON_SCALAR`/`JSON_SERIALIZE` start a
**primary expression** (a leading keyword); `IS JSON` extends the existing **IS-predicate
dispatch** (the same parse site as `IS NULL` / `IS DISTINCT FROM`).

### 5.2 Evaluation

All three query functions compile `path` to a `jsonpath` program, evaluate it to a sequence
over the context item ([jsonpath.md §3](jsonpath.md)) — a `json` context is first converted to
`jsonb`, as PostgreSQL casts it ([jsonpath.md §5](jsonpath.md)) — then:

- **`JSON_EXISTS`** — non-empty → true (a lax path stops at its first item, jsonpath.md §5);
  errors honor `ON ERROR` (PG default `FALSE`).
- **`JSON_VALUE`** — requires a **single scalar** item, coerced to the `RETURNING` type
  (default `text`); empty → `ON EMPTY` (default `NULL`); error / non-scalar → `ON ERROR`
  (default `NULL`); `>1` item → `22034`; non-scalar → `22038`.
- **`JSON_QUERY`** — yields `json`/`jsonb`; `WRAPPER` controls array-wrapping
  (`UNCONDITIONAL` always wraps the sequence in an array, `CONDITIONAL` only when `>1` item,
  `WITHOUT` requires a singleton); `QUOTES` controls scalar-string de-quoting; `ON EMPTY` /
  `ON ERROR` default `NULL`.
- **`JSON()`** — parse text to `jsonb`/`json`; `WITH UNIQUE KEYS` → `22030` on a duplicate
  key (shared kernel §6.2). **`JSON_SCALAR(expr)`** — the value's `json` image, exactly `to_json`'s
  (§2.2): PostgreSQL's executor converts a `JSCTOR_JSON_SCALAR` argument with `datum_to_json`. So a
  `date`/`timestamp`/`timestamptz` is an ISO 8601 string (a `timestamptz` in the session zone, which
  makes the call nonimmutable), an `interval`/`uuid`/`bytea`/range/`jsonpath` the string of its text
  output, and PostgreSQL does not restrict it to scalars: an array is a JSON array, a composite an
  object, a `json` value verbatim. A bare string literal is `text`. A float is its `float8out` text
  (§2.2). It costs as `to_json`. PG 18 has no `RETURNING` clause here, so the result is always `json`.
  **`JSON_SERIALIZE(expr RETURNING text)`** — a JSON value → its text serialization.
- **`IS JSON`** — tests well-formedness (and the optional `VALUE`/`SCALAR`/`ARRAY`/`OBJECT`
  kind and `WITH UNIQUE KEYS`); a non-text/json operand → false (PG); never raises.

The `RETURNING type` is statically known, so the resolver **fixes the result column type at
plan time** — a clean fit for jed's strict static type system. The jsonb-scalar → target-type
coercion reuses the `casts.toml` text→type paths (stringify the scalar) and the direct
jsonb-number → `decimal` path; a failed coercion under `ERROR ON ERROR` is `2203A`.

### 5.3 What is hard (and what defers)

**Status: S2 has landed** — `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` ship (capability
`expr.json_query_fns`, all three cores, oracle-clean). The **constant** behaviors
(`ERROR`/`NULL`/`TRUE`/`FALSE`/`UNKNOWN`/`EMPTY ARRAY|OBJECT`), `RETURNING type`, and
`JSON_QUERY`'s `WITH/WITHOUT [COND|UNCOND] ARRAY WRAPPER` + `KEEP QUOTES` are implemented.
The result type is fixed at resolve (`RExpr::JsonSqlFn`); a SQL/JSON (class-`22`) evaluation
error honors `ON ERROR`, while a resource/cost abort propagates. Reading a `json` context as jsonb is
the context's conversion, not evaluation, so its `22P05` / `22P02` escape errors
([json.md §4.2](json.md)) propagate past `ON ERROR`, as in PostgreSQL. The exact PG codes match on
`ERROR ON ERROR` (`2203F` non-scalar, `22034` >1 item).

Still **deferred** (each `0A000`, a documented divergence):

- **`ON ERROR / ON EMPTY DEFAULT expr`** makes the function partly non-constant and requires
  a **guarded sub-evaluation** (try the kernel; on a SQL/JSON error substitute the default
  expression without unwinding the statement) — a genuinely new evaluator capability. **It is
  deferred to slice S3**.
- **`JSON_QUERY OMIT QUOTES`** — PG returns the de-quoted scalar string still typed `jsonb`
  (an invalid-`jsonb` quirk that fits jed's strict static typing poorly); deferred.
- **`JSON_QUERY RETURNING` a non-json type** — the wrapper/quotes result is a JSON value;
  coercing it to an arbitrary scalar is a follow-on.
- **`IS JSON … WITH UNIQUE KEYS`** (S1) uses the shared duplicate-key kernel (§6.2).

### 5.4 `PASSING`

**Status: landed** (capability `expr.jsonpath_vars`). `PASSING value [FORMAT JSON] AS name, …`
after the path of `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`, and after the root path (and its
optional `AS name`) of `JSON_TABLE` ([json-table.md](json-table.md) §3.1), binds the path variable
`$name` ([jsonpath.md §5](jsonpath.md)). The name is an identifier, folded like one (`AS X` binds
`$x`, `AS "X"` binds `$X`). When two bindings share a name the first wins, as PostgreSQL's lookup
finds it. A path variable with no binding is `42704`, which `ON ERROR` does not catch.

Each value becomes a jsonpath item the way PostgreSQL's `transformJsonPassingArgs` and
`JsonItemFromDatum` make it:

- a `boolean`, an integer, or a `decimal` value is that item; a `text` value is a string item, even
  when its text reads as JSON (a bare string literal is `text`); SQL NULL is JSON `null`;
- a float is the number PostgreSQL's `float8_numeric` (`float4_numeric` for an `f32`) makes of it:
  `%.15g` (`%.6g`) of the exact value read back as a `numeric`, so `1.0000000000000002::f64` is `1`
  and `0.1::f32` is `0.1`. NaN and ±Infinity are below;
- a `jsonb` value is itself, and a `json` value is converted to `jsonb` (keys canonical, the last
  duplicate key winning);
- any other type is converted by `to_jsonb` (§2.2): an array is a JSON array, a composite an object,
  an `interval`/`uuid`/`bytea`/range/`jsonpath` the string of its text output. A `timestamptz` inside
  one renders in the session zone, which makes the expression nonimmutable, as for `to_jsonb`;
- `FORMAT JSON` parses a `text` value as JSON text (`22P02` when malformed) and leaves `json`/`jsonb`
  alone; any other type is converted by `to_jsonb`, so a `date` becomes a string.

**Not supported (`0A000`), ledgered.** A `date`, `timestamp`, or `timestamptz` value without
`FORMAT JSON` is a jsonpath *datetime item* in PostgreSQL, with its own `.type()` and comparison
rules. jed's jsonpath has no datetime items, by decision ([jsonpath.md §3.1](jsonpath.md)); pass
`d::text` (or use `FORMAT JSON`, which makes it a string) instead. **Deferred (`0A000`),
ledgered.** A NaN or ±Infinity float without `FORMAT JSON` is a `numeric` NaN/Infinity item in
PostgreSQL, which jed's finite `decimal` cannot hold; it is raised when the value is evaluated, since
a float column's other values pass. (Under `FORMAT JSON` it is `to_jsonb`'s string.) `FORMAT JSON`
on a `bytea` value (encoded JSON text, with an optional `ENCODING`) is deferred too.

Evaluation order follows PostgreSQL's. For the query functions the context and path come first (a
NULL either way is SQL NULL without evaluating the `PASSING` values), then the `PASSING` values in
order, then the path; the values are evaluated outside `ON ERROR`. `JSON_TABLE` evaluates its
`PASSING` values before its context, and every path in it — the root path, the column paths, `EXISTS`
columns, and `NESTED` paths — reads them. Each value is a `.keyvalue()` base object numbered by its
position in the clause ([jsonpath.md §3.1](jsonpath.md)). A value costs what its expression costs
(a converted one what `to_jsonb` or the cast costs); binding and reading it charge nothing more.

EXPLAIN VERBOSE renders the clause after the path: `json_value(ctx, path passing value as name, …
returning …)`, each value as resolved (a converted value shows its `to_jsonb(…)` or `cast(… as
jsonb)`).

---

## 6. New machinery (only two pieces)

Everything above fits an existing jed mechanism except:

### 6.1 The multi-column synthetic table (C0)

`json[b]_each[_text]` (two columns) and the record-returning functions
([json-table.md](json-table.md), N columns) need a FROM-clause source that yields **multiple
named/typed columns**. Today the SRF path produces a single synthetic column. C0
([json-table.md §1](json-table.md)) generalizes the synthetic `Table` to N columns and is the
shared prerequisite — build it once, before `json_each` and the record functions.

### 6.2 The duplicate-key kernel

A single pure, deterministic kernel — "does this object have a duplicate key?" — is shared by
`json[b]_object_agg_unique[_strict]` (§4), `JSON(... WITH UNIQUE KEYS)` and
`IS JSON … WITH UNIQUE KEYS` (§5). On a duplicate it raises **`22030`** (aggregate/constructor
context) or **`22037`** (object-construction context), per PG. It operates on the parsed
key list before canonical de-dup, so it observes the duplicate `jsonb_in` would otherwise
collapse.

---

## 7. Delivery — vertical slices

After the `jsonb` foundation ([json.md §12](json.md)) and `jsonpath` ([jsonpath.md §9](jsonpath.md)):

- **B1** — scalar processing + builders (§2). Capability `func.json_builders`.
- **B2** — single-column SRFs (`json[b]_array_elements[_text]`, `json[b]_object_keys`, §3).
  Capability `func.json_srf`.
- **B3** — two-column SRFs (`json[b]_each[_text]`, §3) — depends on **C0**.
- **B4** — aggregates (§4) — depends on B1's duplicate-key kernel (§6.2). Capability
  `func.json_agg`.
- **S1** — `IS JSON` + `JSON()` / `JSON_SCALAR` / `JSON_SERIALIZE` + the duplicate-key kernel
  (§5, §6.2). Grammar + simple kernels; no path needed beyond parsing. Capability `func.json_sql`.
- **S2** — `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` with constant `ON ERROR`/`ON EMPTY`/
  `WRAPPER`/`QUOTES` (§5.2). Depends on P1 ([jsonpath.md](jsonpath.md)).
- **S3** (deferred) — `ON ERROR / ON EMPTY DEFAULT expr` (the guarded sub-evaluation, §5.3).

B-series runs in parallel off the J0/J-foundation and C0; S-series depends on the `jsonpath`
type. The record-returning functions and `JSON_TABLE` are in [json-table.md](json-table.md).
