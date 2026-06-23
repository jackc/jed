# JSON_TABLE & record-returning functions — design

> The shared FROM-clause **column-definition-list** facility (the keystone prerequisite),
> the record-returning functions (`json[b]_to_record(set)`, `json[b]_populate_record(set)`),
> and `JSON_TABLE` — the SQL/JSON construct that projects a JSON document into a relational
> table with a `COLUMNS` clause, `FOR ORDINALITY`, `EXISTS` columns, and recursive
> `NESTED PATH`. Depends on the document type ([json.md](json.md)), the path language
> ([jsonpath.md](jsonpath.md)), and `JSON_VALUE`/`JSON_EXISTS`
> ([json-sql-functions.md](json-sql-functions.md)). PostgreSQL semantics are the default
> (CLAUDE.md §1), pinned against the live `postgres:18` oracle; the nested-path join
> semantics (§3.3) are subtle and oracle-pinned. Grammar is data in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf).

> **Status: SPEC-FIRST (design ratified, implementation pending).** Implemented by the
> C0 / R-series / T-series slices (§5). This is the **highest-risk** corner of the JSON
> feature; `JSON_TABLE` with the default plan is T1, the explicit `PLAN` clause is the
> deferred T2.

---

## 1. The shared column-definition-list facility (C0 — keystone)

`JSON_TABLE`, `json_to_record` / `jsonb_to_record`, and `json_to_recordset` /
`jsonb_to_recordset` all need a FROM-clause function followed by a column-definition list:

```sql
SELECT * FROM jsonb_to_recordset('[{"a":1,"b":"x"}]') AS t(a i32, b text)
```

This `AS (col type [, …])` form **does not exist** in jed today. The *representation*,
however, does: `CREATE TYPE … AS (field type, …)` already parses to a `Vec<TypeFieldDef>`
(field name + base type + `[]` suffix + modifiers). C0 builds a shared facility on top of it:

- **Grammar.** Extend the `table_function` production
  ([../grammar/grammar.ebnf](../grammar/grammar.ebnf)) so a FROM-clause function call may be
  followed by `AS "(" column_def_list ")"`, where `column_def_list` reuses the composite-field
  production.
- **Resolution.** When the col-def list is present, the resolver attaches the parsed
  `Vec<TypeFieldDef>` to the synthetic `Table` **instead of** deriving columns from the
  function's catalog `result` — the declared types fix the column types statically (a clean
  fit for the strict type system).
- **Multi-column synthetic table.** Generalize the SRF synthetic `Table` (today single-column)
  to **N named/typed columns**. This is the same facility `json[b]_each[_text]` needs
  ([json-sql-functions.md §6.1](json-sql-functions.md)) — build it once.

C0 is small, well-precedented (the composite-field parser + the synthetic-table machinery
already exist), and is the prerequisite that de-risks the record functions and `JSON_TABLE`.
Capability `func.coldeflist`.

---

## 2. Record-returning functions (R-series)

After C0. These are the canonical col-def-list consumers; they are strictly simpler than
`JSON_TABLE` (no nested paths, no ordinality, no outer/union join), so they ship **before**
`JSON_TABLE` and harden the shared machinery.

| function | shape source | result | machinery |
|---|---|---|---|
| `json_to_record(json)` / `jsonb_to_record(jsonb)` | the `AS (col type, …)` list | record (1 row) | C0 col-def list |
| `json_to_recordset(json)` / `jsonb_to_recordset(jsonb)` | the `AS (col type, …)` list | setof record | C0 + multi-column SRF |
| `json_populate_record(base anyelement, json)` / `jsonb_populate_record(…)` | the **composite type** of `base` | record (1 row) | existing `Composite` facility |
| `json_populate_recordset(base anyelement, json)` / `jsonb_populate_recordset(…)` | the composite type of `base` | setof record | composite + multi-column SRF |

The kernel maps JSON object members → declared columns **by name**, coercing each member to
the column type (the `JSON_VALUE` scalar-coercion path,
[json-sql-functions.md §5.2](json-sql-functions.md)); a missing member → SQL NULL; an
extra member is ignored. The `_to_*` forms take the shape from the C0 `AS (…)` list; the
`populate_*` forms take it from the composite type of the (typically NULL-valued) first
argument — leaning on the existing `Type::Composite(catalog-ref)` facility rather than the
col-def list. **Sequencing:** `_to_record(set)` first (R1, exercises C0), then `populate_*`
(R2, adds composite-shape extraction). Capabilities `func.json_record` (R1) /
`func.json_populate` (R2).

---

## 3. `JSON_TABLE` (T-series)

### 3.1 Grammar

```
json_table ::= "JSON_TABLE" "(" ctx "," path ("AS" identifier)?
                 json_passing?
                 "COLUMNS" "(" jt_column ("," jt_column)* ")"
                 -- explicit PLAN clause: deferred (T2), rejected 0A000 in T1
               ")"
jt_column  ::= identifier "FOR" "ORDINALITY"
             | identifier type ("FORMAT" "JSON")? ("PATH" string)?
                   wrapper_clause? quotes_clause? on_clause? on_clause?     -- a "regular" column
             | identifier type "EXISTS" ("PATH" string)? (behavior "ON" "ERROR")?
             | "NESTED" ("PATH")? string ("AS" identifier)?
                   "COLUMNS" "(" jt_column ("," jt_column)* ")"
```

`ctx` / `path` / `json_passing` / `wrapper_clause` / `quotes_clause` / `on_clause` /
`behavior` are the same productions as the SQL/JSON query functions
([json-sql-functions.md §5.1](json-sql-functions.md)). A regular column with **no explicit
`PATH`** defaults to `$.<column_name>` (PG). `JSON_TABLE` is recognized in `table_ref` as a
new alternative beside `table_function`.

### 3.2 As a FROM-clause table source

`JSON_TABLE` produces a **multi-column relation with recursive nested structure**, so it does
**not** fit the single-column SRF plan. It gets a **new planner row source** (`JsonTablePlan`),
but **reuses** the synthetic-`Table` + implicit-lateral prefix-scope machinery that the SRF
and derived-table paths already share:

- It is **implicitly lateral** — its `ctx` and `PASSING` args may reference earlier FROM
  siblings (`JSON_TABLE(t.doc, …)` works) via the existing prefix-scope builder. No LATERAL
  slice needed.
- The `COLUMNS` list reuses the **C0 col-def-list facility** (§1), extended with a per-column
  *kind* (regular / ordinality / exists / nested). The synthetic `Table`'s column types come
  straight from the declared types — the strict-static-type fit again.

### 3.3 Column kinds and the nested-path expansion (the hard part)

- **`name FOR ORDINALITY`** — a per-level 1-based row counter (numbered in outer-to-inner
  traversal order).
- **regular `name type [PATH p] [wrapper][quotes][ON …]`** — evaluate `p` (default
  `$.name`) relative to the current row item and coerce to `type`, exactly like `JSON_VALUE`
  (or `JSON_QUERY` when a `type`/`FORMAT JSON`/`WRAPPER` implies a structural result);
  `ON EMPTY`/`ON ERROR` per column (constant behaviors only — `DEFAULT expr` follows S3).
- **`name type EXISTS [PATH p]`** — `JSON_EXISTS` of `p` per row, coerced to `type`
  (typically `boolean`/`i32`).
- **`NESTED [PATH] p [AS n] COLUMNS (…)`** — recursively expand a child path relative to the
  current row item.

**The default plan (T1) — parent→child LEFT OUTER, sibling NESTED paths UNIONed:**

1. Evaluate the **root path** over `ctx` → a sequence of "row items".
2. For each row item: evaluate each regular column's relative path (singleton-coerced), each
   `EXISTS` column, and advance the level's ordinality counter.
3. For each `NESTED PATH`, evaluate its path **relative to the current row item** → a child
   sequence, and produce the **LEFT OUTER** product: each parent row joins to each child row;
   if the nested path yields nothing, **one** parent row emerges with the nested columns NULL.
4. **Sibling** `NESTED` paths at the same level are combined by **UNION**, not cross-join:
   each sibling's rows appear with the *other* siblings' columns NULL. This is the
   surprising-but-required PG default (`PLAN DEFAULT (... UNION ...)`); it is the single
   trickiest semantic to get right and is oracle-pinned.
5. Ordinality numbering follows the outer-to-inner traversal order.

**T1 implements the default plan only.** An explicit `PLAN` / `PLAN DEFAULT` clause (which
reorders/overrides the outer/union choices) is the least-precedented, rarely-used sub-feature
and is **rejected `0A000` in T1**, landing as the deferred **T2** slice. Capability
`func.json_table` (T1) / `func.json_table_plan` (T2).

---

## 4. Worked example

```sql
SELECT jt.*
FROM JSON_TABLE(
  '{"id": 7, "items": [{"sku":"a","qty":2}, {"sku":"b","qty":5}]}',
  '$' COLUMNS (
    id        i32                    PATH '$.id',
    n         FOR ORDINALITY,
    NESTED PATH '$.items[*]' COLUMNS (
      sku     text                   PATH '$.sku',
      qty     i32                    PATH '$.qty'
    )
  )
) AS jt
-- → (7, 1, 'a', 2), (7, 2, 'b', 5)
```

---

## 5. Delivery — vertical slices

After C0 ([§1](#1-the-shared-column-definition-list-facility-c0--keystone)), the `jsonb`
foundation ([json.md §12](json.md)), `jsonpath` ([jsonpath.md §9](jsonpath.md)), and
`JSON_VALUE`/`JSON_EXISTS` (S2, [json-sql-functions.md §7](json-sql-functions.md)):

- **C0** — the shared col-def-list facility + multi-column synthetic table (§1). Keystone;
  unblocks R-series, `json[b]_each`, and T-series. Capability `func.coldeflist`.
- **R1** — `json[b]_to_record` / `json[b]_to_recordset` (§2). Capability `func.json_record`.
- **R2** — `json[b]_populate_record` / `json[b]_populate_recordset` (§2). Capability
  `func.json_populate`.
- **T1** — `JSON_TABLE` with the default plan (§3): the `JsonTablePlan` row source, COLUMNS
  via C0, regular/ordinality/exists columns, `NESTED PATH` with default LEFT-OUTER/UNION
  expansion; explicit `PLAN` → `0A000`. Capability `func.json_table`.
- **T2** (deferred) — the explicit `PLAN` clause. Capability `func.json_table_plan`.

**Riskiest, least-precedented work in the whole JSON feature:** T1's nested-path
LEFT-OUTER/UNION expansion (§3.3) — a new planner node, recursive expansion, the sibling-
union semantics, the C0 dependency, and the per-row `JSON_VALUE`/`JSON_EXISTS` evaluation all
compose here. It ships last and is oracle-pinned hardest.
