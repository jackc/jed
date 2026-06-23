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

---

## 2. Scalar processing & builder functions

New scalar `[[operator]]` rows (`kind = "function"`); hand-written kernels. `to_json`/
`to_jsonb` and the builders use the polymorphic `anyelement` resolution the array surface
already built.

| function | signature | result | notes |
|---|---|---|---|
| `to_json(anyelement)` / `to_jsonb(anyelement)` | any value | `json` / `jsonb` | any jed value → its JSON image; uses `anyelement` resolution |
| `json_build_array(VARIADIC "any")` / `jsonb_build_array` | variadic | `json`/`jsonb` | array from heterogeneous args (existing `variadic` facility) |
| `json_build_object(VARIADIC "any")` / `jsonb_build_object` | variadic key/value pairs | `json`/`jsonb` | odd arg count → `22023` |
| `json_object(text[])` / `json_object(text[], text[])` / `jsonb_object(…)` | text array(s) | `json`/`jsonb` | the one-array (k,v,…) and two-array (keys, values) forms |
| `json_array_length(json)` / `jsonb_array_length(jsonb)` | array | `i32` | `22023` on non-array |
| `json_typeof(json)` / `jsonb_typeof(jsonb)` | any | `text` | `object`/`array`/`string`/`number`/`boolean`/`null` (SQL NULL → SQL NULL) |
| `json_strip_nulls(json[, bool])` / `jsonb_strip_nulls(jsonb[, bool])` | any | `json`/`jsonb` | remove object members whose value is JSON null |
| `jsonb_set(jsonb, text[], jsonb [, create_if_missing bool])` | object/array | `jsonb` | set at path |
| `jsonb_set_lax(jsonb, text[], jsonb, create_if_missing bool, null_value_treatment text)` | | `jsonb` | adds the `null_value_treatment` enum (`raise_exception`/`use_json_null`/`delete_key`/`return_target`) |
| `jsonb_insert(jsonb, text[], jsonb [, insert_after bool])` | | `jsonb` | insert at path |
| `jsonb_pretty(jsonb)` | any | `text` | indented render |
| `row_to_json(record [, pretty bool])` | composite | `json` | composite value → object |
| `array_to_json(anyarray [, pretty bool])` | array | `json` | array value → JSON array |

The json-vs-jsonb observable differences (where they exist) are recorded as catalog data:
`json_typeof`/`json_array_length` parse the textual form; the json builders preserve insertion
order and duplicate keys, the jsonb builders canonicalize ([json.md §2.3](json.md)).

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

`json_object_agg` with a NULL key → `22030`/`23502`-class per PG; ordering inside the
aggregate follows any `ORDER BY` in the aggregate call (the existing ordered-aggregate path).

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
passing_arg     ::= expr "AS" identifier                (* binds $identifier in the path *)
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
over the context item ([jsonpath.md §3](jsonpath.md)), then:

- **`JSON_EXISTS`** — non-empty → true; errors honor `ON ERROR` (PG default `FALSE`).
- **`JSON_VALUE`** — requires a **single scalar** item, coerced to the `RETURNING` type
  (default `text`); empty → `ON EMPTY` (default `NULL`); error / non-scalar → `ON ERROR`
  (default `NULL`); `>1` item → `22034`; non-scalar → `22038`.
- **`JSON_QUERY`** — yields `json`/`jsonb`; `WRAPPER` controls array-wrapping
  (`UNCONDITIONAL` always wraps the sequence in an array, `CONDITIONAL` only when `>1` item,
  `WITHOUT` requires a singleton); `QUOTES` controls scalar-string de-quoting; `ON EMPTY` /
  `ON ERROR` default `NULL`.
- **`JSON()`** — parse text to `jsonb`/`json`; `WITH UNIQUE KEYS` → `22030` on a duplicate
  key (shared kernel §6.2). **`JSON_SCALAR(expr)`** — a jed scalar → its JSON scalar.
  **`JSON_SERIALIZE(expr RETURNING text)`** — a JSON value → its text serialization.
- **`IS JSON`** — tests well-formedness (and the optional `VALUE`/`SCALAR`/`ARRAY`/`OBJECT`
  kind and `WITH UNIQUE KEYS`); a non-text/json operand → false (PG); never raises.

The `RETURNING type` is statically known, so the resolver **fixes the result column type at
plan time** — a clean fit for jed's strict static type system. The jsonb-scalar → target-type
coercion reuses the `casts.toml` text→type paths (stringify the scalar) and the direct
jsonb-number → `decimal` path; a failed coercion under `ERROR ON ERROR` is `2203A`.

### 5.3 What is hard (and what defers)

- **`ON ERROR / ON EMPTY DEFAULT expr`** makes the function partly non-constant and requires
  a **guarded sub-evaluation** (try the kernel; on a SQL/JSON error substitute the default
  expression without unwinding the statement) — a genuinely new evaluator capability. **It is
  deferred to slice S3**; the first pass ships only the **constant** behaviors
  (`ERROR`/`NULL`/`TRUE`/`FALSE`/`UNKNOWN`/`EMPTY ARRAY|OBJECT`).
- **`PASSING … AS name`** reuses the `vars` substitution from [jsonpath.md §5](jsonpath.md).
- **`IS JSON … WITH UNIQUE KEYS`** uses the shared duplicate-key kernel (§6.2).

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
