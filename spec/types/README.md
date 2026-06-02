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

## Current scope — signed integers (storable) + boolean (expression-only)

Per CLAUDE.md §4, the **storable** scalar types are `int16`/`smallint`,
`int32`/`int`/`integer`, and `int64`/`bigint`. The general-expression slice adds
`boolean` (aliases `bool`) as the first non-integer scalar, but **expression-only**
(`storable = false`): it is the type of comparison/logical results and `TRUE`/`FALSE`
literals, not yet a column type (`CREATE TABLE … boolean` / `CAST … AS boolean` trap
`0A000`). `decimal`, `text`, `timestamp`/`timestamptz`, `bytea`, and `json`/`jsonb`
remain deferred, and the float/decimal/collation divergence decisions in CLAUDE.md §8
still do **not** bind.
