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

## Current scope — signed integers only

Per CLAUDE.md §4, **step 1 implements only** `int16`/`smallint`, `int32`/`int`/`integer`,
and `int64`/`bigint`. `decimal`, `text`, `boolean`, `timestamp`/`timestamptz`, `bytea`,
and `json`/`jsonb` are deferred to later slices, and the float/decimal/collation
divergence decisions in CLAUDE.md §8 do **not** bind this step.
