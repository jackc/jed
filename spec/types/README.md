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
| [datetime_units.toml](datetime_units.toml) | PostgreSQL's datetime unit-name tables (`deltatktbl`, `datetktbl`): the unit names and aliases `EXTRACT` / `date_part` / `date_trunc` accept. Codegen'd to the per-core `DELTA_TOKENS` / `DATE_TOKENS` tables; see [../design/timezones.md](../design/timezones.md) §9.6. |
| [formatting.toml](formatting.toml) | PostgreSQL's data-type formatting vocabulary (`DCH_keywords`, `DCH_suff`, `NUM_keywords`, the month / day / Roman-numeral / AM-PM / AD-BC / ordinal word lists) for `to_char` / `to_timestamp` / `to_date` / `to_number`. Codegen'd to the per-core `formatting_gen` tables; see [../design/formatting.md](../design/formatting.md). |
| [ranges.toml](ranges.toml) | The six built-in range types (a structural range over a scalar element): id, element subtype, aliases, discreteness. Codegen'd to the per-core `RANGES` table; see [../design/ranges.md](../design/ranges.md). |

## Current scope — the storable scalar set

Per CLAUDE.md §4, the **storable** scalar types are the three signed integers
(`i16`/`smallint`, `i32`/`int`/`integer`, `i64`/`bigint`), `text` (variable-width
UTF-8, collation `C`), `boolean` (aliases `bool`; `{false, true}`, ordered false `<` true),
`decimal`/`numeric` (exact base-10), `bytea` (raw bytes), `uuid` (fixed 16-byte value), the
temporal types `timestamp`/`timestamptz`/`interval`, and the binary floats `f32`/`real`
and `f64`/`double precision`. Only `json`/`jsonb` (and the composite `array` container)
remain deferred. `boolean` is also the type of comparison/logical results and `TRUE`/`FALSE`
literals.

The CLAUDE.md §8 divergence decisions have **landed and bind**: the collation is PostgreSQL
`C`; `decimal` rounds **half away from zero** ([../design/decimal.md](../design/decimal.md));
and the binary floats carry their own PostgreSQL total order plus the `R` render-tolerance tag
([../design/float.md](../design/float.md), ledgered in
[../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml)).
Per-type narrowings — which non-integer types may be a `PRIMARY KEY` (`uuid`, `timestamp`,
`timestamptz` may; the rest are non-key for now), and which casts exist — are enumerated in
[../design/types.md](../design/types.md).
