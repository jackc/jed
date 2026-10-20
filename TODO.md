# Roadmap / TODO

> Working backlog for the engine, **grouped into sections by related area** — not a
> sequence and not a critical path. This is a living file — re-rank freely; items marked
> _(parallel)_ can proceed independently.
>
> **The live backlog is every open `- [ ]` line.** `grep '\- \[ \]' TODO.md` is the
> fastest way to find real work. A completed item is **deleted once it has no open
> follow-on** — its full design, the *why*, the error codes, the golden-fixture names,
> and the divergence ledgers live in `spec/design/*` and git history, **not here**. A
> done `[x]` item survives only to give an open _follow-on:_ beneath it context; size
> tags `_(size: …)_` are kept on open items only.
>
> Read [CLAUDE.md](CLAUDE.md) first — it is the load-bearing design record. Section
> references below (§N) point into it.

## Definition of done (applies to every feature item)

A feature is a **vertical slice** (CLAUDE.md §10), and "done" means **all** of:

1. **Spec first** — the canonical artifact is updated: grammar (`spec/grammar/`), type
   data (`spec/types/`), operator/function catalog (`spec/functions/`), error registry
   (`spec/errors/`), and/or design doc (`spec/design/`) — *before* the executor.
2. **All native cores in lockstep** — Rust, Go, **and** TS (§2). No core leads the spec.
3. **Conformance corpus** — new `.test` entries + a `# requires:` capability (and, where
   it's a milestone, a profile) in `spec/conformance/manifest.toml`. The corpus is the
   contract (§7), not an afterthought.
4. **Determinism** — defined ordering, structured error codes, no float/iteration-order
   leakage (§8, §10).
5. **PostgreSQL behavior by default** — where the feature has a choice and one option matches
   PostgreSQL, take it unless there's a documented overriding reason (CLAUDE.md §1). Any
   deliberate divergence from PG is recorded in the relevant spec doc.

Difficulty key: **S** ≈ hours · **M** ≈ a day · **L** ≈ multi-day · **XL** ≈ a project.

---

## Core query / DML completeness

- [x] **Predicate forms — `IN`/`BETWEEN`/`LIKE`/`CASE`** — plus `ILIKE`, `LIKE`/`ILIKE … ESCAPE e` (desugared to PostgreSQL's `like_escape(p, e)`), and the regex operators `~`/`~*`/`!~`/`!~*` + `regexp_replace`/`regexp_match` (a hand-written linear-time Pike VM, ReDoS-immune). → grammar.md §20–§23, [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ `SIMILAR TO` (deliberately excluded — the SQL-standard surface); Unicode-property char classes (`\p{…}`).
- [x] **Left-to-right short-circuit `AND` / `OR`** — a FALSE left operand decides `AND` and a TRUE
  one decides `OR`; the right operand is then neither evaluated nor charged, so the guard idiom
  `d <> 0 AND 10 / d > 1` no longer traps `22012` (PostgreSQL returns the guarded rows). In a filter
  (`WHERE`, `ON`, `HAVING`, a partial index's `WHERE`, `ON CONFLICT … WHERE`) the top-level `AND`
  chain also stops at a NULL conjunct — PostgreSQL's qual-list rule — while value contexts (select
  list, `CASE`, `CHECK`, aggregate `FILTER`) continue past NULL, as in PostgreSQL. The node still
  charges its `operator_eval`; estimates count every node (an upper bound), so plans are unchanged.
  A fixed order is a stronger promise than PostgreSQL's unspecified one. →
  [cost.md §3](spec/design/cost.md), `expr/short_circuit.test`
  - [ ] _follow-on:_ PostgreSQL cost-sorts a filter's conjuncts, so a guard written *second*
    (`WHERE 10 / d > 1 AND d <> 0`) can succeed there and traps here; jed keeps source order.
- [x] **Scalar functions `abs` / `round`** — first named per-row functions. → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ a general implicit argument-coercion pass. (`ceil`/`floor`/`mod`/`sign` and text `length`/`lower`/`upper` have since landed in their own slices.)
- [x] **Scalar string / text functions** — PG's string surface as built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/`repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/`concat`/`concat_ws`/`format`/…). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing.
- [x] **Double-quoted identifiers + PostgreSQL identifier folding** — bare identifiers fold to lowercase, `"…"` names are exact (any UTF-8, `""` escape, `""` → `42601`); every name comparison, catalog key, and canonical name order is byte-exact; host-API names are verbatim; `nextval('…')` text parses as an identifier (`42602`); auto-names derive from exact names; the `jed_` reservation is byte-exact; `format_version` 37. → [grammar.md §3](spec/design/grammar.md)
  - [x] _follow-on:_ a derived CHECK / FK / UNIQUE / EXCLUDE name's collision walk avoids every constraint name in its schema, as PostgreSQL's `ChooseConstraintName` does — main, the session's temp tables, and each attachment are separate schemas. → [constraints.md §4.3](spec/design/constraints.md)
- [x] **Named + optional (DEFAULT) function arguments** — PG named notation `f(name => value)` + DEFAULT params; `make_interval`, then `make_timestamp`/`make_timestamptz`; `VARIADIC` (landed as **AF6** with the array type). → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ general non-integer DEFAULT values (no consumer yet — built-ins use overloads or `make_interval`-style 0-defaults); user-defined-function defaults (jed has no UDFs).
- [x] **Common aggregates** — PostgreSQL's everyday aggregates, each with its spill/query-memory
  accounting: `bool_and`/`bool_or`/`every`, `bit_and`/`bit_or`/`bit_xor`, `array_agg` (arrays of
  arrays stack a dimension), `string_agg` (text or bytea), the variance/standard-deviation family
  (`var_pop`, `var_samp`, `variance`, `stddev_pop`, `stddev_samp`, `stddev`), and the two-argument
  `covar_pop`, `covar_samp`, `corr`, `regr_count`, `regr_avgx`, `regr_avgy`, `regr_sxx`, `regr_syy`,
  `regr_sxy`, `regr_slope`, `regr_intercept`, `regr_r2`. The variance/stddev family is exact over
  integer/decimal input (as in PostgreSQL) and uses PostgreSQL's `float8` algorithms over floats; the
  two-argument set widens any numeric arguments to `f64`, as PostgreSQL does. → [aggregates.md §23](spec/design/aggregates.md)
- [x] **Everyday scalar functions** — `concat`, `concat_ws`, `format()` (PostgreSQL's `%s`/`%I`/`%L`,
  [string-functions.md §6](spec/design/string-functions.md)), `array_to_string` / `string_to_array`
  ([array-functions.md §15](spec/design/array-functions.md)), the regex `substring`, and the
  `gen_random_uuid` alias of `uuidv4` ([entropy.md](spec/design/entropy.md)). (`to_char` / `to_number`
  have landed — [formatting.md](spec/design/formatting.md).)
- [x] **Named windows** — the `WINDOW w AS (…)` clause + `OVER w`, base-window extension, and the
  shared partition/sort pass, slice S5 of [window.md](spec/design/window.md) §5.3: PostgreSQL's
  whole-clause rules (a duplicate name `42P20`, every entry validated even when unreferenced,
  `OVER name` outside the select list / `ORDER BY` `42P20`, named windows in the query `ORDER BY`),
  NoREC `window_share`, benchmark `window_shared_named`. PostgreSQL's prefix-compatible sort sharing
  stays open (window.md §11).
- [ ] **Revisit `DISTINCT ON`** — recorded out of scope in [grammar.md §11](spec/design/grammar.md), but
  it is one of PostgreSQL's most-used extensions; decide whether to implement it or keep the
  non-goal. _(size: M if implemented)_
- [ ] **`MERGE`** — the SQL-standard (PostgreSQL 15+) `MERGE INTO … USING … ON … WHEN [NOT] MATCHED
  THEN …`, sharing upsert's two-phase write machinery ([upsert.md](spec/design/upsert.md)). _(size: L)_
- [x] **`SAVEPOINT` / nested transactions** — `SAVEPOINT name`, `RELEASE [SAVEPOINT] name`,
  `ROLLBACK [TRANSACTION|WORK] TO [SAVEPOINT] name` with PostgreSQL semantics (name shadowing,
  `25P01` outside a block, `3B001` unknown name, `ROLLBACK TO` recovers a failed block). The open
  transaction carries a stack of snapshot frames, so a rollback-to restores rows, DDL, sequences,
  statistics, temp tables, attachment writes, and pending-write accounting together; SQL-only (no host
  API methods), `0A000` inside `execute_script`; no format change. A nested `BEGIN` stays `25001`.
  → [transactions.md §11](spec/design/transactions.md)
- [ ] **Decide: SQL-language triggers and functions** — SQLite-style triggers whose bodies are plain
  SQL statements, and `CREATE FUNCTION … LANGUAGE sql`. Both stay pure, so neither conflicts with
  CLAUDE.md §13. Record each as a scheduled item or an explicit non-goal. _(size: decision; L each if built)_

---

## The type system as the product (the differentiator, §4)

> `boolean`, `text` (collation `C` → linguistic UCA), `decimal`, `timestamp`/`timestamptz`,
> `date`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the `array`,
> `range`, and composite containers are all done. What remains: per-type cast/function
> follow-ons, the JSON `0A000` follow-ons, and the composite-container narrowings.

- [x] **`text` + collation** — UTF-8 code-point order (type code 4), text PK/index/UNIQUE via `text-terminated-escape`; **linguistic collation** landed end-to-end: jed-owned UCA executor, `COLLATE` / per-column / per-db default, collated keys, the reference-only/vendored-tier pivot (`format_version 18`, real Unicode-17 root + `es`), and the host-loaded `JUCD` Unicode-data bundle (`db.LoadUnicodeData`). → [types.md §11](spec/design/types.md), [collation.md](spec/design/collation.md), [encoding.md §2.4](spec/design/encoding.md)
  - [x] **`varchar(n)` length limits** — a `varchar(n)` / `character varying(n)` / `string(n)` max-length typmod (the 2nd parameterized type), counted in code points; over-length assignment traps `22001` (with PG's trailing-space-truncation exception), explicit `::varchar(n)` cast silently truncates; `1 ≤ n ≤ 10485760` else `22023`; `format_version` 22 (text column/field `u32 varchar_max_len` typmod slot). → [types.md §15](spec/design/types.md)
    - [ ] _follow-on:_ `char(n)`/`character(n)` (blank-padded); `varchar(n)[]` element typmod (the `numeric(p,s)[]` narrowing). _(The regex `substring(text FROM pattern)` has landed ([regex.md §8e](spec/design/regex.md)); the `SIMILAR … ESCAPE` form is excluded with `SIMILAR TO`.)_ _(Runtime non-literal text→T casts, any-scalar → text casts (`x::text` — [types.md §5](spec/design/types.md)), `length`/`lower`/`upper`, `substring(text, …)` with the `SUBSTRING(x FROM s FOR l)` form ([string-functions.md §4](spec/design/string-functions.md)), and text `||` (with PG's `anynonarray || text`, [string-functions.md §5](spec/design/string-functions.md)) have landed.)_
- [x] **Exact `decimal`** — *the* headline type: sign+coefficient+scale, round-half-away (settles §8), PG result scales, finite-only (documented divergence), decimal PK/index/UNIQUE via `decimal-order-preserving`; `round`/`ceil`/`ceiling`/`floor`/`trunc(x[,n])`, `gcd`/`lcm`/`width_bucket`, and the exact-numeric transcendentals `sqrt`/`ln`/`exp`/`log`/`log10`/`log(b,x)`/`power`/`pow`. → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods.
  - [x] _follow-on:_ mixed numeric arguments to the two-argument math functions — `power`/`pow`/`log(b, x)`/`atan2` rows are `arg_resolution = "promote"`, so `power(2.0, 3)` resolves to the numeric kernel and `pow(f, 3)` to the float one (and `width_bucket(f, 0, 10, 5)` adapts its literals). → [functions.md §9.2](spec/design/functions.md)
  - [ ] _follow-on:_ `generate_series(numeric, numeric[, numeric])` — PostgreSQL has a numeric series (`generate_series(1, 3.5)`); jed's SRF is integer-only (`42883`).
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, `±infinity` first-class, timestamp PK; the host-loaded `JTZ` tz database + `AT TIME ZONE`; `date_trunc`/`EXTRACT`/cross-family casts in a zone + an observable session `TimeZone` slot. → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md)
  - [ ] _follow-on:_ a separate `time` type; `current_timestamp(p)` / `localtimestamp(p)`; the multi-word `timestamp [(p)] with[out] time zone` spelling; `timestamp(p)[]` element typmods (the `varchar(n)[]` narrowing); zone abbreviations in `to_timestamp` input (`'… EST'` under `TZ` is `0A000` until the abbreviation-table decision, [timezones.md §15](spec/design/timezones.md), [formatting.md §5](spec/design/formatting.md)). (`timestamp(p)` / `timestamptz(p)` precision typmods — rounding from PostgreSQL's 2000 epoch on assignment and cast, `format_version` 43 ([timestamp.md §8](spec/design/timestamp.md)) — PostgreSQL's data-type formatting functions — `to_char` over datetimes, intervals, and numbers, `to_timestamp` / `to_date` / `to_number`, and `to_timestamp(double precision)` ([formatting.md](spec/design/formatting.md)) — `date_part` (float8, with PG's ±Infinity / NULL over a ±infinity source — [timezones.md §9.2](spec/design/timezones.md)), `EXTRACT(julian …)`, PG's unit aliases for `EXTRACT`/`date_part`/`date_trunc` ([timezones.md §9.6](spec/design/timezones.md)), `age` ([interval.md §5.2](spec/design/interval.md)), the runtime text→`timestamp`/`timestamptz` casts, offset-less `timestamptz` input read in the session zone on every path — literals, assignment, DEFAULT, bound parameters, container text ([timezones.md §9.3](spec/design/timezones.md)) — and session-zone rendering of every `timestamptz` → text — result cells through the host session render, and `timestamptz` nested in `array → text` ([timezones.md §9.5](spec/design/timezones.md)) — have landed.)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, type code 11, interval PK/index/UNIQUE/FK via the 16-byte `interval-span-i128` key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ the `INTERVAL(p) '…'` typed-literal spelling (write `'…'::interval(p)`); `interval[]` element typmods. (Field qualifiers (`YEAR TO MONTH`, `INTERVAL '1' DAY`) + `interval(p)` / `SECOND(p)` on columns, fields, casts, and literals — [interval.md §6.1](spec/design/interval.md), `format_version` 43 — `EXTRACT`/`date_part` of an interval — [timezones.md §9.2](spec/design/timezones.md) — and `age` — [interval.md §5.2](spec/design/interval.md) — have landed.)
- [x] **`bytea`** — variable-width bytes, unsigned order, `\x`-hex and traditional escape-format (`\nnn`) input (`22P02`), type code 7, bytea PK/index/UNIQUE via `bytea-terminated-escape`; the binary functions `length`/`octet_length`/`bit_length`, `||`, `substr`/`substring` (+ the `SUBSTRING(x FROM s FOR l)` special form, text too), `get_byte`/`set_byte`, `get_bit`/`set_bit`, `overlay` and `position` (+ the `OVERLAY(… PLACING …)` / `POSITION(… IN …)` special forms, text too), `btrim`/`ltrim`/`rtrim(bytea, bytea)`, and the digests `md5`/`sha224`/`sha256`/`sha384`/`sha512` (library-backed, CLAUDE.md §14 clause 3); a bare string literal argument is read as bytea input where no text overload applies ([functions.md §9.1](spec/design/functions.md)). → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md), [string-functions.md §4](spec/design/string-functions.md)
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower, the first types narrowly exempted from byte-identity (the `R` tolerant compare + exception ledger), type code 12; **float in a PK/index** via the `float-order-preserving` key; the float math functions. → [float.md](spec/design/float.md), [determinism.md](spec/design/determinism.md)
- [x] **`json` / `jsonb` + SQL/JSON** — the committed XL headline feature: all non-deferred slices across all three cores, oracle-clean; spec'd across [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md); type codes 18/19/20, one `format_version` bump (v18→v19).
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); `JSON_TABLE` explicit `PLAN` (T2 — needs a maintainer decision: PostgreSQL 17/18 do not implement `PLAN` (a syntax error, `42601`), so there is no PostgreSQL behavior to follow); a composite/array column in a json record function's column-definition list, and `json[b]_populate_record(set)` over an anonymous-record base (`NULL::record`) ([json-table.md §1](spec/design/json-table.md)); and a NaN or ±Infinity float `PASSING` value (a `numeric` NaN/Infinity item in PG, which jed's finite `decimal` cannot hold, [json-sql-functions.md §5.4](spec/design/json-sql-functions.md)); and PostgreSQL 16+'s `_` digit separators and `0x`/`0o`/`0b` radix integers in jsonpath *path text* number literals and `.**{level}` bounds (`$ ? (@ == 0x10)`, `{1_000}` — still `42601`; the SQL literals and every text → number input already take them, [jsonpath.md §2](spec/design/jsonpath.md)).
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — COMPLETE (S0–S6): the open `Type { Scalar | Composite(catalog-ref) }`, `CREATE`/`DROP TYPE`, nested + recursive types, storable composite column + recursive codec (`format_version` 9), `ROW(…)`, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY; anonymous `record` / `record[]` derived-relation columns and `unnest(record[]) AS u(a i32, …)` ([composite.md §13](spec/design/composite.md)); array- and range-typed fields (the range field is `format_version` 41). Named composites only. → [composite.md](spec/design/composite.md)
  - [x] `ROW(…)` expressions assigned to a composite column (field-by-field assignment coercion, nested `ROW(ROW(…),…)` included), non-NULL composite `DEFAULT`s (`format_version` 44), and the runtime composite casts (text→composite, `composite::text`, `ROW(…)::type`). → [composite.md §8/§12](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ an **array-of-composite** field as a key + a composite **FK** pairing (the remaining `0A000` key axes).
  - [ ] _follow-on:_ `DROP TYPE` dependencies from stored **expressions** — a column `DEFAULT`, `CHECK`, or expression-index key that names the type only in a cast (`(ROW(1,2)::pt).x`) records no dependency today, so `DROP TYPE pt` succeeds and leaves the expression failing when applied; PostgreSQL refuses (`2BP01`) and its `CASCADE` drops the default. Columns, fields, and views are tracked ([composite.md §7](spec/design/composite.md)).
- [ ] **Enum types** (`CREATE TYPE name AS ENUM (…)`) — PostgreSQL's ordered label sets: a named
  catalog type (the composite precedent), ordered by declaration, keyable, `ALTER TYPE … ADD VALUE`;
  a `format_version` bump. _(size: L)_
- [ ] **Domains** (`CREATE DOMAIN name AS base [DEFAULT …] [NOT NULL] [CHECK (…)]`) — a named,
  constrained base type, enforced at every assignment site and cast; squarely the "type system is
  the product" theme (CLAUDE.md §4). _(size: L)_

---

## Relational depth + constraints

- [x] **Subqueries** — uncorrelated scalar, `[NOT] IN (SELECT …)`, `[NOT] EXISTS`, correlated, subqueries in UPDATE/DELETE and `INSERT ... VALUES` slots, `$N` inside a subquery, derived tables, a `VALUES` body, `LATERAL`, `x op ANY/ALL(SELECT …)`, correlated `ORDER BY` / `GROUP BY` keys, **row-valued** subqueries (`(a, b) op | IN | op ANY/ALL (SELECT x, y …)`, with the bare `(a, b)` row constructor). → [grammar.md §26/§42/§44](spec/design/grammar.md)
  - [x] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …) [AS g]`) — unaliased groups keep their inner labels visible (chain-leading groups splice into the chain; right-hand groups plan as inline derived bodies, behind a lateral fence when a `LATERAL` item inside reaches the relations to their left), aliased groups are `(SELECT * FROM <join>) AS g`. → [grammar.md §15](spec/design/grammar.md)
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — precedence tree (INTERSECT binds tighter), full per-column type unification, NULL-safe multiset semantics, trailing ORDER BY by name/ordinal, parenthesized operands/queries with their own ORDER BY/LIMIT. → [grammar.md §25](spec/design/grammar.md)
- [x] **Common table expressions (`WITH`)** — named derived tables (PG hybrid inline/materialize), `WITH RECURSIVE`, data-modifying (writable) CTEs, nested `WITH`. → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [x] _follow-on:_ a recursive CTE's `SEARCH` / `CYCLE` clauses (PostgreSQL's depth/breadth ordering and cycle-detection columns, on `record` / `record[]` working-table columns). → [recursive-cte.md §9](spec/design/recursive-cte.md)
  - [ ] _follow-on:_ the remaining recursive-CTE deferrals (a set-op / `FROM`-subquery recursive term, mutual recursion). → [recursive-cte.md §8](spec/design/recursive-cte.md)
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, comparable-type pairing (integer ↔ integer, integer → `decimal`, `f32` ↔ `f64`; other pairs `42804`), MATCH SIMPLE and MATCH FULL (a partly-NULL tuple is `23503`; `format_version` 40's match bit, `jed_constraints.match_type`), enforced at four write sites (`23503`), plus recursive `ON DELETE`/`ON UPDATE` `CASCADE | SET NULL | SET DEFAULT` action closures (`format_version` 30's three-bit action fields). → [constraints.md §6](spec/design/constraints.md)
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — on-disk B-trees with equality/range and multi-column-prefix bounds, expression and partial keys, bounded mutation scans, and LIMIT streaming; per-key `ASC`/`DESC` + `NULLS FIRST|LAST` (the ordered slot — encoding.md §2.16; forward walks serve matching `ORDER BY`s); `format_version` 5/26/27/46. → [indexes.md](spec/design/indexes.md)
  Variable-width (text/decimal/bytea/interval, C or collated) range and trailing key elements are
  skipped by their self-delimiting encodings (indexes.md §5.1). Partial indexes are used whenever the
  WHERE implies their predicate under a bounded `predtest.c`-subset prover (§9.1), including a full
  partial-index scan with no access predicate and its ORDER BY walk (§9.2), OR/IN sets, and the
  index-nested-loop inner (§9.3); `ON CONFLICT (cols) WHERE pred` infers a partial unique arbiter.
  - [ ] _follow-on (each its own slice + NoREC obligation):_ a **backward** index walk serving a
    `DESC` order from an `ASC` index (needs a unique NULL-free key, or a reversed tie-break); a
    container (range/array/composite) element after the equality prefix (needs a container slot
    skip); a full partial-index scan inside a join (sound for a NULL-rejecting predicate,
    indexes.md §9.2); prover extensions — refutation, cross-family constants, `NOT`, and a bound
    `$N`'s value (would make implication parameter-sensitive, estimator.md §6.1). (`jsonb` and
    composite keys are tracked separately above.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over every scalar key element type (incl. `text[]`, each element's `C` key cut at 128 bytes, `bytea[]` and `decimal[]`, each key cut at 128 bytes — a decimal's key is value-based, so `1.5 = 1.50` is term-exact and an integer `= ANY(decimal[])` converts, `f32[]`/`f64[]`, whose canonicalizing float key makes `-0 = +0` and `NaN = NaN` term-exact, and `interval[]`, whose span key makes `1 mon = 30 days` term-exact) — plus the **`jsonb_ops`** opclass over a `jsonb` column (PostgreSQL's key/value terms, flag byte + terminated body cut at 128 bytes) accelerating `@>`/`?`/`?|`/`?&`, lossy and always rechecked; an ALL-mode **skip-scan** intersection reads only the shortest posting list whole and probes the others leaf by leaf at the surviving candidates (gin.md §6.2). → [gin.md](spec/design/gin.md)
  Bounded LIMIT streaming completes the posting gather, then stops candidate table lookups/filtering at the window.
  Since landed: `IN` lists and `OR`s of GIN arms on one index (gin.md §6.3); multi-column GIN (`format_version` 47, §4.4); the `jsonb_ops` jsonpath strategies `@?`/`@@` (§2.2); posting-list run compression (`format_version` 47, §4.5 — 64-byte front-coded runs, about 5× fewer index pages).
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows); a `BitmapOr` across different indexes; the **remaining** element types — composite- and `jsonb`-element arrays (each needs a non-scalar term rule); one combined gather over conjuncts on several columns of a multi-column GIN; correlated / array-column query operands; a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. The partial `EXCLUDE … WHERE (predicate)` form, `EXCLUDE USING btree (a WITH =)` (btree is the default method; a non-unique ordered backing index, `23P01`), and `ON CONFLICT` arbitration of exclusion constraints have landed (`format_version` 45). → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape; a GiST predicate-implication gate so a partial GiST index serves reads (and `CREATE INDEX … USING gist … WHERE`); `EXCLUDE` on a temporary table (`0A000` for either method).
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [x] `ON CONFLICT (cols) WHERE index_predicate` partial unique-index arbiter inference.
  - [x] the `COLLATE` / operator-class inference decorations (PostgreSQL's matching rules over jed's frozen column collations and default B-tree classes, `btree_opclass` in scalars.toml); `DO UPDATE SET pk_col = …` re-keying the conflicting row (end-state validation, index + FK maintenance), with PostgreSQL's second-affect rule over the phase-1 overlay. → [upsert.md §2–§5](spec/design/upsert.md)
  - [ ] _follow-on:_ the `WHERE index_predicate` through an updatable view (`0A000`); several partial
    arbiters at once (PostgreSQL arbitrates every inferable index); expression inference elements
    (`ON CONFLICT (lower(email))`). → [upsert.md §10](spec/design/upsert.md)
- [x] **`ALTER TABLE`** — catalog-only rename/default/nullability/constraint edits plus the
  `ADD`/`DROP COLUMN`, `ALTER COLUMN … TYPE … [USING]`, and `ADD`/`DROP PRIMARY KEY` rewrite slices
  have landed, as has identity management (`ALTER COLUMN … ADD GENERATED … AS IDENTITY`, `SET GENERATED`,
  `SET <sequence_option>` / `RESTART`, `DROP IDENTITY`, the identity-only `SEQUENCE NAME` option,
  `jed_columns.identity_generation`, identity columns in the CLI dump). The rest of PG's menu
  (OWNER/RLS/triggers/partitions/tablespaces/schemas/storage knobs) is deliberately out of scope,
  `0A000` (alter.md §6). → [alter.md](spec/design/alter.md)
  - [ ] _follow-on:_ the CLI dump still writes neither standalone sequences nor a `serial` column's
    owned sequence (a replayed `serial` default names a sequence that does not exist); identity columns
    are dumped (cli.md §3).
- [ ] **Generated columns** (`GENERATED ALWAYS AS (expr) STORED`) — PostgreSQL and SQLite both have
  them; jed has only identity columns. Stored text binds like a `CHECK`: persisted canonical token
  text, the [index-dependencies.md](spec/design/index-dependencies.md) purity contract, host-function
  dependencies as for `CHECK` (extensibility.md §7.1), and an entry in the
  [compatibility.md](spec/design/compatibility.md) manifest if that lands. _(size: L)_
- [x] **Temporary tables** — session-local temp relations with zero writes to the database file,
  each domain a per-session `MemoryBlockStore` + pager with within-session compaction; CREATE/DROP
  INDEX, serial/IDENTITY, composite columns, FOREIGN KEYs among temp tables (`42P16` across kinds);
  `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }`;
  `allow_temp_ddl`; the `max_temp_storage_bytes` `54P03` ceiling over the domain's page bytes. **Spill-to-disk (slice 3) landed:** past `temp_buffers` (the
  resident budget) a file host migrates the domain into an unlinked scratch file in its spill directory
  with a page cache bounded by `temp_buffers` — residency only, results/cost/`54P03`-neutral, run over
  the whole temp corpus by `mise run conformance:spill`. (The database-wide `SHARED` kind was removed
  in favor of in-memory attachments — [attached-databases.md §6](spec/design/attached-databases.md).)
  → [temp-tables.md](spec/design/temp-tables.md)
  - [ ] _follow-on:_ a host-configurable spill directory (shared with the external sort) so WASI/OPFS
    hosts can offer a temp spill target; migrating a shrunken spilled domain back into RAM.
  - [x] _follow-on:_ temporary views — landed with the view follow-ons below. → [views.md §10](spec/design/views.md)
- [x] **`CREATE TABLE … AS`** — `CREATE [TEMP] TABLE [IF NOT EXISTS] name [(cols)] [ON COMMIT …] AS
  query [WITH [NO] DATA]`, PostgreSQL's: columns typed from the query with the typmods its outputs
  keep (`exprTypmod`, now tracked through every query output), filled as `INSERT … SELECT` (which now
  accepts a same-typed array/range/composite source). **Follow-ons landed:** a data-modifying `WITH`
  in the query (run as the statement's writable `WITH`), `SELECT … INTO [TEMP] [TABLE] name` with
  PostgreSQL's placement rules, `EXPLAIN [ANALYZE] CREATE TABLE … AS`, and view columns reporting
  their bodies' typmods (`jed_columns`, `CREATE OR REPLACE VIEW`'s type comparison).
  → [create-table-as.md](spec/design/create-table-as.md)
  - [ ] _follow-on:_ array element typmods (with array columns that declare them).
    → [create-table-as.md §5](spec/design/create-table-as.md)
- [x] **Views** (`CREATE [OR REPLACE] VIEW` / `ALTER VIEW … RENAME` / `DROP VIEW`, writes through
  updatable views) — a named persisted query read as the equivalent derived table (identical rows,
  cost, plan);
  canonical token text with `*` frozen at
  CREATE, persisted table/column/view dependencies (`2BP01`, `0A000` for a referenced column's type
  change), `RENAME` text rewrites through base-table column alias lists, re-validation after `ALTER
  TABLE`, `SELECT`-on-the-view privileges with the owner check at CREATE, `55000` on writes, the
  `jed_views` catalog relation, and the CLI dump; `format_version` 38. Slices V1 (create/read/drop),
  V2 (`RENAME TO` rewrite), V3 (base-table column alias lists), V4 (`RENAME`/`DROP COLUMN` rewrites),
  V5 (`CREATE OR REPLACE VIEW`: PG's column-compatibility rule `42P16`, a `42P17` cycle check, and
  re-validation of every reader with an appended-column capture check), and V6 (`ALTER VIEW … RENAME
  TO` / `RENAME [COLUMN]` and the same `ALTER TABLE` forms on a view, rewriting dependents) all
  landed in every core. V7 (automatically updatable views: `INSERT`/`UPDATE`/`DELETE` through a view
  of PostgreSQL's simple single-relation shape, chains, view `WHERE`s AND-ed ahead of the
  statement's, `RETURNING`, writable CTEs, privileges checked on the view as for a table, and the
  `view_write` NoREC relation) landed in every core too, as did V8 (`DROP TABLE` / `DROP VIEW` /
  `ALTER TABLE … DROP COLUMN` `CASCADE` dropping the closure of dependent views) and V9
  (`WITH [CASCADED | LOCAL] CHECK OPTION`: `44000` on an `INSERT`/`UPDATE` whose new row a checked
  view would not show, `format_version` 39, `jed_views.check_option`), and V10 (`INSERT … ON
  CONFLICT` through an updatable view, written in the view's columns). V11–V16 landed too: `CREATE
  RECURSIVE VIEW` (§17), temporary views and views that become temporary by reading a temp relation
  (§10), view column defaults and `ALTER VIEW … SET / RESET` options (`check_option`,
  `security_barrier`, `security_invoker` — §16), dependencies on sequences and composite types with
  `DROP SEQUENCE / DROP TYPE … CASCADE` (collations and host functions deliberately record none —
  §6.2), and catalog entries longer than a page (`format_version` 48).
  → [views.md](spec/design/views.md)
  - [ ] _follow-on:_ materialized views, views in or over attached databases. (View
    columns in `jed_columns` have landed — [introspection.md §5](spec/design/introspection.md).) → [views.md §12](spec/design/views.md)

---

## Query planner / optimizer

> The planner is a **deterministic cost-based rule engine**. A one-table SELECT inventories every
> legal access/order pipeline; eligible INNER/CROSS base-table islands jointly choose physical
> relation order, access paths, and nested-loop / index-nested-loop / hash algorithms through a
> bounded left-deep search. Exact row counts and explicit deterministic column statistics are
> transactional and persisted; `EXPLAIN` makes the selected plan and its estimates
> corpus-assertable. UPDATE/DELETE targets use the same costed access choice; hard-fenced join shapes
> retain their documented fixed policies.
> **The load-bearing constraint:** cost is **observable and a cross-core contract** (§8; the
> `# cost:` corpus directive), so (a) any plan change that changes which plan runs changes the metered
> cost — it must recompute *identically* in all three cores and re-pin the affected `# cost:` entries;
> (b) the estimator is itself a spec'd, deterministic, cross-core-identical artifact, so cost-based
> plan choice *extends* the §8 contract rather than breaking it; (c) some textbook rewrites
> (constant folding, CSE) are **not** cost-neutral here — they drop `operator_eval`
> charges — so each needs an explicit cost
> decision, not a silent apply. Every optimization is a vertical slice carrying a **NoREC relation**
> (the standing §7 obligation — the sweep does not discover new optimizations).

### Cost-based planner follow-ons

- [x] **Runtime query-compilation investigation (Rust/Go/TS)** — reviewed the Ruby compiler
  and all-core VM experiments; measured isolated TS tree/closure/generated-function kernels,
  confirmed V8 optimization, and probed Node/Chromium code-generation restrictions.
  [Investigation](spec/design/query-compilation-investigation.md). Research only; no production
  rollout. Independent proposals share an
  [experiment contract](spec/design/execution-specialization-experiments.md); each can proceed
  and reach its own decision without another core's implementation:
  - [ ] **Rust cached typed programs** — selective scan execution, plan-owned reuse and
    per-execution scratch; static-kernel controls, no native JIT dependency.
    [Proposal](spec/design/rust-expression-specialization.md).
  - [ ] **Go static execution kernels** — bounded typed patterns, allocation/call reduction
    and conditional row-loop fusion; pure Go, no runtime compiler.
    [Proposal](spec/design/go-execution-specialization.md).
  - [ ] **TS CSP-compatible specialization** — typed closures and static kernels, validated
    plan reuse, strict-CSP browser and ordinary Node execution; no runtime source generation.
    [Proposal](spec/design/ts-expression-specialization.md).
  All are proposed investigations, not production adoption; each requires forced-path
  conformance coverage, whole-query controls and a separate outcome report.

- [x] **Cost as a plan input (Path B)** — landed end-to-end: exact transactional row counts,
  relation-scoped prepared-plan validity, complete candidate inventory, whole-plan estimates and
  EXPLAIN columns, costed access/order pipelines, bounded deterministic N-way join search, and
  transactional v29 `ANALYZE` statistics, plus v42 GIN term statistics (the most common array /
  `jsonb` GIN terms, pricing GIN predicates and candidates). The canonical contracts are
  [estimator.md](spec/design/estimator.md), [statistics.md](spec/design/statistics.md),
  [planner.md](spec/design/planner.md), and [explain.md](spec/design/explain.md).
  - [x] **Parameter-sensitive (custom) plans** — landed in all three cores
    ([estimator.md §6.1](spec/design/estimator.md)): every execution is planned with its bound
    values as plan-time literals (probe, then custom only when the probe read a value), so MCV,
    histogram, GIN term, contradiction, and `generate_series` facts reach parameterized queries.
    A value-blind statement (key lookups) keeps its single cached plan; a sensitive one plans per
    execution with a bounded memo of custom plans by exact binding. EXPLAIN accepts bound values. No
    PostgreSQL-style switch to a generic plan after five executions.
    - [ ] _follow-on:_ subqueries in a top-level plain `INSERT ... VALUES` slot still plan unbound
      (the fast path opens no boundary); custom-plan them if a workload needs it.
  - [x] **`key = ANY(array)` point sets** — a literal, cast, or bound array (`id = ANY($1)`) over a
    single-column PK or leading index key is one interval-set disjunct, a point per element at
    execution; the estimator reads the elements (a bound array in its custom plan). A promoted
    integer/decimal element type (`i32[]` against an `i64` key, `decimal[]` against an `i32` key)
    converts each element to the key's type, dropping one no key can equal. →
    [cost.md §3](spec/design/cost.md) "OR / IN-list", [estimator.md §7](spec/design/estimator.md)
    - [x] _follow-on:_ an uncorrelated `IN (SELECT …)` / `= ANY(SELECT …)` (its folded value list),
      an ANY under an AND leaf of an OR, the `<> ALL` complement estimate, and the residual's
      per-element `operator_eval` for an array of known length.
    - [x] _follow-on:_ an index-nested-loop sibling array column (`o.id = ANY(l.ids)`) — an inner
      PK / index interval set or tuple set with a sibling source. → [cost.md §3](spec/design/cost.md)
      "index-nested-loop"
    - [x] _follow-on:_ a bench param generator that binds a real array (`int_array`; the
      `any_array_pk_param` / `composite_pk_any_param` benches bind one `bigint[]`).
    - [ ] _follow-on:_ the shapes that still full-scan — a promoted `ARRAY[...]` constructor
      (`ARRAY[1::i32, 3::i32]` against an `i64` key) and promoted floats (`f32[]` against an `f64`
      key). _(size: S)_
    - [ ] _follow-on:_ an uncorrelated subquery inside a derived-table body is evaluated with the
      body (per row) instead of folded before its scan opens, so its `IN` point set widens to every
      key there. _(size: S)_
  - [x] **Composite-PK tuple interval sets** — IN / OR / `= ANY` on the members of a composite
    primary key (`tenant = $1 AND id = ANY($2)`, `a IN (1, 2) AND b IN (3, 4)`, `a IN (…) AND b > 5`)
    probe the cartesian product of the point members' keys, with a final range member mapped under
    each prefix, capped at `MAX_KEY_SET_PROBES` combinations. →
    [cost.md §3](spec/design/cost.md) "Composite primary keys", [estimator.md §8.1](spec/design/estimator.md)
    - [x] _follow-on:_ the same tuple set over a multi-column secondary B-tree index (an index on
      `(a, b)` probes `(a, b)` prefixes for `a = 1 AND b = ANY($1)`; key columns from the first later
      set or range onward stay fixed-width for row-key suffix recovery). → [cost.md §3](spec/design/cost.md)
      "Multi-column secondary indexes"
    - [x] _follow-on:_ an index-nested-loop sibling column as a tuple-set member's point source
      (PK and index): `b.tenant = a.tenant AND b.id IN (1, 2)` probes the outer row's tuples.
    - [x] _follow-on:_ a variable-width index column at or after the first later set or range — an
      admitted entry steps over any scalar slot (a `text` of any collation, `bytea`, `decimal`, or
      `interval`) by its self-delimiting slot rule (indexes.md §5.1), so such a column no longer
      leaves only the plain index bound. The plain index bound and the index-nested-loop bound use
      the same rule.
  - [ ] _follow-on:_ statistics quality — pattern selectivity, extended/multi-column correlation,
    configurable targets, MCV-aware join skew, automatic analyze, distribution facts for
    composite/array/json/jsonb, GIN term statistics for `<@`, and an element-count
    histogram (PostgreSQL's `elem_count_histogram`) for array `=`.
  - [ ] _follow-on:_ broader physical search — costed outer/barrier shapes, bushy trees, parallel
    plan search, or adaptive runtime re-planning; each requires a bounded deterministic contract.
- [ ] **Parallel execution within a query** — fan one query's scans, joins, aggregation, and sorts
  out across threads over the pinned snapshot (DuckDB does this; CLAUDE.md §2 cites Rayon-grade
  parallelism as a reason to wrap Rust). Results, cost, and errors must stay identical to serial
  execution: `ORDER BY` output and the error raised must not depend on scheduling, and order-sensitive
  aggregates follow [determinism.md](spec/design/determinism.md). Needs the thread-count `CreateOptions`
  knob (below) and a design doc first. _(size: XL)_

### Planner infrastructure

- [x] **Predicate pushdown + contradiction detection** — the planner's first stage-2 rewrites
  ([planner.md §3](spec/design/planner.md)), all three cores, each with its explicit cost decision.
  **Contradiction** (`query.where_contradiction`): a top-level AND-chain proven never TRUE from
  plan-time literals (literal FALSE/NULL, a false literal comparison, bare-column literal ranges or
  equalities with no common value) reads no relation and charges nothing for it; the pipeline above
  runs over the empty input. **Pushdown** (`query.where_pushdown`): a structurally non-trapping
  single-base-table conjunct on a preserved side of a join runs as that table's rows are read (per
  admitted row, before the join and the row account); only the residual is applied to joined rows.
  EXPLAIN shows pushed filters on their Scan; the estimator models both; NoREC `where_pushdown` and
  `contradiction` relations.
  - [x] push into **inlined CTE bodies** and **below a window** — a conjunct over columns every
    window partitions by moves below the window ([planner.md §3.2](spec/design/planner.md)); a CTE
    named exactly once (by a query part), not MATERIALIZED, uncorrelated, is planned at its reference
    like a derived table and takes the same body pushdown (§3.4; its definition plans with reference
    expansion off and each body expands at most once, so chains plan linearly). EXPLAIN renders such
    a body under its `CTE Scan`. NoREC `cte_window_pushdown`.
  - [x] push into a **widened set-operation arm** — the arm reads its column through the set
    operation's own widening: bare for an integer width, `CAST(col AS decimal|f64)` for int→decimal /
    f32→f64 ([planner.md §3.6](spec/design/planner.md)). NoREC `safe_conjunct_pushdown`.
  - [ ] _follow-on:_ push into each reference of a multiply-referenced `NOT MATERIALIZED` CTE (needs a
    planning-work bound: expanding every reference re-plans a chain exponentially), into a CTE
    referenced from a data-modifying part (planned only when it runs), and through a lateral
    relation.
  - [x] **broader safe-conjunct gate** (`query.safe_conjunct_gate`, [planner.md §3.5](spec/design/planner.md))
    — a catalog-level `traps` / `preserves_equality` classification, name-level in catalog.toml and
    per row in casts.toml, codegen'd into every core ([functions.md §15](spec/design/functions.md)):
    the non-trapping text functions (`length`, `lower`/`upper`, the trims, `strpos`, `starts_with`, …)
    and casts (integer widenings, integer → decimal/float, `f32` → `f64`, `boolean` ↔ `i32`, renders
    to `text`) move; a decimal/float render (`preserves_equality = false`) moves into a body only when
    the body matches no rows. **Parameter-time proof:** a LIKE / ILIKE moves when its pattern is a
    plan-time literal with no unpaired trailing escape, a bound parameter read through the custom-plan
    frame. NoREC `safe_conjunct_pushdown`.
  - [ ] _follow-on:_ more plan-time proofs and classifications — integer `/` and `%` by a literal or
    bound divisor outside {0, −1}, a regular expression whose literal pattern compiles at plan time,
    the remaining non-trapping built-ins (numeric `trunc`/`sign`/`scale`, the range accessors, the
    SHA-2 digests), the datetime renders; and widening the hash join's own residual gate
    ([cost.md §3](spec/design/cost.md)), which still admits only bare-column comparisons.
  - [ ] _follow-on:_ plan-time **constant folding** / CSE / eliding a pushed recheck that its own
    access bound already guarantees — each removes `operator_eval` charges, so each needs its own
    explicit cost decision (the framing above), not a silent apply.

---

## Storage maturation (§9)

> The path to a **larger-than-RAM file that does not fall over** (CLAUDE.md §9): no
> full-residency assumption above the storage seam.

- [x] **Durable commit latency — validated COW implementation** — production v33 across Rust,
  Go, and TypeScript: inline/overflow manifests, one steady-state durable barrier, protected
  dependency reuse, recovery/foreign-generation stabilization, and poisoned-writer rejection.
  Verified with independent Ruby fixtures, fault devices, shared recovery corpus, mixed-core
  process/reuse byte parity, and real durable Go benchmarks (1.60–1.65× in the measured Linux cases).
  → [validated COW](spec/design/validated-cow.md),
  [experiment report](spec/design/durable-commit-experiments.md). _(size: L)_
  - [x] **macOS measurements** — Apple M3 Max/APFS: v33 cuts the production durable commit
    1.72–1.76× (9.0 → 5.2 ms); `F_FULLFSYNC` is ~4 ms and the whole cost. The
    `F_BARRIERFSYNC` → full variant (4.8 ms) is slower than v33's single drain (4.2 ms), so it
    is not adopted; the full durable-completion guarantee is unchanged.
    → [macOS measurements](spec/design/durable-commit-experiments.md#macos-measurements-2026-10-13)

- [x] **Shared multi-process file access** — ✅ **landed, then superseded** by the page service (phase 4 retired it; [locking.md](spec/design/locking.md) now specifies the bundle and ownership). The stable `protocol-v1` bundle, alone fast path, global writer, commit/meta gate, append-only shared commits, foreign-plan invalidation, independently coordinated file attachments, and Rust↔Go↔Node real-process matrix are implemented. The independent TypeScript engine uses the narrow first-party `impl/ts/native-lock` Node-API adapter; the full Rust wrapper remains a workload-dependent reach experiment. `locking = auto` is shared on capable local hosts. Pre-protocol binaries still require a one-time drain. → [locking.md](spec/design/locking.md), [benchmarks.md §7.3](spec/design/benchmarks.md)
  - [ ] **Node prebuilt distribution matrix** — ship no-install-script Node-API-8 artifacts for Linux glibc/musl x64+arm64, macOS x64+arm64, and Windows x64 with SHA-256/provenance and packaging tests. Source/dev builds already use `mise run ts:lock_build`; missing production artifacts fail closed `0A000`.
  - [ ] **Expanded shared-file fault/platform matrix** — extend the landed real-process handoff, timeout, pinned-reader, killed-writer, and attachment scenarios with deterministic body-sync/meta-publish fault hooks, a joiner arriving mid-compaction (the basic presence refusal + later join is `compact.process.toml`), symlink/hard-link process lanes, and every supported Windows/macOS packaging lane.
- [x] **Survive stored SQL a binary cannot parse** — ✅ **landed** ([unparsable-definitions.md](spec/design/unparsable-definitions.md)). Catalog loading used to fail the whole file with `XX001` when stored SQL text (a view, expression `DEFAULT`, `CHECK`, partial-index predicate, or expression index key) used syntax a newer jed added without a `format_version` bump (`SEARCH`/`CYCLE`, e92a7a3e), locking an older program out of a file a newer one edited: standalone, in shared mode, and under any future page service. The load now keeps the object unparsed with its text unchanged; only statements that need it fail, with the new `XX003`: a reference to the view, a write that evaluates the check or applies the default, a write that maintains the index, and the `ALTER TABLE` actions that analyze or rewrite the text. Reads proceed and skip the index; `DROP` of each object and `DROP TABLE` work. All three cores, against the Ruby-forged `unparsable_definitions.jed` fixture and `suites/storage/unparsable_definitions.test`. Also fixed in passing: the index-nested-loop path no longer uses a partial index as its inner (it never checked the predicate, so it could drop rows; indexes.md §9 already excluded it).
- [x] **File compaction / shrink (return space to the OS)** — the host-invoked `compact(name)` rewrites `main` or an attachment as the from-scratch image of its committed snapshot at `txid + 1`, streamed page by page into `<path>.jedtmp` and atomically renamed into place (in-memory: a fresh `MemoryBlockStore`). Never waits: a held writer gate, a pinned reader, or another process holding the file is `55006`; read-only `25006`; OPFS `0A000`. Rows, results, costs, and prepared statements survive; the shared process corpus covers presence refusal and a later join (`compact.process.toml`). Ordinary commits also return trailing free space without a rewrite (in-place trailing truncation: an owner commit whose top pages are on the persisted free list lowers `page_count` past them and shortens the file after its barrier, keeping one preallocation step; never on a page client; under `synchronous = off` only a batch's first commit lowers the high-water and the file is shortened at the flush; simulation S10, `truncation` profile (half relaxed) + exact high-water and floor-length oracles). → [api.md §2.6](spec/design/api.md), [storage.md](spec/design/storage.md), [format.md *Trailing truncation*](spec/fileformat/format.md)
  - [ ] _follow-on:_ **OPFS compaction** — write the image to a sibling OPFS file and swap it in once a crash-safe replace is designed for the browser host (`0A000` today). _(size: M)_
  - [ ] _follow-on:_ compaction options — repack half-empty leaves (today nodes are kept as-is so costs are unchanged) and change `page_size` during the rewrite. (GiST R-tree pages now stream to the image page by page; the R-tree itself is still built from the index's keys in memory, as every commit does.) _(size: M)_
- [x] **Blocking sort and hash-operator spill** — `ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate and `DISTINCT` use bounded row/state buffers under `work_mem` on native file hosts. Ordered disk hash partitions preserve JOIN candidate order, aggregate fold order, first occurrence and exact costs; direct scan feeds and cost prepasses stream. Aggregate DISTINCT and growing ordered-set/hypothetical collections spill, with bounded external ordering and descriptor counts. Shared forced-spill corpus checks run in CI; `mise run bench:spill` checks wide-input results/costs and measures RSS. → [spill.md](spec/design/spill.md)
  - [ ] **Remaining query-memory owners** — Q1 (row buffers, result collectors), Q2 (operator state), Q3 (pending writes), and the storage limit (Q4a in-memory, Q4c file-backed — memory.md §8.7) have landed, as has every constructing scalar kernel (cost.md §8.1, S1–S5); host scratch support for OPFS/WASI remains. `work_mem` is an operator threshold, not a whole-query memory ceiling. → [memory.md](spec/design/memory.md), [hosts.md](spec/design/hosts.md)
- [x] **Point-lookup, cold-read, and INSERT performance work** — the measured point-lookup,
  checksum/PAX/buffer-pool, concurrent-fault, and INSERT execution/tree slices have landed; final
  timings, allocation evidence, checksums, and decisions live in
  [benchmarks.md](spec/design/benchmarks.md), [pager.md](spec/design/pager.md), and
  [locking.md §9.3](spec/design/locking.md).
  Go's ordinary streaming range scans now reuse private input storage while preserving owned
  output rows; the allocation/complete-query measurements and adoption gate are recorded in
  [the Go row materialization report](bench/go-row-materialization/results/2026-10-19/report.md).
  This is independent of the rejected G1/G2/G3 execution-specialization experiments.
  - [x] _follow-on:_ unique-dirty mutation for remove/rebalance (Rust). Removes and merges now edit a
    uniquely owned dirty path in place under INSERT's `Arc::get_mut` + `page == 0` rule; tree shape,
    bytes, and costs are unchanged. Allocator calls fall 89% for 1,000 consecutive deletes and 99% for
    a 10,000-row range delete, and the new `delete_*` lanes run 2–32× faster.
    → [benchmarks.md](spec/design/benchmarks.md) ("DELETE unique-dirty result"),
    [transactions.md §3](spec/design/transactions.md)
  - [ ] _follow-on:_ the same DELETE/rebalance transient for Go and TypeScript, over their INSERT
    mutation-generation tokens. Measure first: their delete path's allocation and GC profile differs
    from Rust's. _(size: M)_
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/`decimal`/`json` pushed out-of-line onto overflow-page chains (`format_version` 3), optionally LZ4-compressed first via a deterministic hand-rolled block codec (no third-party dep — a library fails §8 byte-identity). → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [x] _follow-on:_ **chain sharing on rewrite** — ✅ **landed**: an UPDATE that does not assign a large column and a commit that rewrites the leaf around it keep the value's stored form and overflow chain (no read, decompress, recompress, or chain write). The plan is computed from stored facts, so dispositions, bytes other than page numbers, and cost are unchanged; no `format_version` bump. The live-page count skips kept chains; image checkers enforce one record per chain. → [large-values.md §15](spec/design/large-values.md)
    - [ ] _follow-on:_ keep chains on the remaining rewrite paths — `INSERT … ON CONFLICT DO UPDATE`, table rewrites (`ALTER TABLE`), and in-session rows of a table whose single root leaf stays resident after commit. Physical only (no byte or cost change). _(size: S–M)_
- [ ] **Upgrade path for older files + an on-disk stability commitment (a 1.0 blocker)** — a reader
  accepts only the current `format_version` ([format.md](spec/fileformat/format.md)), so every bump
  strands existing files, and the CLI's `--dump` needs the old binary to cross. Decide the 1.0 policy:
  an in-place upgrade on open, a `jed upgrade` that carries readers for older versions, or a frozen
  format with additive changes only. Ratify or replace the
  [compatibility.md](spec/design/compatibility.md) proposal alongside. _(size: L + decision)_
- [x] **Batched durability and group commit** — `synchronous = off` (a create/open handle setting,
  `format_version` 49): a relaxed commit is visible once its pages and meta are written, and one
  flush makes a run of commits durable — fsync coalescing across every committer of the handle.
  Pending commits write the slot not holding the newest durable meta, carry cumulative manifests,
  reuse only pages the floor had free, and keep the batch off the free list, so recovery lands between
  the last flush and the newest attempted commit and never corrupts. The batch ends at the
  inline-manifest cap (a background flush in Go/Rust, inline in TS), at close, before a page client's
  `begin_write`, and at compaction. Simulator S9 (`relaxed` profile, engine-aligned floor oracle).
  → [validated-cow.md §4](spec/design/validated-cow.md), [transactions.md §9](spec/design/transactions.md)
  - [ ] _follow-on — needs a decision:_ a **time-based flush** — a background flusher that makes an
    idle relaxed batch durable after an interval. Its default interval and the knob's name are a
    product decision (no spec names one); until then an idle relaxed handle stays undurable until its
    next cap flush or `close`. _(size: S + decision)_
  - [ ] _follow-on:_ honor a page client's own `synchronous = off`: a client's setting does not reach
    the owner (no wire field), so every client commit keeps its barrier; batching them needs a commit
    flag in `spec/pageservice/` and a way to name the owner's pending set. _(size: M)_

---

## Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> + the CLI have landed; OPFS spill + the e2e-in-CI gap remain. Parallelizable with most feature work.

- [x] **One explicit `Environment` for the engine's outside inputs** — landed in all three cores: the process-global loaded collation/property/time-zone sets, the free-function and handle-level loads, the `ExtensionRegistry`, and the entropy/clock setters are replaced by one immutable `Environment` (built by an `EnvironmentBuilder`, `to_builder()` derives an add-and-replace variant) passed to `open`/`create`; a missing one is a fresh empty environment. Sources are controllable objects (seeded `reseed`, manual clock `set`/`advance`, advancing clock `reset`). No format/SQL/conformance change. → [environment.md](spec/design/environment.md)
- [x] **Unify the create constructor** — the five overlapping DB constructors collapsed to two:
  `create(opts)` (fresh, either backing; `opts.path` absent → in-memory) and `open(path, opts)`
  (existing file); Go's exported `OpenDatabaseWithOptions`/`OpenOptions` closed the last open-surface
  divergence. Host-API only, byte-neutral. → [api.md §2.1.1](spec/design/api.md)
  - [ ] _follow-on:_ the anticipated create-time knobs as new `CreateOptions` fields
    (`max_storage_bytes` has landed as the storage limit, memory.md §8 / Q4a and Q4c; next a spill `temp_dir`, then a thread count);
    optionally sweep the async OPFS/browser host (`createOpfs`/`OpfsDatabase.create`) into the
    unified `create(opts)` shape.
- [x] **Unknown-typed function arguments** — a bare `$N` or `NULL` scalar-function argument adopts
  the family the catalog declares for its slot (one candidate decides, `text` wins among several,
  undecidable is `42725`; no preferred-type step), and a `$N` operand of `LIKE`/`ILIKE`/`~` adopts
  `text`. The `LIKE … ESCAPE` desugar's cast workaround is gone. → [functions.md §9.1](spec/design/functions.md), [api.md §5](spec/design/api.md)
  - [ ] _follow-on:_ the same rule over host-registered function signatures (an unknown argument is
    `42883` there today); the dedicated resolvers (array, range-constructor, json-set functions)
    still type their own arguments; a parameter already typed by an earlier occurrence still counts
    as unknown in a later function argument.
- [ ] **Integrity-check host API** — SQLite's `PRAGMA integrity_check` / `quick_check`: verify page
  checksums, tree invariants, free-list and live-count accounting, and index ↔ table agreement on a
  live database, returning structured findings. The checkers already exist in the simulation runners
  and `spec/fileformat/verify.rb`; expose them through the host API and the CLI. _(size: M)_
- [ ] **Change notifications** — host callbacks after a commit (tables touched, `txid`), plus a cheap
  "has anything committed since" counter (SQLite's update/commit hooks and `PRAGMA data_version`), for
  reactive embedded apps and cache invalidation. Must cover commits from other page-service
  clients. _(size: M)_
- [ ] **Database ↔ bytes** — open a database from a byte buffer and export one to bytes (SQLite's
  `sqlite3_serialize` / `sqlite3_deserialize`), over the in-memory `MemoryBlockStore`; useful for
  browsers, tests, and shipping prebuilt databases. _(size: S)_
- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the per-core `FileBlockStore`s, the Node `fs` host, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, a Playwright real-browser e2e run inside `mise run ci`). **Open:** OPFS disk-spill. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **jed-migrate — the schema-migration library** — tern-modeled, opt-in migration packages at
  `/migrate` (Go + Rust + TS; a NON-CORE consumer per language) over one shared contract +
  `testdata` corpus; one committed transaction per step (resumable); bundled by the CLI as
  `jed migrate`. → [/migrate/design.md](migrate/design.md)
  - [ ] _follow-on ([design.md §11](migrate/design.md)):_ an all-or-nothing whole-run transaction
    mode. (A per-migration ledger table — drift detection, out-of-order, truthful status — is **not
    planned**; a potential option only on a definite need.)
- [x] **Schema introspection — the `jed_` catalog relations** — I0 (the `jed_` name reservation,
  `42939`), I1 (`jed_tables` + `jed_columns`), I2 (`jed_indexes` + `jed_constraints`), and I3
  (`jed_sequences` + `jed_types` + `jed_type_fields`) are landed (all 3 cores): read-only computed
  relations resolved in every database's relation namespace, derived from the pinned catalog snapshot
  (no storage, no format bump), riding the SRF plan shape; per-table `SELECT`-gated (`42501`),
  write/DDL targets `42809`, one `generated_row` per row; caps `introspect.*`. SQL against the `jed_`
  relations is the whole surface (no host-API convenience); `information_schema`/`pg_catalog` are
  recorded non-goals. → [introspection.md](spec/design/introspection.md)
  - [x] **`jed_columns.column_default`** — ✅ LANDED (all 3 cores). An expression default shows its
    persisted canonical token text, a constant its stored value as a SQL literal, `DEFAULT NULL` NULL
    (PG stores none). No storage change. → [introspection.md §5](spec/design/introspection.md)
  - [x] **View columns in `jed_columns`; `jed_constraints.operators`** — ✅ LANDED (all 3 cores). Each
    main-database view contributes one row per output column, typed by planning its body (result
    types, no typmods); an EXCLUDE row's `operators text[]` lists each element's `WITH` operator,
    parallel to `columns`. No storage change.
    → [introspection.md §5/§5.1](spec/design/introspection.md)
- [x] **Structured error fields** ([error-fields.md](spec/design/error-fields.md)) — ✅ LANDED (all 3 cores). `EngineError` gained four optional identifier fields modeled on pgx's `pgconn.PgError` — `ConstraintName`/`TableName`/`ColumnName`/`DataTypeName` (Rust `Option<String>`, Go `string`, TS optional) — so a host identifies *which* constraint fired without regexing the (non-contractual) message. Populated via **typed constructor helpers** that own message *and* fields together (no drift): 23505/23514/23503/23P01 → constraint+table; 23502 → column (+ table stamped at the DML boundary via `stampTable`/`.map_err`); 22003 routes through the `overflow(ty)` helper + 22001 varchar → data type (+ column). `Display`/`Error()` unchanged; per-core `error_fields` tests (corpus can't assert structured fields). **Hard-excluded:** pgx's `File`/`Line`/`Routine` (core source location differs across cores → would break §8 byte-identity). No format bump, no cost/determinism change.
  - [x] **`Detail`** — ✅ LANDED (all 3 cores). PostgreSQL's DETAIL sentences (`Key (id)=(1) already exists.`, `… is not present in table "t".`, `Failing row contains (…).`) as a fifth structured field, never appended to the one-line message (house style kept; the CLI prints a psql-style `DETAIL:` line). Values through the result-cell renderer, 64-byte cap, withheld without `SELECT` on the table; NOT NULL now checks the complete row (PostgreSQL's order). Pinned by the shared `spec/errors/detail_cases.txt`. → [error-fields.md §9](spec/design/error-fields.md)
  - [x] **`Position` + `Hint`** — ✅ LANDED (all 3 cores). `Position` is the 1-based character offset of the offending token (PostgreSQL's unit): every lexer/parser error except the class-54 limits, column-reference `42703`/`42702`/`42P01`/`42P10`, unknown INSERT/SET targets, unknown FROM tables and DML targets (`42P01`), function lookup (`42883`/`42725`), operator operand types (`42883`/`42804`), non-boolean arguments, common-type and assignment mismatches (`42804`), and resolve-time literal input (`22P02`, …); script errors count from the script start. Tokens carry character positions; the AST records equality-invisible `SrcPos` on named nodes; stored text records none. `Hint` is PostgreSQL's misspelled-column (Levenshtein, `errorMissingColumn`) and table-alias suggestions, its LATERAL hints (the search also walks the FROM items a reference cannot see), and its fixed function-lookup, operator-lookup, ordering-operator, assignment, and recursive-CTE sentences. The CLI prints psql's `LINE n:` cursor and `HINT:` line. Pinned by the shared `spec/errors/position_cases.txt` (PostgreSQL-captured). → [error-fields.md §10–§11](spec/design/error-fields.md)
  - [x] **Remaining hints, details, positions + `DatabaseName`** — ✅ LANDED (all 3 cores). The LATERAL Details ("There is a column named … but it cannot be referenced from this part of the query."); the collation hints (`42P21` / `42P22` "…applying the COLLATE clause…"), the `428C9` identity-column Detail + Hint, the `2BP01` CASCADE / drop-owner hints, and `to_timestamp` / `to_date` input errors' Details and Hints; positions for `42704` unknown types, casts with no conversion, a simple `CASE`'s `WHEN`, `INSERT … VALUES` literal slots, sort/group/`DISTINCT`/set-op keys with no operator, a recursive CTE's column type, and every expression kind (`$N`, subqueries, constructors, `EXTRACT`, SQL/JSON). `DatabaseName` — the analog of pgx's `SchemaName` — names the database (`main` / `temp` / attachment) whenever `TableName` is set; the CLI prints a non-`main` one. Pinned by `position_cases.txt` (new `detail:` / `database:` / `attach:` / `load-unicode` directives). → [error-fields.md §11.8, §11.9, §12](spec/design/error-fields.md)
  - [ ] _follow-on:_ PostgreSQL's input-syntax Details for malformed `json` / array / range literals ("The input string ended unexpectedly.", "Unexpected end of input."), the `numeric field overflow` Detail, the `2BP01` Detail listing every dependent, and the LATERAL Detail for a column hidden behind a join alias; positions for collation conflicts (PostgreSQL: the second collation's expression), a `CREATE TYPE` field's type, and a `CAST(x AS t)` of a non-literal. → [error-fields.md §7, §10.2](spec/design/error-fields.md)
  - [ ] _follow-on (bugs found here):_ two `DROP`s jed allows that PostgreSQL refuses with `2BP01`: `DROP INDEX` of the index backing a `UNIQUE` constraint (`CREATE TABLE t (a int UNIQUE); DROP INDEX t_a_key` succeeds — PostgreSQL: "cannot drop index t_a_key because constraint t_a_key on table t requires it", HINT "You can drop constraint t_a_key on table t instead."), and `DROP SEQUENCE` of a sequence a column `DEFAULT nextval(…)` uses (not an owned `serial` sequence — PostgreSQL: "cannot drop sequence s because other objects depend on it"). → [constraints.md §5](spec/design/constraints.md), [sequences.md](spec/design/sequences.md)

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `mise run corpus:import` / `corpus:check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `mise run corpus:norec_sweep`, in `mise run ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning — now including **aggregate-TLP** for `COUNT`/`SUM`/`MIN`/`MAX`/`AVG`, both ungrouped via scalar-subquery combination — `SUM` by `COALESCE` (grammar.md §51), `MIN`/`MAX` by `LEAST`/`GREATEST` (grammar.md §52), `AVG` as summed sums over summed counts with a by-construction `select_div_scale` expected — and per-`GROUP BY`-group via a UNION-ALL derived-table super-aggregate), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator) and broader NoREC relations. _(size: M remaining)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `mise run corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations.
- [x] **Deterministic simulation testing** — S1–S7 landed in all three cores ([spec/design/simulation-testing.md](spec/design/simulation-testing.md)): generator + runners + oracles, byte-identical transcripts and crash images across cores, resource limits and per-core I/O faults, spill faults through a spill-host seam in every core (sort, spool, hash, and temp-table spill), a simulated filesystem (`create`, compaction, file attachments with per-database oracles), simulated multi-process shared access, `verify.rb --check-image`, the reader-aware bounded-growth invariant, and the `corrupt` mode; `ci` runs the regression traces and a fixed sweep. It found three engine bugs (all fixed). **Standing obligation** (§13 of the design): a new page type, commit path, host operation, or storage setting grows the generator and checkers in the same change.
- [ ] **Coverage-guided fuzzing of SQL text** — untrusted SQL must never crash a core (CLAUDE.md §13;
  a panic is a denial of service), but the only fuzzer today mutates on-disk pages
  (`impl/go/corruption_fuzz_test.go`). Add parser → resolver → executor fuzz targets in every core
  (Go's built-in `testing.F`; a Rust/TS fuzzer is a tooling dependency needing §14 confirmation),
  seeded from the corpus, with crashes reduced into regression `.test` entries. _(size: M)_
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): concurrent-reader cross-engine (PG/SQLite) + truly larger-than-pool eviction/thrashing variants (the jed-only resident and cold-population `concurrent_read_pk*_r{1,4}` pairs landed — benchmarks.md §8.1), durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** — wrap the safe Rust core, shipped as a gem (conforms by construction; a distribution artifact, not an independent conformance voice). **Landed:** Slice 1 (the `cdylib` + a `fiddle`-loaded pure-Ruby gem), Slice 2 (`$N` bind params), Slice 3 (richer typed values — `BigDecimal`/`Date`/`Time`, AR-style), Slice 4 (host-loaded `JUCD`/`JTZ` bundles), Slice 5 (**host functions** — `Jed::ExtensionRegistry` with single-row and batch kernels upcalling through Fiddle closures, ABI v5, measured by the `host_function_*` triple, benchmarks.md §8.2), and the binding-overhead benchmark (`bench/ruby`). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `mise run ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**; **host functions over `bytea`/`uuid`/`interval`/`json`/`jsonb`/`jsonpath`** (needs a native text→value parse at the boundary, ruby.md §5b); **`component_id`/`semantic_version` on gem host functions**, without which a gem host function can back neither an index nor a `CHECK` (extensibility.md §7.1/§8.1). _(size: L wrap)_
- [ ] **Native Node/Rust reach package experiment** — the benchmark-capable prototype is built (`impl/node`, `bench-node-rust.ts`), exact-pinned, and checksum-verified against all 53 lanes. It is not production-complete: add the full typed value/error/session API, async/worker-thread story, host-function policy, prebuilt/provenance matrix, packaging tests, and a package-name/product decision before presenting it as an alternative to native TS. Do not count it as a conformance core—it echoes Rust. Current performance decision is workload-dependent; see [cores.md §5.2](spec/design/cores.md) and [benchmarks.md §7.3](spec/design/benchmarks.md). _(size: L wrap)_
- [ ] **Python package** — the largest audience for embedded databases (the `sqlite3` module,
  DuckDB), and absent from the plan. Likely a wrap of the safe Rust core (PyO3 or the C API below —
  either needs §14 confirmation), with a DB-API 2.0 (PEP 249) surface, `Decimal`/`datetime`/`uuid`
  value mapping, and prebuilt wheels. Record the native-vs-wrap call in [cores.md](spec/design/cores.md). _(size: L wrap)_
- [ ] **Public, stable C API + header** — the Ruby gem already loads a Rust `cdylib`; promoting that
  boundary to a documented, versioned C ABI lets PHP, Dart/Flutter, Elixir, and others bind cheaply
  (SQLite's C API is the reason it is everywhere). _(size: L)_
- [ ] **C#** core — **lean native** (value-type generics, `Span<T>`, NativeAOT → near-Rust speed *and* a clean pure-managed NuGet package; in-process host functions). Strongest native candidate. _(size: XL native / L wrap; §2)_
- [ ] **Swift** core — **lean wrap** (UniFFI + XCFramework over the safe Rust core: Rust speed, well-trodden Apple packaging, untrusted-query safety preserved §13). Native only if hot-path per-row host functions make the FFI upcall tax dominant. _(size: L wrap / XL native; §2/§13)_
- [ ] **Java** core — **conflicted**: wrap for performance (pre-Valhalla boxing + JIT warmup hurt a native core), native for clean pure-JAR packaging + in-process host functions (no JNI/upcall tax). Decide at scheduling time; Valhalla shifts it toward native. _(size: XL native / L wrap; §2)_
- [x] **Runtime function registry** — built-in named scalar + aggregate resolution is data-driven over the generated catalog tables (one `(name, arg_families)` lookup). → [extensibility.md §5](spec/design/extensibility.md)
  - [ ] _follow-on:_ built-in type-vtable dogfood (Fork A).
- [x] **Host scalar functions over existing types** (extensibility.md §14 step 3, all 3 cores) — a host builds an `ExtensionRegistry` (scalar functions over the built-in types), passes it in `CreateOptions`/`OpenOptions`, and the engine freezes it for the handle + resolves/evaluates host calls through a registry dispatch arm beside the built-in one (reached by id). Exact-signature overloading (built-ins win an exact collision, `42723` on a duplicate registration); ephemeral (no persisted use / no DDL); strict; declared static **cost** charged per call + guarded; wrong-typed kernel result caught (`22000`). → [extensibility.md §4.2 / §5.1](spec/design/extensibility.md)
  - [x] _follow-on:_ **vectorized/batched host-function ABI** — **DONE (all 3 cores).** Every host function runs through a column-in→column-out batch kernel (a single-row kernel is looped); the buffered projection prefetches a chunk of ≤1,024 rows per non-volatile call and replays row by row, so rows/cost/errors are identical to batch-of-one by construction (speculation capped by the cost headroom). Benchmarked by `host_function_{none,row,batch}` (benchmarks.md §8.2). → [extensibility.md §4.2.1](spec/design/extensibility.md)
    - [ ] _follow-on:_ batch at the remaining row-evaluation sites — index-driven streaming scans (carry the per-row point-lookup charge to replay), streaming joins, `WHERE`/`ON` filters inside blocking scans and joins, aggregate arguments, `INSERT … VALUES`, `UPDATE … SET`, `RETURNING`, index build/maintenance; and calls with arguments beyond column/constant/param. The Ruby gem now exposes host functions, so the bench triples measure a real boundary (benchmarks.md §8.2); the Node/Rust package remains. _(size: M)_
  - [ ] _follow-on:_ non-strict host functions, host functions over container args, and runtime **enforcement** of the recorded `cross_core` declaration (taint/containment of non-cross-core results — no runtime taint mechanism exists yet, matching how `float` is handled at the spec layer). _(`immutable` is now enforced for index-backing, and an immutable host function may back an index — below.)_
- [x] **Host functions in persisted indexes** (extensibility.md §14 step 4, all 3 cores, `format_version` 31) — **Model B (registry-only, no SQL DDL)**: a `HostFunction` gains `component_id` + `semantic_version`; an `immutable` + versioned host function may back an expression / partial index (`CREATE INDEX ON t (geo_hash(a))`). The index catalog persists a per-index host-function **dependency list** (`index_flags` bit2 `has_host_deps` + the `(name, arg types, result, component_id, semantic_version)` tuples); the per-statement re-resolution re-checks it — a **read** whose candidate index no longer matches skips it (correct heap scan), a **write** is refused (`XX002` component/version mismatch, `42883` missing function), never a silent stale-key read. Also **fixed a latent bug**: a host function (even `volatile`) previously leaked into an index expression (the immutability gate was purely syntactic) — now `42P17`. → [extensibility.md §7 / §8.1 / §11](spec/design/extensibility.md)
  - [ ] _follow-on:_ the full [compatibility.md](spec/design/compatibility.md) per-object graded reopen manifest (step 4 landed the index-only form).
  - [x] _follow-on:_ **`REINDEX`** (all 3 cores) — `REINDEX [(VERBOSE | CONCURRENTLY …)] {INDEX | TABLE} [CONCURRENTLY] [db.]name` and `{DATABASE | SYSTEM} [CONCURRENTLY] [name]` rebuild indexes in place from their stored definitions, re-admitting key expressions and predicates against the current environment — the one-statement repair for a stale host-function or timezone pin (an unparsed definition stays `XX003`). PG-matched codes (42P01/42809/42601/0A000, 25001 for CONCURRENTLY/DATABASE/SYSTEM in a block); cost = each index's CREATE INDEX build scan; `SCHEMA`/`TABLESPACE` 0A000; no format change. → [indexes.md §10](spec/design/indexes.md)
    - [ ] _follow-on:_ a database qualifier on `DROP INDEX` (REINDEX INDEX already takes one).
- [ ] **Host-defined types and operator classes** — the designed but unscheduled steps 5–8 of
  [extensibility.md §14](spec/design/extensibility.md): opaque host scalar types (Storable /
  Equatable / Ordered) with derived `array<host-type>` and the graded reopen verdict for every object
  kind; keyable host types (an order-preserving encoder + the invariant harness); host GIN/GiST
  operator classes behind the unified seam; and SQL syntax to choose a non-default operator class.
  _(size: XL across the steps)_
- [ ] **Host aggregate and table-valued functions** — not yet designed. Host aggregates (state,
  transition, final, as a batched kernel) and host functions that return rows usable in `FROM` (SQLite's
  virtual tables, DuckDB's table functions — a host-extension row source, CLAUDE.md §9/§13). Design
  in extensibility.md first. _(size: L each)_

---

## Maybes / distant ideas (keep the door open — do NOT schedule)

> Not backlog. Architectural doors to **leave open**, not walk through now. The §9 rule — SQL
> is the primary surface and everything must be reachable through it, but it need not be the
> *only* access path — is read **broadly** here. Nothing below is a commitment; the only
> requirement is that nearer-term work not quietly foreclose these.

- **Alternative access paths beyond low-level direct reads.** §9 already keeps a sub-SQL
  `getValue("table", key)` seam open. Read that intent broadly: keep the architecture from
  foreclosing *entirely different* surfaces over the same storage + type core.
- **Other query languages.** SQL is clunky; the core (typed values, order-preserving keys,
  relational storage) need not be SQL-only. A graph query language, a document/dataframe
  surface, etc., could one day sit *beside* SQL over the same engine. Very distant.
- **Graph / vector workloads.** Growing toward graph traversal or vector-similarity search.
  §9 already flags alternative physical layouts as open; a vector index would be another.
- **Further collation expansion.** Curated tailorings, nondeterministic collations, `LIKE` under
  non-`C`, CLDR `shifted`, and CJK tier-3 data remain possibilities, not scheduled work.
  → [collation.md §14](spec/design/collation.md)
- **Full-text search.** PostgreSQL (`tsvector`/`tsquery` + GIN) and SQLite (FTS5) both ship it. In
  jed it would be a type pair, a tokenizer/stemmer data bundle loaded through the `Environment` (the
  collation precedent), and a GIN operator class. Large; not scheduled.
- **Browser/Node async runtime.** [ts-runtime.md §10](spec/design/ts-runtime.md) implements
  async-only application entry points over dedicated workers, versioned lossless values and
  structured errors, bounded admission/results/handles, session ordering with parked writer
  waiters, prepared/batched execution, transaction callbacks, cleanup, failure uncertainty and
  atomic cancellation on Node/shared-memory browsers plus cooperative cancellation at logical
  page boundaries and common execution loops on every host. Retained continuations share sync
  and async semantic paths, including awaitable Node data-page reads. The immediate-expression
  prototype removes generator traversal for common resident expressions; pending operands retain
  their state without replay. Transferable binary result batches avoid cloning recursive value
  objects, and buffered iteration avoids per-row scheduling chains. Broaden these changes only
  with throughput and cancellation measurements. Heavy-query measurements now cover Node and
  ordinary/isolated Chromium, including main-thread responsiveness and 200,000 typed result rows
  ([report](bench/ts/results/2026-10-18-heavy/report.md)). Default fetches now admit up to 512 rows
  under the same byte allowances; the [batching investigation](bench/ts/results/2026-10-18-result-batching/report.md)
  measures the gain and remaining substantial per-value delivery overhead.
  The elapsed-time scheduling experiment was reverted after both 1 ms and 0.125 ms slices
  regressed browser message-only cancellation. The original 256-checkpoint task yields remain;
  [rollback measurements and transport options](bench/ts/results/2026-10-19-scheduling-rollback/report.md)
  retain the evidence. Shared-memory result transport and batch-oriented consumption remain
  unimplemented investigations, separate from the existing atomic cancellation flag.
  The [query-stage profile](bench/ts/results/2026-10-19-query-profile/report.md) separates worker
  execution/waits, message delivery/dispatch, result decoding and client consumption, with
  independently sampled main/worker CPU and explicit instrumentation calibration.
  The [profile-guided optimization](bench/ts/results/2026-10-19-delivery-optimization/report.md)
  retains the SQL hex ASCII fast path (typed-output worker time −6% Node / −31% Chromium).
  Codec byte-view reuse and idle dispatch were measured and reverted: only about 1% additional
  throughput with larger observed Node timer gaps. Transport overhead remains substantial;
  batch limits, full decoding validation and 256-checkpoint yields are unchanged.
  Website examples/live panels use it;
  the shared concurrency schedules run through the async scheduler. Synchronous internals remain
  for conformance/tooling. Browser custom host functions use `workerFactory` and the public
  `jed-ts/browser/worker` startup helper; registration runs once before storage opens, and jed
  owns the dedicated worker's cleanup. Browser-safe registration modules can be reused through
  Node's `extensionModule`. Cancellation is best effort; sorting, parsing/planning and long scalar
  kernels stay synchronous unless measurements justify extra pause points. Commit, open/recovery
  and storage maintenance await asynchronous page-codec/device stages (ts-runtime.md §11).
  Remaining: crypto providers, an async compaction (with OPFS compaction), cold/durable and
  other-browser public-boundary measurements, ARM/package-size measurements, and optional Rust
  delivery parity.
  Public async host API follow-ons: compaction, changing database storage limits after open,
  the storage-byte gauge, and resetting an individual custom session variable. The docs describe
  their current availability; displayed TypeScript examples execute in the Node unit suite.
  Encryption protocol/provider selection is separate; no SQL replay or whole-file preload.
- **Encryption at rest (file-level).** Whole-file or per-page encryption is a door to keep
  open, **designed in [encryption.md](spec/design/encryption.md)**: a page codec in the core
  above the block seam, standardized AEAD and envelope encryption (cheap credential rewrap,
  full live-page rewrite for data-key rotation). Crash/restore/fork-safe nonce allocation and
  authenticated metadata transitions remain open; `(page_index, txid)` alone is insufficient.
  Vetted crypto uses safe APIs with explicitly reviewed accelerated backends (§14). Page-sized
  AEAD, real-browser WebCrypto/JS/Rust-WASM and writer-key lifecycle probes are recorded in
  `bench/aead/`. Suite direction (encryption.md §3.1): AES-256-GCM with HKDF keys from fresh random
  salts, one key per page (chosen after Go/Rust measurement), pending a security review and
  fault fixtures; XChaCha is dropped because no platform standard library ships it. The TS write side
  already awaits an async page codec (§8.1). No dependency is selected and no implementation is
  scheduled. Decided: a reserved-bytes region with
  guarantees in terms of usable capacity, all page types authenticated, ephemeral-key scratch
  files, and a dedicated CSPRNG for key material (§1.1). Open: page-level replay and manifest
  identity bytes (§4). The only present storage requirement is non-foreclosure — already satisfied.
- **Replication.** ✅ **Architecture decided (block-shipping, no WAL), not built** — designed in
  [replication.md](spec/design/replication.md). Ship the **per-commit page-delta** in `txid`
  order, as a tee at the block seam (below the encryption codec → keyless backup replicas). No
  WAL: COW + the root swap already give atomicity *and* lock-free concurrency. Trade:
  write-amplification. A **logical** changeset stream is a separate higher-layer door, not
  foreclosed, not scheduled.
- **Page service (ratified 2026-10-16; phases 1–4 and 6 landed).** One owner process alone touches the
  database file and serves pages and whole-commit deltas to client processes, each running its own
  engine, over a local socket or SSH. It replaced shared multi-process mode at phase 4 (`locking =
  auto`) and carries remote access, backup, and replication. → [page-service.md](spec/design/page-service.md) §16
  - [x] **Phase 1 — spec.** ✅ **landed**: `spec/pageservice/` holds the message catalog and framing
    as shared data, field encodings, and byte fixtures checked by a Ruby verifier under `mise run
    verify`; `08006`, `08007`, `08P01`, `25P03` registered; a hand-written codec per core (Rust, Go,
    TS) tested against the fixtures.
  - [x] **Phase 2 — read path.** ✅ **landed** (test-only `locking = service`): Rust, Go, TS
    election by `presence EX`, the owner record, the same-user endpoint,
    `hello`/`begin_read`/`read_pages`/`end_read`, incarnation ids and client cache invalidation,
    read-only connections, the TS worker-thread client and service (one I/O worker per process).
    Proven by the `service_*` process-corpus scenarios in every pairing and by `conformance:service`
    (the SQL corpus with every query answered by a page client of another core's owner, part of
    `ci`); `bench:page_service` measures the alone-owner fast path and warm/cold client reads
    (benchmarks.md §12). Known gaps for later phases: a plain `BEGIN` on a client takes no owner pin;
    the TS owner bounds distinct pinned versions at 4096 (`54000`).
  - [x] **Phase 3 — write path.** ✅ **landed** (test-only `locking = service`): Rust, Go, TS
    page clients commit through the owner. The owner's writer gate is shared by its own writers and
    client `begin_write` (waited on without stalling the connection's reads; `55P03` at the session
    lock timeout); the reuse grant; the `CommitDelta` stream (`commit_begin`/`commit_pages`/
    `commit_end`) with owner validation (`08P01`) and the point of no return; structural owner
    publication with lazy catalog adoption at owner transaction begins; owner-only free-list rebuilds
    by maintenance commits (the transactions.md §2.1 carve-out); the resolution window and
    `commit_status`, which a client uses to resolve a lost acknowledgement; the idle-in-transaction
    timeout (`25P03`); read-only connections only on request. Proven by the `service_*` scenarios
    in every pairing and by `conformance:service` now running every record, writes included, on a
    page client; `bench:page_service` adds `client_commit`.
  - [x] **File attachments under the service.** ✅ **landed** (Rust, Go, TS; spec/pageservice/
    README.md §6.6): a file attachment under `locking = auto` is elected owner (serving it) or page
    client of its file, through an inner handle with its own role, handoff, reader registry, and
    resolution window, taking main's ownership preference and owner limits; read begins pin it through
    that role; a writable transaction takes every writer gate, local or remote, in real-path byte
    order with `lock_timeout_ms` as the backstop, releasing in reverse on failure; attach/detach wait
    for no other process. No wire or format change. Proven by the `attachment_service`,
    `attachment_gate_order`, and `attachment_read_only` process scenarios in every pairing and by the
    `service-attachments` simulation profile (simulation-testing.md §14 S11).
  - [x] **Phase 4 — handoff.** ✅ **landed.** Promotion, ownership preference and yield, graceful close, the grace
    periods; the process-corpus rewrite; simulation testing of owner + clients; retire shared mode,
    the `shared` and test-only `service` values, and locking.md §4–§5; a new bundle marker.
    - [x] **Handoff** (test-only `locking = service`, Rust, Go, TS; spec/pageservice/README.md §6.5):
      re-election (promotion or reconnection) with transparent retry of unanswered begins and `08006`
      for transactions begun under the old role; lost-ack resolution through re-election;
      `ownership = prefer | yield | never`, yield with notices, drain, and the 2 s handoff pause; a
      read-only owner yields to a writer; graceful close; idle-session timeout; `max_connections`
      (`53300`). Proven by the `service_*` process scenarios in every pairing.
    - [x] **Shared mode retired**: `locking = auto | exclusive | none`, `auto` the page service (a
      non-serving owner where serving is unavailable), `exclusive` a non-serving owner, the
      `protocol-v2` bundle of `presence` and `arrival`, file attachments owner-only until the item
      above; the process corpus runs every scenario under it.
    - [x] **Simulation of an owner and its clients** (simulation-testing.md §14 S8, all three cores):
      `# processes: service` traces over a simulated lock host and network, settled after every step;
      disk faults kill the owner, `disconnect` steps and frame faults, the commit-role oracle, owner
      maintenance commits in the history; the `service` profile, a regression trace, and the
      `client-reclaims` plant.
  - [x] **Phase 5 — SSH.** ✅ **landed** (Rust and the CLI): the stream transport
    (`Database::open_remote` over a transport command's stdio; `Database::serve_page_stream`, run by
    `jed pages [--read-only] DBFILE`, relaying to the live owner or owning as a `yield` owner), the CLI
    opening `ssh://[user@]host[:port]/path` through the system `ssh`, and client-side read-ahead /
    subtree fetch over the unchanged `read_pages` for stream clients (spec/pageservice/README.md §5.3,
    page-service.md §5.3, cli.md §3). No wire change. Proven by `service_stream.process.toml` (a Rust
    stream client against owners of every core), Rust host-API tests (including a backup through the
    stream, byte-identical to a local one), and CLI e2e tests with a fake `ssh`; `bench:page_service`
    adds `stream_cold_scan` with and without read-ahead.
    - [ ] **Decide: configuring the remote binary and the ssh command** (maintainer). Today the CLI runs
      `ssh` from `PATH` and `jed` from the remote `PATH`, with no option for either (an environment
      variable, a flag, or a URL query are the candidates).
    - [ ] **Go / TS stream clients**, when a program needs remote access from them (page-service.md §14);
      a transport, not a protocol, change. Read-ahead would then be ported with them.
  - [x] **Phase 6 — backup.** ✅ **landed** (Rust, Go, TS). Page-identical and compacted forms — jed's
    answer to SQLite's `sqlite3_backup` / `VACUUM INTO`, locally as well as over SSH. A backup pins one
    committed snapshot like a reader (never the writer gate) and walks it with its own engine in every
    role: the page-identical copy keeps every reachable page and the free-list chain at its own index,
    zeroes the rest, and writes bootstrap meta; the compacted copy is the from-scratch image at
    `txid + 1`, byte-for-byte what compaction leaves. A page client reads the pages with batched
    `read_pages`, so the owner needs no new message (`snapshot_stream` stays reserved; page-service.md
    §9 records why the client walks). Proven by `backup*.process.toml` in every pairing (byte-identical
    backups across cores, the independent `verify.rb --check-image` decode, a compacted backup equal
    to the compacted file) and the simulation's `backup` step (service, compaction, relaxed, and service-attachments profiles).
    - [ ] _follow-on:_ **name the host API and the CLI command.** Today each core has only a test-only
      entry point (`BackupForTesting` / `backup_for_testing` / `backupForTesting`); the public shape (a
      destination path and a form, attachments, progress) is a product decision, and cli.md has no
      backup command yet.
    - [ ] _follow-on:_ back up a file attachment, and to an OPFS destination (main only today).
  - [ ] **Phase 7 — replication.** The tee, retained log, `subscribe`, lineage and timelines (a format
    change), replicas and read replicas; amend replication.md.

## Resource admission follow-through

- [x] **JSON nesting-depth limit.** A fixed `MAX_JSON_DEPTH = 256` bounds every `json`/`jsonb`
  document where it is produced: the text parser and every nesting constructor (builders,
  aggregates, `to_json[b]`/`array_to_json`, `||`, `jsonb_set`/`jsonb_insert`,
  `jsonb_path_query_array`, the `JSON_QUERY` wrapper) raise `54001`, and the on-disk `jsonb`
  decoder raises `XX001`. The jsonpath compiler bounds its own nesting at the same 256,
  `JSON_TABLE NESTED PATH` counts toward `MAX_EXPR_DEPTH`, and `array_in` stops at the 7th brace.
  This closes the native-stack crash from untrusted deeply nested JSON. See
  [json.md §6.4](spec/design/json.md) and `resource/json_depth_limit.test`.
  - [ ] _follow-on:_ when a storable `jsonpath` column lands, its decoder must re-run the compiler's
    nesting gate (`XX001` on a crafted body). (`.**` walks an explicit stack, so its descent is
    bounded by document depth.)
- [x] **Q2 — operator state.** Hash-join, group, DISTINCT, set-operation and recursive-CTE
  tables (`ENTRY` 32 + key), retained aggregate inputs, window partitions, sort state,
  columnar lanes, and spill structures' resident elements; spill residency is measured on
  the same schedule so every core spills at the same row and releases on spill. Thresholds
  pinned by `resource/query_memory_state.test`; `mise run conformance:query_memory` compares
  every record's peak across cores in memory, disk, and forced-spill modes. The transient owners
  are charged too: collation sort-key decorations, window operand caches, and merge-heap heads
  (`resource/query_memory_transient.test`). A spill-capable structure spills when the account
  rejects it. See [memory.md](spec/design/memory.md) §6.
- [x] **Q3 — pending writes.** Every record version a transaction stages into a table or index
  reserves its stored bytes (on-disk record + out-of-line payload) against
  `max_query_memory_bytes` until commit/rollback, net of the versions it later replaced or deleted;
  each statement's account opens holding them, takes on its own staged bytes after each write phase,
  and they must fit after every statement. Thresholds pinned by
  `resource/query_memory_pending.test`. See [memory.md](spec/design/memory.md) §7.
- [x] **Q4a — committed in-memory storage.** A database-owned `max_storage_bytes` (create/attach
  option + handle setter, unlimited by default; the file form, Q4c, has landed — memory.md §8.7) over
  `page_count × page_size`, checked at commit before any write; a commit that raises the
  high-water past the limit fails `54P06` after one forced compaction, with a repair exemption for
  pure deletes and drops and a multi-root precheck. The compaction trigger is shared data
  (`[reclamation]` in `spec/cost/schedule.toml`); the `storage_bytes` gauge and a per-record
  cross-core storage comparison in `mise run conformance:query_memory` back it. Thresholds pinned by
  `resource/storage_memory.test` and `concurrency/storage_watermark.test`. Page caches stay
  evict-only under `cache_bytes`. See [memory.md](spec/design/memory.md) §8.
  - [ ] _follow-on:_ bounding the interior skeleton and resident GiST R-tree once they page.
