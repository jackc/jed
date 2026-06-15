# spec/functions/ — function / operator catalog, as data

The function and operator catalog (CLAUDE.md §5): [catalog.toml](catalog.toml) names each
operator, its operand contract, result type, and NULL behavior — **as data**, authored
once. It is the first surface on the **codegen middle path**: a build-time generator emits
per-language operator descriptor tables from it (`impl/{rust/src,go,ts/src}/operators.{rs,go,ts}`,
via `rake codegen`) rather than hand-writing N times — see
[../design/codegen.md](../design/codegen.md). It is **descriptive of the implemented
operators**, not aspirational, and grows one entry per feature. The *why* — the schema,
truth-value result types, NULL propagation vs detection — lives in
[../design/functions.md](../design/functions.md).

Operator *result types* (e.g. the type of `int32 + int32`) live here, not in
[../types/](../types/): `types/` defines the scalars and how they compare/promote;
`functions/` defines what operators do with them, **referencing** `types/` rather than
restating it.

## Covered entries

The catalog has grown well past the original operator set. Authored kinds:

| Kind | Members | Result | NULL |
|---|---|---|---|
| `logical` | `AND` `OR` `NOT` | `boolean` | `kleene` (AND/OR), `propagates` (NOT) |
| `comparison` | `=` `<` `>` `<=` `>=`, `IS [NOT] DISTINCT FROM` | `boolean` | propagates (null-safe for `IS [NOT] DISTINCT FROM`) |
| `null_test` | `IS NULL`, `IS NOT NULL` | `boolean` (always definite) | detects |
| `arithmetic` | `+` `-` `*` `/` `%`, unary `-` (integer / decimal / float / interval / timestamp families) | `promoted` | propagates |
| `function` (scalar) | `abs` `round`, `make_interval`, `uuid_extract_version`/`uuid_extract_timestamp`, `uuidv4`/`uuidv7`, `now`/`clock_timestamp` | per-function (§9–§12) | propagates / volatile-on-seam |
| `aggregate` | `count` `sum` `min` `max` `avg` | per-PG widening | skip-NULL |
| `set_returning` | `generate_series` | a row **set** | any NULL arg → 0 rows |

> Status: covers the operators, scalar functions, aggregates, and set-returning functions
> the three cores implement today — 28 scalar `function`, 10 `aggregate`, and 2
> `set_returning` entries alongside the operator kinds (`<>`/`!=` deliberately do not
> exist — only `=`). The `precedence` and `cost` fields are authored, and
> `IS [NOT] DISTINCT FROM` plus the named / `DEFAULT`-argument functions have landed.
> Coherence is checked by [verify.rb](verify.rb) (`rake verify`).
