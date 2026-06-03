# spec/types/ — the type system, as data

The type system is **the product** (CLAUDE.md §4). It is designed on paper, as data,
*before* the executor — it is the spec everything else tests against, not a detail
discovered during implementation.

The reasoning behind these tables lives in
[../design/types.md](../design/types.md). **Read that first.**

## Files

| File | Contents |
|---|---|
| [scalars.toml](scalars.toml) | Scalar type definitions: id, aliases, width, range, overflow behavior, key-encoding rule, promotion rank. |
| [compare.toml](compare.toml) | Comparison & promotion: comparability classes, the numeric promotion tower, three-valued NULL logic. |
| [casts.toml](casts.toml) | Coercion matrix: which casts exist and their mode (implicit / assignment / explicit). Anything unlisted is forbidden. |

## Current scope — signed integers + text + boolean (all storable)

Per CLAUDE.md §4, the **storable** scalar types are `int16`/`smallint`,
`int32`/`int`/`integer`, `int64`/`bigint`, `text` (variable-width UTF-8, collation `C`),
and `boolean` (aliases `bool`; `{false, true}`, ordered false `<` true). `boolean` is also
the type of comparison/logical results and `TRUE`/`FALSE` literals. Two boolean narrowings
remain (each relaxable, mirroring text): a boolean `PRIMARY KEY` is rejected `0A000`, and
`CAST … AS boolean` / boolean⇄integer casts are deferred (`0A000` / `42804`) — see
[../design/types.md](../design/types.md) §9. `decimal`, `timestamp`/`timestamptz`, `bytea`,
and `json`/`jsonb` remain deferred, and the float/decimal divergence decisions in CLAUDE.md §8
still do **not** bind (the collation decision landed: PostgreSQL `C`).
