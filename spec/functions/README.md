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

## Covered operators

| Kind | Operators | Result | NULL |
|---|---|---|---|
| `logical` | `AND` `OR` `NOT` | `boolean` | `kleene` (AND/OR), `propagates` (NOT) |
| `comparison` | `=` `<` `>` `<=` `>=` | `boolean` | propagates |
| `null_test` | `IS NULL`, `IS NOT NULL` | `boolean` (always definite) | detects |
| `arithmetic` | `+` `-` `*` `/` `%`, unary `-` | `promoted` | propagates |

> Status: covers the comparison/null-test/arithmetic/logical operators the cores
> implement today (`<>`/`!=` do not exist). The `precedence` field is now authored;
> `cost` and the deferred surfaces — `IS [NOT] DISTINCT FROM`, named `function` entries —
> are added here *first* as their features land. Coherence is checked by
> [verify.rb](verify.rb) (`rake verify`).
